// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! NEW-1.5 — process-wide GC quiescence flag for active JIT frames.
//!
//! See [`crate::vm_heap::VmHeap::is_object_address`] and the upper-layer
//! `vm/src/jit/conservative_roots.rs` module for the full design.
//!
//! Why this lives in the GC crate (not the VM crate): the GC needs to
//! consult the flag at the start of every collection cycle to decide
//! whether compaction is safe. If the flag lived in the VM crate (which
//! depends on GC), the GC could not call into it without a circular
//! dependency. So the *flag itself* is owned by the GC crate; the VM
//! crate's `JitEntryGuard` increments and decrements it through the
//! [`enter`] and [`leave`] entry points.
//!
//! Semantics:
//! - `enter()` increments the global counter (AcqRel ordering — SECURITY
//!   FIX (V8) — so the increment is both released to and acquired against
//!   GC threads on other cores that observe the change via `is_active()`).
//! - `leave()` decrements.
//! - `is_active()` returns `true` whenever any thread anywhere in the
//!   process has at least one outstanding `enter()` without a matching
//!   `leave()`. The GC reads this at safepoint entry and, when set,
//!   defers compaction (it may still mark, but it does not relocate any
//!   object — see `gen_heap::collect_garbage_inner`).

use std::sync::atomic::{AtomicUsize, Ordering};

static JIT_ACTIVE_DEPTH: AtomicUsize = AtomicUsize::new(0);

/// DBG: total enter()/leave() calls — an imbalance means a leaked JIT entry
/// that keeps the non-moving sweep wedged on after JIT calls have returned.
pub static ENTER_COUNT: AtomicUsize = AtomicUsize::new(0);
pub static LEAVE_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Increment the global JIT-active counter. Called from the VM crate's
/// `JitEntryGuard::enter` immediately before transferring control to JIT
/// code. Returns the new depth (1-based).
///
/// SECURITY FIX (V8): use `AcqRel` rather than a bare `Release`.
///
/// The previous `Release`-only RMW had no acquire half, so this store was
/// not ordered against prior loads on the entering thread and — more
/// importantly — the ordering intent against the collector's
/// `is_active()` (`Acquire` load) was not self-contained. The
/// happens-before edge that makes the divert-to-non-moving path
/// (`gen_heap::collect_garbage_inner`, the `is_active()` check before any
/// relocation) sound is:
///
///   enter() [AcqRel RMW, makes the incremented depth visible] ──hb──▶
///       the JIT thread reaches the STW safepoint poll ──hb──▶
///       collector observes all threads parked, then loads is_active()
///       [Acquire] ──▶ sees depth > 0 ──▶ runs the NON-MOVING sweep
///       (never relocates objects out from under the JIT thread's raw
///        heap pointers held in registers/spill slots).
///
/// The STW safepoint barrier supplies the synchronization between the
/// JIT thread and the collector; the `AcqRel` here guarantees that once a
/// thread has incremented the counter, no subsequent collector
/// `is_active()` load can be reordered to observe the pre-increment value
/// (which would let the mover relocate live JIT-referenced objects =>
/// use-after-free). `AcqRel` does not weaken the existing release
/// visibility — it only adds the missing acquire half.
pub fn enter() -> usize {
    ENTER_COUNT.fetch_add(1, Ordering::Relaxed);
    JIT_ACTIVE_DEPTH.fetch_add(1, Ordering::AcqRel) + 1
}

/// Decrement the global JIT-active counter. Called from the VM crate's
/// `JitEntryGuard::drop`. Returns the new depth (post-decrement). It is a
/// debug-assert error to call this when the counter is already 0; the
/// release version saturates at 0 so a stray pop never wraps the counter.
pub fn leave() -> usize {
    LEAVE_COUNT.fetch_add(1, Ordering::Relaxed);
    let prev = JIT_ACTIVE_DEPTH.fetch_update(Ordering::Release, Ordering::Acquire, |d| {
        Some(d.saturating_sub(1))
    });
    match prev {
        Ok(p) => p.saturating_sub(1),
        Err(_) => 0,
    }
}

/// Returns true if any thread is currently inside a JIT call.
#[inline]
pub fn is_active() -> bool {
    JIT_ACTIVE_DEPTH.load(Ordering::Acquire) > 0
}

/// Current depth (mostly useful for tests and JFR diagnostics).
#[inline]
pub fn depth() -> usize {
    JIT_ACTIVE_DEPTH.load(Ordering::Acquire)
}

// ---------------------------------------------------------------------------
// Stage B (precise oop maps, B-K fix) — movable precise-JIT roots
// ---------------------------------------------------------------------------
//
// A conservatively-discovered JIT root MUST be pinned: a stack qword that
// merely looks like a heap pointer might be an `i64`, so the collector cannot
// rewrite it after a move — the object it points at must stay put. That pinning
// is exactly what wedges the young generation under heavy JIT (bt18 @ small
// heap: over-pinning blocks the drain) AND, when a pinned-but-relocated object
// slips through, leaves a stale slot (the B-K under-count).
//
// When a JIT frame is FULLY precisely covered (`CompiledMethod::fully_oop_
// covered`), its live oops live in a precise, *rewritable* oop map. The VM's
// `remap_active_jit_frames` rewrites those frame slots after a move and the
// JIT's post-safepoint reload refreshes the registers, so such an oop may be
// marked-but-NOT-pinned: selective promotion can evacuate it (draining young)
// and the slot is fixed up afterwards. The marking walk publishes each such
// young address here; `sweep_young_non_moving` consults it to EXCLUDE those
// addresses from the pin set.
//
// Per-thread because the JIT entry chain and the collection both run on the
// triggering thread. Cleared at the start of each root-gathering pass so it
// reflects only the CURRENT stack. A missed publication is always SAFE (the
// address simply stays pinned, the legacy behaviour); only a *stale* extra
// entry could be unsafe, which the per-pass clear prevents.

thread_local! {
    static MOVABLE_JIT_ROOTS: std::cell::RefCell<std::collections::HashSet<usize>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

/// Clear the movable-precise-JIT-root set. Called by the VM's root gatherer at
/// the start of every collection, before the JIT-frame scan republishes.
pub fn clear_movable_jit_roots() {
    MOVABLE_JIT_ROOTS.with(|s| s.borrow_mut().clear());
}

/// Record `addr` (an object address held in a precisely-covered, rewritable JIT
/// frame slot) as movable — i.e. it may be evacuated rather than pinned.
pub fn add_movable_jit_root(addr: usize) {
    MOVABLE_JIT_ROOTS.with(|s| {
        s.borrow_mut().insert(addr);
    });
}

/// True if `addr` was published as a movable precise JIT root this cycle.
/// `sweep_young_non_moving` calls this to exclude the address from the pin set.
#[inline]
pub fn is_movable_jit_root(addr: usize) -> bool {
    MOVABLE_JIT_ROOTS.with(|s| s.borrow().contains(&addr))
}

/// Count of movable roots published this cycle (diagnostics).
pub fn movable_jit_root_count() -> usize {
    MOVABLE_JIT_ROOTS.with(|s| s.borrow().len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_leave_round_trip() {
        let d0 = depth();
        let d1 = enter();
        assert_eq!(d1, d0 + 1);
        let d2 = leave();
        assert_eq!(d2, d0);
    }

    #[test]
    fn is_active_reflects_depth() {
        let d0 = depth();
        if d0 == 0 {
            assert!(!is_active());
        }
        let _ = enter();
        assert!(is_active());
        let _ = leave();
    }

    #[test]
    fn leave_saturates_at_zero() {
        // Force the counter to a known state, then over-leave.
        // We can't reset reliably from a test (other tests may share the
        // counter via parallel execution), so we just confirm the monotone
        // invariant: after one extra leave, depth never goes below 0.
        let _ = leave();
        let _ = leave();
        // Cannot assert == 0 because other concurrent tests may have
        // pushed; only assert the counter is well-formed (no UB / wrap).
        let d = depth();
        assert!(d <= usize::MAX / 2);
    }
}
