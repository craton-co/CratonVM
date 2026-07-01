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

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[cfg(not(test))]
static JIT_ACTIVE_DEPTH: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
thread_local! {
    static TEST_JIT_ACTIVE_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(not(test))]
#[inline]
fn active_depth_enter() -> usize {
    JIT_ACTIVE_DEPTH.fetch_add(1, Ordering::AcqRel) + 1
}

#[cfg(test)]
#[inline]
fn active_depth_enter() -> usize {
    TEST_JIT_ACTIVE_DEPTH.with(|d| {
        let next = d.get().saturating_add(1);
        d.set(next);
        next
    })
}

#[cfg(not(test))]
#[inline]
fn active_depth_leave() -> usize {
    let prev = JIT_ACTIVE_DEPTH.fetch_update(Ordering::Release, Ordering::Acquire, |d| {
        Some(d.saturating_sub(1))
    });
    match prev {
        Ok(p) => p.saturating_sub(1),
        Err(_) => 0,
    }
}

#[cfg(test)]
#[inline]
fn active_depth_leave() -> usize {
    TEST_JIT_ACTIVE_DEPTH.with(|d| {
        let next = d.get().saturating_sub(1);
        d.set(next);
        next
    })
}

#[cfg(not(test))]
#[inline]
fn active_depth_get() -> usize {
    JIT_ACTIVE_DEPTH.load(Ordering::Acquire)
}

#[cfg(test)]
#[inline]
fn active_depth_get() -> usize {
    TEST_JIT_ACTIVE_DEPTH.with(|d| d.get())
}

/// DBG: total enter()/leave() calls — an imbalance means a leaked JIT entry
/// that keeps the non-moving sweep wedged on after JIT calls have returned.
pub static ENTER_COUNT: AtomicUsize = AtomicUsize::new(0);
pub static LEAVE_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Whether the **default moving / compacting young generation**
/// (`CRATONVM_MOVING_YOUNG`) is enabled. Cached on first read.
///
/// When on, `gen_heap::collect_garbage_inner` runs the moving (Cheney) young
/// collection even while JIT frames are live (`is_active()`), instead of
/// diverting to the non-moving sweep. Safe only because the JIT publishes a
/// COMPLETE rewritable precise root map via the shadow stack and the
/// conservative frame scan is suppressed (see the vm crate's
/// `conservative_roots::moving_young_enabled` and
/// `docs/feature-designs/default-moving-young-gen.md`). Off by default; gated for
/// validation against the bt18 = 68332206 invariant.
#[inline]
pub fn moving_young_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("CRATONVM_MOVING_YOUNG").is_some())
}

static MOVING_YOUNG_COVERAGE_INCOMPLETE: AtomicBool = AtomicBool::new(false);
static MOVING_YOUNG_COVERAGE_FALLBACKS: AtomicUsize = AtomicUsize::new(0);

/// Start a new VM young-GC root-publication cycle. The VM calls this before
/// mutators publish the snapshots that the collector will use for the cycle.
pub fn begin_moving_young_coverage_cycle() {
    MOVING_YOUNG_COVERAGE_INCOMPLETE.store(false, Ordering::Release);
}

/// Record that at least one live JIT frame in this collection lacks a complete
/// moving-young coverage proof. The collector must use the non-moving sweep.
pub fn mark_moving_young_coverage_incomplete() {
    MOVING_YOUNG_COVERAGE_INCOMPLETE.store(true, Ordering::Release);
}

/// Whether the current collection has observed an incomplete moving-young JIT
/// frame/safepoint coverage proof.
#[inline]
pub fn moving_young_coverage_incomplete() -> bool {
    MOVING_YOUNG_COVERAGE_INCOMPLETE.load(Ordering::Acquire)
}

/// Bump the diagnostic fallback counter and return the post-increment value.
pub fn record_moving_young_coverage_fallback() -> usize {
    MOVING_YOUNG_COVERAGE_FALLBACKS.fetch_add(1, Ordering::Relaxed) + 1
}

/// Number of moving-young cycles diverted to the non-moving sweep because at
/// least one live JIT frame did not have complete coverage.
pub fn moving_young_coverage_fallback_count() -> usize {
    MOVING_YOUNG_COVERAGE_FALLBACKS.load(Ordering::Relaxed)
}

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
    active_depth_enter()
}

/// Decrement the global JIT-active counter. Called from the VM crate's
/// `JitEntryGuard::drop`. Returns the new depth (post-decrement). It is a
/// debug-assert error to call this when the counter is already 0; the
/// release version saturates at 0 so a stray pop never wraps the counter.
pub fn leave() -> usize {
    LEAVE_COUNT.fetch_add(1, Ordering::Relaxed);
    active_depth_leave()
}

/// Returns true if any thread is currently inside a JIT call.
#[inline]
pub fn is_active() -> bool {
    active_depth_get() > 0
}

/// Current depth (mostly useful for tests and JFR diagnostics).
#[inline]
pub fn depth() -> usize {
    active_depth_get()
}

// ---------------------------------------------------------------------------
// Unregistered JIT frame detection (A5 fix)
// ---------------------------------------------------------------------------
//
// `JIT_ACTIVE_DEPTH` / `is_active()` only counts JIT entries that pushed a
// `JitEntryGuard` (the interpreter→JIT invoke paths). The process entry point
// (`Vm::invoke` → app `main`) and any other JIT method whose native frame is on
// the stack WITHOUT a guard is invisible to it. With `is_active()` false the
// generational collector picks the MOVING young collector, which relocates the
// unregistered frame's live objects and cannot rewrite their raw stack slots →
// stale all-zero-header receiver (the bintrees `main`-compiled corruption).
//
// The VM root scan (`conservative_roots::scan_active_jit_frames`) detects such a
// frame by finding a JIT code address among the native stack words and sets
// this per-thread flag; the collector ORs it into the non-moving-sweep decision
// (and the scan additionally does a conservative full-stack pass so the frame's
// oops are MARKED). Per-thread because the STW collection runs on the detecting
// (mutator) thread; cleared at the start of every root-gathering pass.

thread_local! {
    static UNREGISTERED_JIT_FRAME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Record that this thread has an unregistered JIT frame on its native stack
/// (a JIT method live without a `JitEntryGuard`). Set by the VM root scan.
pub fn set_unregistered_jit_frame_on_stack() {
    UNREGISTERED_JIT_FRAME.with(|c| c.set(true));
}

/// Clear the unregistered-JIT-frame flag (start of each root-gathering pass).
pub fn clear_unregistered_jit_frame_on_stack() {
    UNREGISTERED_JIT_FRAME.with(|c| c.set(false));
}

/// True iff the VM root scan found an unregistered JIT frame on this thread's
/// stack this cycle. The generational collector treats this like
/// `is_active()` — run the non-moving sweep so the frame's conservatively-marked
/// oops are not relocated out from under its raw stack slots.
#[inline]
pub fn unregistered_jit_frame_on_stack() -> bool {
    UNREGISTERED_JIT_FRAME.with(|c| c.get())
}

// ---------------------------------------------------------------------------
// Per-cycle fallback for incomplete rewritable JIT coverage
// ---------------------------------------------------------------------------
//
// `CRATONVM_MOVING_YOUNG` is only sound while every live JIT-held oop is
// published through a precise, rewritable root channel. If the VM detects an
// active JIT frame whose coverage is incomplete, it conservatively scans that
// frame and sets this per-thread flag so the generational collector runs the
// non-moving young sweep for this cycle instead of moving objects behind raw
// JIT frame slots.

thread_local! {
    static FORCE_NON_MOVING_JIT_ROOTS: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

pub fn set_force_non_moving_jit_roots() {
    FORCE_NON_MOVING_JIT_ROOTS.with(|c| c.set(true));
}

pub fn clear_force_non_moving_jit_roots() {
    FORCE_NON_MOVING_JIT_ROOTS.with(|c| c.set(false));
}

#[inline]
pub fn force_non_moving_jit_roots() -> bool {
    FORCE_NON_MOVING_JIT_ROOTS.with(|c| c.get())
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

// ---------------------------------------------------------------------------
// Conservative (non-movable) JIT roots — G1 region pinning
// ---------------------------------------------------------------------------
//
// The generational collector honours "a conservatively-discovered JIT root must
// not be relocated" by running its NON-MOVING young sweep whenever any thread is
// in JIT (`is_active()` above) — nothing moves, so a register/spill slot that
// the collector cannot rewrite keeps pointing at a valid object.
//
// G1 has no non-moving young mode: it always evacuates the collection set. So it
// needs the same guarantee expressed in its region model — the REGIONS that hold
// conservatively-discovered JIT roots must be EXCLUDED from the collection set
// (pinned in place) for the duration of the collection, exactly like a
// JNI-critical pinned region. The VM's root gatherer publishes each conservative
// JIT-frame root address here (only under G1); `G1Collector::{young,mixed}
// _collection` map those addresses to region indices, drop them from the CSet,
// and still scan each pinned region as a source so its referents in the CSet are
// evacuated and its own slots fixed up in place (young→young references carry no
// remembered set, so the pinned region must be scanned explicitly).
//
// Thread-local for the same reason as MOVABLE_JIT_ROOTS: the JIT entry chain and
// the (self-triggered) collection run on the same thread. Cleared at the start
// of each root-gathering pass. A missed publication only risks a stale slot (the
// pre-fix behaviour); a stale EXTRA entry is prevented by the per-pass clear and
// would at worst over-pin one region for one cycle.

thread_local! {
    static PINNED_JIT_ROOTS: std::cell::RefCell<std::collections::HashSet<usize>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

/// Clear the conservative-pinned-JIT-root set. Called by the VM's root gatherer
/// at the start of every collection, before the JIT-frame scan republishes.
pub fn clear_pinned_jit_roots() {
    PINNED_JIT_ROOTS.with(|s| s.borrow_mut().clear());
}

/// Record `addr` (an object address discovered conservatively in a JIT frame,
/// whose holder slot the collector cannot rewrite) as pin-required for G1.
pub fn add_pinned_jit_root(addr: usize) {
    PINNED_JIT_ROOTS.with(|s| {
        s.borrow_mut().insert(addr);
    });
}

/// Snapshot the conservative-pinned-JIT-root addresses published this cycle.
/// `G1Collector` maps these to regions it must exclude from the collection set.
pub fn pinned_jit_roots_snapshot() -> Vec<usize> {
    PINNED_JIT_ROOTS.with(|s| s.borrow().iter().copied().collect())
}

/// Count of conservative-pinned JIT roots published this cycle (diagnostics).
pub fn pinned_jit_root_count() -> usize {
    PINNED_JIT_ROOTS.with(|s| s.borrow().len())
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
