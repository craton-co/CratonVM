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

use crate::gc_flags;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// How deep any thread is inside a JIT call, process-wide.
///
/// Striped per thread: this is written on every interpreter/JIT boundary
/// crossing and read only by the collector. As one shared `AtomicUsize` — with
/// a `fetch_update` CAS *loop* on the leave side — it was one of the
/// contended cache lines that stopped compiled code scaling past a couple of
/// threads. See [`cratonvm_types::striped_counter`] for why summing stripes
/// answers `is_active()` exactly as the single counter did.
#[cfg(not(test))]
static JIT_ACTIVE_DEPTH: cratonvm_types::striped_counter::StripedCounter =
    cratonvm_types::striped_counter::StripedCounter::new();

#[cfg(test)]
thread_local! {
    static TEST_JIT_ACTIVE_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(not(test))]
#[inline]
fn active_depth_enter() -> usize {
    JIT_ACTIVE_DEPTH.inc();
    // No caller uses the returned depth on the hot path, and summing every
    // stripe to produce it would reintroduce exactly the cross-thread traffic
    // the striping removes. Report this thread's own contribution instead.
    1
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
    // Saturating per stripe, so a stray unbalanced leave can no longer cancel
    // a *different* thread's live entry the way the single counter allowed.
    JIT_ACTIVE_DEPTH.dec();
    0
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
    JIT_ACTIVE_DEPTH.get()
}

/// `active_depth_get() > 0` without summing every stripe.
#[cfg(not(test))]
#[inline]
fn active_depth_nonzero() -> bool {
    !JIT_ACTIVE_DEPTH.is_zero()
}

#[cfg(test)]
#[inline]
fn active_depth_nonzero() -> bool {
    active_depth_get() > 0
}

#[cfg(test)]
#[inline]
fn active_depth_get() -> usize {
    TEST_JIT_ACTIVE_DEPTH.with(|d| d.get())
}

/// DBG: total enter()/leave() calls — an imbalance means a leaked JIT entry
/// that keeps the non-moving sweep wedged on after JIT calls have returned.
pub static ENTER_COUNT: cratonvm_types::striped_counter::StripedCounter =
    cratonvm_types::striped_counter::StripedCounter::new();
pub static LEAVE_COUNT: cratonvm_types::striped_counter::StripedCounter =
    cratonvm_types::striped_counter::StripedCounter::new();

// ---------------------------------------------------------------------------
// Moving / compacting young generation — the ONE gate
// ---------------------------------------------------------------------------
//
// WHY THIS IS A PUBLISHED VALUE AND NOT JUST `gc_flags().moving_young`
// (arch-2026-07-26 `moving-young-precise-roots`):
//
// The moving-young switch is consumed by three layers that cannot see each
// other, and only ONE of them can physically make relocation safe:
//
//   * `cratonvm_jit::x64::moving_young_enabled()` — CODEGEN. Decides whether
//     the shadow-stack push/reload sequences (the rewritable precise root map)
//     are emitted at all, and whether `OopMapEntry::moving_young_coverage_
//     complete` can ever be true. It still parses `CRATONVM_MOVING_YOUNG` from
//     the environment ITSELF rather than reading `flags().gc.moving_young`.
//   * `cratonvm_vm::jit::conservative_roots::moving_young_enabled()` — ROOT
//     GATHERING. Decides whether the conservative JIT-frame scan is suppressed.
//   * this function — COLLECTOR. Decides whether `gen_heap` relocates.
//
// Any skew where the COLLECTOR says "moving" while the CODEGEN says "no shadow
// map" relocates objects whose only home is a JIT register/frame slot that
// nothing will ever rewrite. That is not a risk of corruption, it is
// corruption. Because the codegen still reads the raw variable, a default flip
// in `cratonvm_types::flags` alone WOULD produce exactly that skew.
//
// Model: the codegen side is authoritative, and the VM PUBLISHES its answer
// here via [`publish_moving_young_enabled`] — `collect_roots` is on the path of
// every collection, so the collector can never decide to relocate against a
// gate the codegen disagrees with. Before the first publish (a `cratonvm-gc`
// process with no JIT at all: unit tests, embedders) this falls back to
// `gc_flags().moving_young`, where there are no JIT frames and moving is
// unconditionally safe.
//
// The interlock is deliberately kept even after the codegen migrates to
// `flags()`: it is what makes the skew unrepresentable rather than merely
// unlikely, and it costs one relaxed load per collection.

const MOVING_YOUNG_UNPUBLISHED: u8 = 0;
const MOVING_YOUNG_OFF: u8 = 1;
const MOVING_YOUNG_ON: u8 = 2;

#[cfg(not(test))]
static MOVING_YOUNG_STATE: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(MOVING_YOUNG_UNPUBLISHED);

// Per-test isolation, mirroring `TEST_JIT_ACTIVE_DEPTH` above: the gc unit
// tests run in parallel threads of one process, so a process-global gate would
// make "publish on, collect, publish off" tests race each other.
#[cfg(test)]
thread_local! {
    static MOVING_YOUNG_STATE: std::cell::Cell<u8> =
        const { std::cell::Cell::new(MOVING_YOUNG_UNPUBLISHED) };
}

#[cfg(not(test))]
#[inline]
fn moving_young_state_get() -> u8 {
    MOVING_YOUNG_STATE.load(Ordering::Acquire)
}

#[cfg(not(test))]
#[inline]
fn moving_young_state_set(v: u8) {
    MOVING_YOUNG_STATE.store(v, Ordering::Release);
}

#[cfg(test)]
#[inline]
fn moving_young_state_get() -> u8 {
    MOVING_YOUNG_STATE.with(std::cell::Cell::get)
}

#[cfg(test)]
#[inline]
fn moving_young_state_set(v: u8) {
    MOVING_YOUNG_STATE.with(|c| c.set(v));
}

/// Publish the authoritative (codegen-side) moving-young decision.
///
/// Called by the VM's root gatherer, which is on the path of every collection.
/// Idempotent; a *changed* value is a bug (the codegen gate is a `OnceLock`)
/// and is reported loudly rather than silently accepted.
pub fn publish_moving_young_enabled(on: bool) {
    let next = if on {
        MOVING_YOUNG_ON
    } else {
        MOVING_YOUNG_OFF
    };
    let prev = moving_young_state_get();
    if prev != MOVING_YOUNG_UNPUBLISHED && prev != next {
        tracing::warn!(
            "[moving-young] gate skew: collector previously observed on={}, codegen reports \
             on={} — the collector now follows the codegen. This must never happen; it means \
             a relocation decision was taken against a stale gate.",
            prev == MOVING_YOUNG_ON,
            on,
        );
    }
    moving_young_state_set(next);
}

/// Whether the **moving / compacting young generation** is in effect.
///
/// When on, `gen_heap::collect_garbage_inner` runs the moving (Cheney) young
/// collection even while JIT frames are live (`is_active()`), instead of
/// diverting to the non-moving sweep. Sound only because the JIT publishes a
/// COMPLETE rewritable precise root map via the shadow stack and the
/// conservative frame scan is suppressed (see the vm crate's
/// `conservative_roots::moving_young_enabled` and
/// `moving-young-precise-roots.md`).
///
/// Reads what the VM published from the codegen gate; before the first publish
/// it falls back to `gc_flags().moving_young`. Never an independent policy
/// decision — see the module comment above.
#[inline]
pub fn moving_young_enabled() -> bool {
    match moving_young_state_get() {
        MOVING_YOUNG_ON => true,
        MOVING_YOUNG_OFF => false,
        _ => gc_flags().moving_young,
    }
}

// The per-cycle coverage verdict is process-global in production: it is
// written by whichever thread proves an obligation unprovable (including the
// STW initiator scanning FROZEN peers) and read by the collector, which may be
// a different thread. Under `cfg(test)` it is thread-local for the same reason
// `TEST_JIT_ACTIVE_DEPTH` is: gc unit tests run in parallel threads of one
// process, and a global verdict would let one test's deliberate "incomplete"
// divert another test's deliberate "complete" collection.
#[cfg(not(test))]
static MOVING_YOUNG_COVERAGE_INCOMPLETE: AtomicBool = AtomicBool::new(false);
#[cfg(not(test))]
static MOVING_YOUNG_INCOMPLETE_REASON: AtomicUsize = AtomicUsize::new(0);

// EVERY reason this cycle recorded, as a bitmask over `incomplete_reason`
// codes, alongside the first-wins scalar above.
//
// Diagnostic only — nothing reads it to decide anything. It exists because the
// scalar is first-wins, which is right for "what forced this verdict" and wrong
// for "would repairing reason X have helped": a cycle reported as
// `active-safepoint-map-incomplete` may ALSO have hit
// `innermost-rbp-belongs-to-unguarded-callee`, and a cycle reported as the
// latter may have hit nothing else at all. Only that second kind turns into a
// moving collection if the innermost-rbp resolution is repaired, so sizing that
// repair needs the whole set, not its first element. Printed by
// `CRATONVM_DBG_GC_FALLBACK_REASONS=1`.
#[cfg(not(test))]
static MOVING_YOUNG_INCOMPLETE_REASON_MASK: AtomicUsize = AtomicUsize::new(0);

// A STRICTLY NARROWER per-cycle verdict than the one above: this cycle scanned
// state belonging to a peer thread that will never apply the collection's
// pointer map to itself (an OS-suspended in-JIT peer, or a blocked peer's JIT
// helper window). See `unrewritable_peer_state` for why the two must not be
// conflated. Same production-global / `cfg(test)`-thread-local split, and for
// the same reason.
#[cfg(not(test))]
static UNREWRITABLE_PEER_STATE: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    static MOVING_YOUNG_COVERAGE_INCOMPLETE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    static MOVING_YOUNG_INCOMPLETE_REASON: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
    static MOVING_YOUNG_INCOMPLETE_REASON_MASK: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
    static UNREWRITABLE_PEER_STATE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(not(test))]
#[inline]
fn coverage_incomplete_get() -> bool {
    MOVING_YOUNG_COVERAGE_INCOMPLETE.load(Ordering::Acquire)
}

#[cfg(not(test))]
#[inline]
fn coverage_incomplete_set(v: bool) {
    MOVING_YOUNG_COVERAGE_INCOMPLETE.store(v, Ordering::Release);
}

#[cfg(not(test))]
#[inline]
fn incomplete_reason_get() -> usize {
    MOVING_YOUNG_INCOMPLETE_REASON.load(Ordering::Acquire)
}

#[cfg(not(test))]
#[inline]
fn incomplete_reason_set_if_unset(reason: usize) {
    let _ = MOVING_YOUNG_INCOMPLETE_REASON.compare_exchange(
        incomplete_reason::NONE,
        reason,
        Ordering::AcqRel,
        Ordering::Acquire,
    );
}

#[cfg(not(test))]
#[inline]
fn incomplete_reason_clear() {
    MOVING_YOUNG_INCOMPLETE_REASON.store(incomplete_reason::NONE, Ordering::Release);
    MOVING_YOUNG_INCOMPLETE_REASON_MASK.store(0, Ordering::Release);
}

#[cfg(not(test))]
#[inline]
fn incomplete_reason_mask_add(reason: usize) {
    if reason < incomplete_reason::COUNT {
        MOVING_YOUNG_INCOMPLETE_REASON_MASK.fetch_or(1usize << reason, Ordering::AcqRel);
    }
}

#[cfg(not(test))]
#[inline]
fn incomplete_reason_mask_get() -> usize {
    MOVING_YOUNG_INCOMPLETE_REASON_MASK.load(Ordering::Acquire)
}

#[cfg(not(test))]
#[inline]
fn unrewritable_peer_state_get() -> bool {
    UNREWRITABLE_PEER_STATE.load(Ordering::Acquire)
}

#[cfg(not(test))]
#[inline]
fn unrewritable_peer_state_set(v: bool) {
    UNREWRITABLE_PEER_STATE.store(v, Ordering::Release);
}

// ---- The cross-thread JIT coverage handshake ledger -----------------------
//
// `incomplete_reason::CROSS_THREAD_JIT_PEER` used to be an unconditional
// refusal: *any* peer inside compiled code made the cycle unprovable, because
// the initiator cannot walk a peer's `JIT_ENTRY_CHAIN` (it is a thread-local)
// and cannot rewrite a peer's registers. On a many-threaded workload that is
// nearly every cycle, which is how
// `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md` ends with an
// `OutOfMemoryError` on a heap that is 97 % free.
//
// The missing half was never the REWRITE. A peer that parks COOPERATIVELY at
// the STW barrier runs `apply_pointer_map_to_thread` on resume, and that is
// `remap_active_jit_frames` + `remap_register_image_words` +
// `shadow_stack.remap` over its OWN chain — precisely the rewrite the
// initiator cannot perform on its behalf. The missing half was the PROOF: the
// initiator had no way to learn that those frames were rewritable.
//
// So the peer proves it for itself, on its own thread, at its own park, and
// deposits the answer here. The ledger is a DEPTH rather than a thread count
// because `GLOBAL_JIT_DEPTH` — the only process-wide view of how much compiled
// code is live — is a depth too, and comparing like with like is what makes
// the accounting airtight: the initiator accepts a cycle only when the proven
// depth accounts for EVERY peer JIT entry in the process. Anything it cannot
// account for (an OS-frozen peer, a peer blocked in a native with compiled
// frames below it, a peer whose own proof failed) never lands here, and the
// shortfall refuses the cycle.
//
// Reset at the two points that OPEN a pause — `begin_moving_young_coverage_cycle`
// and the barrier's own `request_stw` — never at the end of one. A stale
// nonzero value is the only unsound state this ledger has, so it is cleared on
// the way in, by whichever of the two runs first, rather than trusted to a
// path that may not run at all.
#[cfg(not(test))]
static PEER_PROVEN_JIT_DEPTH: AtomicUsize = AtomicUsize::new(0);
#[cfg(not(test))]
static PEER_COVERAGE_ACCEPTED: AtomicUsize = AtomicUsize::new(0);
#[cfg(not(test))]
static PEER_COVERAGE_REFUSED: AtomicUsize = AtomicUsize::new(0);
#[cfg(not(test))]
static PEER_COVERAGE_DEPOSITS: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
thread_local! {
    static PEER_PROVEN_JIT_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PEER_COVERAGE_ACCEPTED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PEER_COVERAGE_REFUSED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PEER_COVERAGE_DEPOSITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(not(test))]
#[inline]
fn peer_proven_depth_reset_inner() {
    PEER_PROVEN_JIT_DEPTH.store(0, Ordering::Release);
}

#[cfg(not(test))]
#[inline]
fn peer_proven_depth_add_inner(n: usize) {
    PEER_PROVEN_JIT_DEPTH.fetch_add(n, Ordering::AcqRel);
    PEER_COVERAGE_DEPOSITS.fetch_add(1, Ordering::Relaxed);
}

#[cfg(not(test))]
#[inline]
fn peer_proven_depth_get_inner() -> usize {
    PEER_PROVEN_JIT_DEPTH.load(Ordering::Acquire)
}

#[cfg(not(test))]
#[inline]
fn peer_coverage_bump(accepted: bool) {
    if accepted {
        PEER_COVERAGE_ACCEPTED.fetch_add(1, Ordering::Relaxed);
    } else {
        PEER_COVERAGE_REFUSED.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(not(test))]
fn peer_coverage_counters_inner() -> (usize, usize, usize) {
    (
        PEER_COVERAGE_ACCEPTED.load(Ordering::Relaxed),
        PEER_COVERAGE_REFUSED.load(Ordering::Relaxed),
        PEER_COVERAGE_DEPOSITS.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
#[inline]
fn peer_proven_depth_reset_inner() {
    PEER_PROVEN_JIT_DEPTH.with(|c| c.set(0));
}

#[cfg(test)]
#[inline]
fn peer_proven_depth_add_inner(n: usize) {
    PEER_PROVEN_JIT_DEPTH.with(|c| c.set(c.get() + n));
    PEER_COVERAGE_DEPOSITS.with(|c| c.set(c.get() + 1));
}

#[cfg(test)]
#[inline]
fn peer_proven_depth_get_inner() -> usize {
    PEER_PROVEN_JIT_DEPTH.with(std::cell::Cell::get)
}

#[cfg(test)]
#[inline]
fn peer_coverage_bump(accepted: bool) {
    if accepted {
        PEER_COVERAGE_ACCEPTED.with(|c| c.set(c.get() + 1));
    } else {
        PEER_COVERAGE_REFUSED.with(|c| c.set(c.get() + 1));
    }
}

#[cfg(test)]
fn peer_coverage_counters_inner() -> (usize, usize, usize) {
    (
        PEER_COVERAGE_ACCEPTED.with(std::cell::Cell::get),
        PEER_COVERAGE_REFUSED.with(std::cell::Cell::get),
        PEER_COVERAGE_DEPOSITS.with(std::cell::Cell::get),
    )
}

/// Clear the cross-thread coverage ledger for a pause that is about to open.
///
/// Called from [`begin_moving_young_coverage_cycle`] and from the VM's
/// `GcBarrier::request_stw` — both run strictly BEFORE any peer can park and
/// deposit, and a pause that reaches only one of them is still cleared.
pub fn reset_peer_proven_jit_depth() {
    peer_proven_depth_reset_inner();
    // The helper-window pins describe peers frozen during THIS cycle only.
    clear_xt_cycle_pinned_jit_roots();
    clear_xt_cycle_pinned_jit_depth();
}

/// A cooperatively-parking peer deposits `depth` JIT entries it has just PROVEN
/// rewritable for this pause.
///
/// `depth` must be the depositing thread's own `JIT_ENTRY_CHAIN` length read
/// AFTER its per-thread coverage proof returned `true`: the proof prunes
/// returned entries, and a length read before it can be too LARGE — which is
/// the unsound direction here, since the initiator's test is a comparison
/// against the process-wide depth.
pub fn add_peer_proven_jit_depth(depth: usize) {
    if depth != 0 {
        peer_proven_depth_add_inner(depth);
    }
}

/// Total peer JIT depth proven rewritable for this pause.
pub fn peer_proven_jit_depth() -> usize {
    peer_proven_depth_get_inner()
}

/// Record whether a cycle's cross-thread obligation was discharged by the
/// handshake (`accepted`) or fell back to the blanket refusal.
pub fn note_peer_coverage_verdict(accepted: bool) {
    peer_coverage_bump(accepted);
}

/// `(cycles accepted, cycles refused, peer deposits)` for the handshake — the
/// engagement counter that has to sit beside any claim made about it.
pub fn peer_coverage_counters() -> (usize, usize, usize) {
    peer_coverage_counters_inner()
}

#[cfg(test)]
#[inline]
fn coverage_incomplete_get() -> bool {
    MOVING_YOUNG_COVERAGE_INCOMPLETE.with(std::cell::Cell::get)
}

#[cfg(test)]
#[inline]
fn coverage_incomplete_set(v: bool) {
    MOVING_YOUNG_COVERAGE_INCOMPLETE.with(|c| c.set(v));
}

#[cfg(test)]
#[inline]
fn incomplete_reason_get() -> usize {
    MOVING_YOUNG_INCOMPLETE_REASON.with(std::cell::Cell::get)
}

#[cfg(test)]
#[inline]
fn incomplete_reason_set_if_unset(reason: usize) {
    MOVING_YOUNG_INCOMPLETE_REASON.with(|c| {
        if c.get() == incomplete_reason::NONE {
            c.set(reason);
        }
    });
}

#[cfg(test)]
#[inline]
fn incomplete_reason_clear() {
    MOVING_YOUNG_INCOMPLETE_REASON.with(|c| c.set(incomplete_reason::NONE));
    MOVING_YOUNG_INCOMPLETE_REASON_MASK.with(|c| c.set(0));
}

#[cfg(test)]
#[inline]
fn incomplete_reason_mask_add(reason: usize) {
    if reason < incomplete_reason::COUNT {
        MOVING_YOUNG_INCOMPLETE_REASON_MASK.with(|c| c.set(c.get() | (1usize << reason)));
    }
}

#[cfg(test)]
#[inline]
fn incomplete_reason_mask_get() -> usize {
    MOVING_YOUNG_INCOMPLETE_REASON_MASK.with(std::cell::Cell::get)
}

#[cfg(test)]
#[inline]
fn unrewritable_peer_state_get() -> bool {
    UNREWRITABLE_PEER_STATE.with(std::cell::Cell::get)
}

#[cfg(test)]
#[inline]
fn unrewritable_peer_state_set(v: bool) {
    UNREWRITABLE_PEER_STATE.with(|c| c.set(v));
}

// Diagnostic counters. Thread-local under `cfg(test)` for the same reason as
// the verdict above: a unit test that asserts "this proven cycle recorded no
// fallback" must not be raced by a parallel test that deliberately provokes one.
#[cfg(not(test))]
static MOVING_YOUNG_COVERAGE_FALLBACKS: AtomicUsize = AtomicUsize::new(0);
#[cfg(not(test))]
static MOVING_YOUNG_CYCLES: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
thread_local! {
    static MOVING_YOUNG_COVERAGE_FALLBACKS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
    static MOVING_YOUNG_CYCLES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(not(test))]
#[inline]
fn bump_fallbacks() -> usize {
    MOVING_YOUNG_COVERAGE_FALLBACKS.fetch_add(1, Ordering::Relaxed) + 1
}

#[cfg(not(test))]
#[inline]
fn read_fallbacks() -> usize {
    MOVING_YOUNG_COVERAGE_FALLBACKS.load(Ordering::Relaxed)
}

#[cfg(not(test))]
#[inline]
fn bump_moving_cycles() -> usize {
    MOVING_YOUNG_CYCLES.fetch_add(1, Ordering::Relaxed) + 1
}

#[cfg(not(test))]
#[inline]
fn read_moving_cycles() -> usize {
    MOVING_YOUNG_CYCLES.load(Ordering::Relaxed)
}

#[cfg(test)]
#[inline]
fn bump_fallbacks() -> usize {
    MOVING_YOUNG_COVERAGE_FALLBACKS.with(|c| {
        let n = c.get() + 1;
        c.set(n);
        n
    })
}

#[cfg(test)]
#[inline]
fn read_fallbacks() -> usize {
    MOVING_YOUNG_COVERAGE_FALLBACKS.with(std::cell::Cell::get)
}

#[cfg(test)]
#[inline]
fn bump_moving_cycles() -> usize {
    MOVING_YOUNG_CYCLES.with(|c| {
        let n = c.get() + 1;
        c.set(n);
        n
    })
}

#[cfg(test)]
#[inline]
fn read_moving_cycles() -> usize {
    MOVING_YOUNG_CYCLES.with(std::cell::Cell::get)
}

/// Why a moving-young cycle could not prove complete rewritable JIT coverage.
///
/// Carried as a plain code (no allocation, no lock) so it can be set from the
/// STW root-scan hot path and read back by the collector for the warn-level
/// fallback diagnostic. The numbering is stable; append new variants.
pub mod incomplete_reason {
    /// No incompleteness recorded this cycle.
    pub const NONE: usize = 0;
    /// A registered JIT entry carried no precise oop-map metadata at all.
    pub const NO_PRECISE_MAP: usize = 1;
    /// A live JIT frame did not publish its exact RBP, so no map can be located.
    pub const MISSING_EXACT_RBP: usize = 2;
    /// The active safepoint's oop map is not `moving_young_coverage_complete`.
    pub const ACTIVE_FRAME_MAP: usize = 3;
    /// A parent (RBP-chain) frame's map is not complete.
    pub const PARENT_FRAME_MAP: usize = 4;
    /// A JIT frame is on the native stack without a `JitEntryGuard` (A5).
    pub const UNREGISTERED_JIT_FRAME: usize = 5;
    /// An active OSR artifact cannot prove rewritable shadow coverage.
    pub const OSR_SHADOW: usize = 6;
    /// Another thread holds live JIT frames whose coverage this thread's scan
    /// cannot verify and whose registers/stack are not rewritable.
    pub const CROSS_THREAD_JIT_PEER: usize = 7;
    /// A peer thread was OS-suspended in JIT code and scanned conservatively.
    pub const XT_TAKEOVER: usize = 8;
    /// A blocked peer's JIT helper window was scanned conservatively.
    pub const XT_HELPER_WINDOW: usize = 9;
    /// A live compiled frame's own spill band holds a young-heap address that
    /// the shadow stack never published, so nothing can rewrite that slot after
    /// a relocation (arch-2026-07-26 `moving-young-corruption-rootcause`).
    pub const UNPUBLISHED_FRAME_OOP: usize = 10;
    /// A live compiled frame's spill band could not be bounded (no exact RBP or
    /// no recorded frame size), so "every oop is published" is unverifiable.
    pub const UNBOUNDED_FRAME_BAND: usize = 11;
    /// The innermost RBP recorded for a chain entry belongs to a DEEPER frame
    /// that was entered by a direct JIT->JIT call (the inline MIC/PIC cascade
    /// or the hashed megamorphic stub), which pushes no guard. The entry's
    /// `compiled_method` therefore does not describe the frame at that RBP, so
    /// neither its coverage nor its oop slots can be resolved.
    pub const FOREIGN_INNERMOST_RBP: usize = 12;
    /// Compiled code is present, but the JIT's precise relocation contract is
    /// not yet strong enough to permit a copying young collection.  JIT code
    /// remains enabled; this only selects the non-moving young sweep.
    pub const JIT_RELOCATION_UNSUPPORTED: usize = 13;
    /// The frame-band verifier could not run: its young-residency test reads
    /// `gen_heap::JIT_REGION_BOUNDS`, and that table is unpublished on this
    /// collector, so every band word classifies as not-young and the verifier
    /// would report "nothing unpublished" without having inspected anything.
    ///
    /// This is the fail-closed answer to a VACUOUS pass, and it belongs with
    /// [`UNBOUNDED_FRAME_BAND`] rather than with [`UNPUBLISHED_FRAME_OOP`]: it
    /// says the frame could not be inspected, not that an oop was missed.
    pub const YOUNG_BOUNDS_UNPUBLISHED: usize = 14;
    /// The runtime completeness oracle (`CRATONVM_DBG_VERIFY_OOP_MAPS`) found an
    /// in-band live object address that NO oop map of the frame names, on a
    /// frame whose `fully_oop_covered` is `true`. The codegen bit's claim was
    /// directly refuted by observation, so the suppression it licenses is
    /// withdrawn — for this cycle and, because the method will run again, for
    /// the rest of the process.
    ///
    /// This is the only reason code produced by *checking the answer* rather
    /// than by failing to establish a precondition.
    pub const COVERAGE_ORACLE_REFUTED: usize = 15;
    /// The frame-band verifier could not run for the OPPOSITE reason to
    /// [`YOUNG_BOUNDS_UNPUBLISHED`]: the tables it tests ARE published, and are
    /// published about a different heap.
    ///
    /// `gen_heap::JIT_REGION_BOUNDS` and `gen_heap::MOVABLE_BOUNDS` are
    /// process-global and discriminated by slot 0, so each describes exactly
    /// one heap. With a second heap alive — a second embedded VM, an init-time
    /// heap not yet dropped — whichever heap lost the slot has every one of its
    /// addresses answer `false` to `gen_heap::addr_is_movable`, and the
    /// verifier reports "nothing unpublished" over frames it never classified.
    ///
    /// Separated from [`YOUNG_BOUNDS_UNPUBLISHED`] because the operator action
    /// differs: that one says a collector publishes nothing and is expected on
    /// G1; this one says the process holds more heaps than the tables can
    /// describe, which no production configuration does.
    pub const BOUNDS_NOT_REPRESENTATIVE: usize = 16;

    /// One past the highest defined reason code. Sizes the per-reason counter
    /// array; a new variant must bump it (asserted by
    /// `every_incomplete_reason_has_a_label`).
    pub const COUNT: usize = 17;

    /// Human-readable label for a reason code (for the fallback diagnostic).
    pub fn label(code: usize) -> &'static str {
        match code {
            NONE => "none",
            COVERAGE_ORACLE_REFUTED => "coverage-oracle-refuted",
            NO_PRECISE_MAP => "jit-entry-without-precise-map",
            MISSING_EXACT_RBP => "missing-exact-rbp",
            ACTIVE_FRAME_MAP => "active-safepoint-map-incomplete",
            PARENT_FRAME_MAP => "parent-frame-map-incomplete",
            UNREGISTERED_JIT_FRAME => "unregistered-jit-frame-on-stack",
            OSR_SHADOW => "osr-shadow-coverage-unproven",
            CROSS_THREAD_JIT_PEER => "cross-thread-jit-peer",
            XT_TAKEOVER => "xt-takeover-conservative-scan",
            XT_HELPER_WINDOW => "xt-helper-window-conservative-scan",
            UNPUBLISHED_FRAME_OOP => "compiled-frame-oop-not-published",
            UNBOUNDED_FRAME_BAND => "compiled-frame-band-unbounded",
            FOREIGN_INNERMOST_RBP => "innermost-rbp-belongs-to-unguarded-callee",
            YOUNG_BOUNDS_UNPUBLISHED => "young-bounds-unpublished-verifier-vacuous",
            BOUNDS_NOT_REPRESENTATIVE => "published-bounds-describe-another-heap",
            JIT_RELOCATION_UNSUPPORTED => "jit-relocation-contract-unproven",
            _ => "unknown",
        }
    }
}

// Per-reason fallback histogram.
//
// `moving_young_coverage_fallback_count()` answers "did moving-young give up?"
// but not "on which obligation?", and the FIRST reason of a cycle is the only
// one recorded per cycle — so a single scalar cannot tell an operator whether
// one obligation is blocking every cycle or ten are blocking one each. That
// distinction is the whole content of the follow-up work, so it is counted.
//
// Process-global in production, thread-local under `cfg(test)`, for the same
// reason as every other counter in this module.
#[cfg(not(test))]
static MOVING_YOUNG_REASON_COUNTS: [AtomicUsize; incomplete_reason::COUNT] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

#[cfg(test)]
thread_local! {
    static MOVING_YOUNG_REASON_COUNTS: std::cell::RefCell<[usize; incomplete_reason::COUNT]> =
        const { std::cell::RefCell::new([0; incomplete_reason::COUNT]) };
}

#[cfg(not(test))]
#[inline]
fn bump_reason_count(reason: usize) {
    if let Some(slot) = MOVING_YOUNG_REASON_COUNTS.get(reason) {
        slot.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(not(test))]
fn read_reason_counts() -> [usize; incomplete_reason::COUNT] {
    let mut out = [0usize; incomplete_reason::COUNT];
    for (i, slot) in MOVING_YOUNG_REASON_COUNTS.iter().enumerate() {
        out[i] = slot.load(Ordering::Relaxed);
    }
    out
}

#[cfg(test)]
#[inline]
fn bump_reason_count(reason: usize) {
    MOVING_YOUNG_REASON_COUNTS.with(|c| {
        if let Some(slot) = c.borrow_mut().get_mut(reason) {
            *slot += 1;
        }
    });
}

#[cfg(test)]
fn read_reason_counts() -> [usize; incomplete_reason::COUNT] {
    MOVING_YOUNG_REASON_COUNTS.with(|c| *c.borrow())
}

/// Histogram of the reasons moving-young cycles fell back to the non-moving
/// sweep, indexed by [`incomplete_reason`] code.
///
/// Paired with [`moving_young_cycle_count`] this is the whole runtime answer to
/// "is the young generation a copying collector, and if not, what is stopping
/// it?" — see `moving-young-corruption-rootcause.md`.
pub fn moving_young_fallback_reason_counts() -> [usize; incomplete_reason::COUNT] {
    read_reason_counts()
}

/// Start a new VM young-GC root-publication cycle. The VM calls this before
/// mutators publish the snapshots that the collector will use for the cycle.
pub fn begin_moving_young_coverage_cycle() {
    coverage_incomplete_set(false);
    incomplete_reason_clear();
    unrewritable_peer_state_set(false);
    CONSERVATIVE_JIT_SCANS.store(0, Ordering::Relaxed);
    // The cross-thread handshake ledger is per-PAUSE and only ever read as
    // "does this account for every peer JIT entry?", so a value carried over
    // from the previous pause would be an over-count — the one direction that
    // could license a relocation nobody proved. Clear it here and again in the
    // barrier's `request_stw`; see `reset_peer_proven_jit_depth`.
    peer_proven_depth_reset_inner();
    // Same scope, same reason: this cycle's helper-window pins describe peers
    // frozen during THIS cycle. Cleared here as well as in
    // `reset_peer_proven_jit_depth`, because this entry point does not go
    // through it and a pause that reaches only one of the two is still cleared.
    clear_xt_cycle_pinned_jit_roots();
    clear_xt_cycle_pinned_jit_depth();
    // Same scope, and this one is load-bearing rather than belt-and-braces.
    // The blocked-peer native-stack captures are produced by exactly the
    // conservative scans `CONSERVATIVE_JIT_SCANS` counts above, and their only
    // drain (`fold_pointer_map_into_blocked_audited`) sits behind
    // `update_all_roots`'s empty-pointer-map early return — i.e. it never runs
    // on a NON-moving cycle. Without a clear here the buffer accumulates every
    // non-moving cycle's captures until it reaches its cap and the repair goes
    // silent on the one cycle whose captures matter. See
    // `clear_peer_stack_slots` for the ABA half of the argument.
    clear_peer_stack_slots();
    // The pairing diagnostic's capture has the same shape and the same drain:
    // `take_peer_reg_capture` runs on the moving path only, so "words captured
    // this cycle" silently included every non-moving cycle since the last
    // relocation.
    clear_peer_reg_capture();
}

/// Whether this cycle's root scan touched state belonging to a peer thread that
/// will never apply the collection's pointer map to itself.
///
/// **This is NOT `moving_young_coverage_incomplete`, and the difference is a
/// live-heap-correctness-vs-throughput fault line.** The two ask different
/// questions:
///
/// * `moving_young_coverage_incomplete` — "can the COPYING young collector run
///   this cycle?" It is false for the overwhelmingly common case of a compiled
///   frame that did not publish a complete rewritable oop map
///   (`UNPUBLISHED_FRAME_OOP`, `MISSING_EXACT_RBP`, …). Those frames are still
///   scanned CONSERVATIVELY, and the non-moving sweep's selective promotion is
///   safe under exactly that regime: it pins by raw slot VALUE, so a
///   conservatively-discovered address — real oop or false positive — is never
///   evacuated.
/// * this predicate — "does some thread hold state that neither the pin-by-value
///   set nor the post-GC remap can protect?" A forcibly OS-suspended in-JIT peer
///   is excused from the safepoint barrier, so it never re-reads its own slots;
///   worse, its registers can hold ONLY a DERIVED/interior pointer to an object
///   whose base is reachable through precise heap edges. An interior address
///   does not resolve to a root, so pin-by-value does not protect the base: the
///   base gets evacuated, its young source zeroed and re-served, and the resumed
///   peer keeps loading through the stale derived pointer. That — and only that
///   — is the hazard the promotion gate was added for (xt-hardening 2026-07-03).
///
/// Conflating them was the `HIB-GCOVERHEAD-HALFFULL.1` defect. The gate was
/// written when `mark_moving_young_coverage_incomplete` had exactly one caller,
/// the cross-thread takeover path. The arch-2026-07-26 moving-young work then
/// reused the same flag for the relocation-capability question, and once
/// moving-young became the default the flag was set on essentially EVERY
/// JIT-active collection — so selective promotion, the non-moving sweep's only
/// way to drain young into old, silently switched off VM-wide. The young
/// generation then filled with live objects that could never leave it: forced
/// GCs freed slivers, the GC-overhead streak latched, and the process died with
/// `OutOfMemoryError` on a heap that was **49 % full with 570 MB free**.
/// Measured on `probes/GcPromoteProbe.java`: 3.9 s with promotion, permanently
/// wedged (`promoted=0`) without it.
#[inline]
pub fn unrewritable_peer_state() -> bool {
    unrewritable_peer_state_get()
}

/// Record that this cycle scanned un-rewritable peer state — see
/// [`unrewritable_peer_state`]. Cleared by
/// [`begin_moving_young_coverage_cycle`].
pub fn mark_unrewritable_peer_state() {
    unrewritable_peer_state_set(true);
}

/// Whether an [`incomplete_reason`] code, on its own, implies this cycle
/// scanned un-rewritable peer state.
///
/// Exactly the two codes the 2026-07-03 promotion gate was scoped to: a peer
/// **OS-suspended** in JIT code, and a **blocked** peer's JIT helper window.
/// Both are excused from the STW barrier, so neither ever re-reads its own
/// registers — that is what makes a derived/interior pointer in one of them
/// unfixable.
///
/// [`incomplete_reason::CROSS_THREAD_JIT_PEER`] is deliberately NOT here. It
/// means only "some peer is somewhere inside compiled code", which is true of
/// nearly every multi-threaded cycle in a warmed-up server workload. Such a peer
/// is parked *cooperatively*: it deposits a root snapshot (including its own
/// conservative JIT-frame scan) that the collection folds into `roots`, so
/// selective promotion's pin-by-value covers it, and it remaps its shadow stack
/// on resume. Classifying it as un-rewritable would re-create
/// `HIB-GCOVERHEAD-HALFFULL.1` for every multi-threaded application — the exact
/// mistake this predicate exists to undo, one abstraction level up. The code is
/// a moving-young-era relocation obligation and was never in the promotion
/// gate's scope.
#[inline]
pub fn reason_implies_unrewritable_peer_state(reason: usize) -> bool {
    matches!(
        reason,
        incomplete_reason::XT_TAKEOVER | incomplete_reason::XT_HELPER_WINDOW
    )
}

/// Record that at least one live JIT frame in this collection lacks a complete
/// moving-young coverage proof. The collector must use the non-moving sweep.
pub fn mark_moving_young_coverage_incomplete() {
    coverage_incomplete_set(true);
}

/// Same as [`mark_moving_young_coverage_incomplete`], but also records WHY, so
/// the warn-level fallback diagnostic names the specific unproven obligation
/// instead of just "incomplete". First reason of a cycle wins (it is the one
/// that actually forced the decision; later ones are consequences).
pub fn mark_moving_young_coverage_incomplete_because(reason: usize) {
    incomplete_reason_set_if_unset(reason);
    incomplete_reason_mask_add(reason);
    coverage_incomplete_set(true);
    // Classify off the reason the CALLER passed, not the stored one: the stored
    // reason is first-wins (it names what forced the decision), so a later
    // cross-thread obligation would otherwise never arm the promotion gate.
    if reason_implies_unrewritable_peer_state(reason) {
        unrewritable_peer_state_set(true);
    }
}

/// The first recorded reason this cycle's moving-young coverage was incomplete
/// (see [`incomplete_reason`]).
#[inline]
pub fn moving_young_incomplete_reason() -> usize {
    incomplete_reason_get()
}

/// EVERY reason recorded this cycle, as a bitmask over [`incomplete_reason`].
///
/// [`moving_young_incomplete_reason`] is first-wins — it names what forced the
/// decision — so it cannot answer "which obligation did THIS proof add?". The
/// mask can, by diffing it around a single call, and that is what the
/// cross-thread handshake's peer diagnostic needs: a peer whose own proof
/// returns false is the shortfall that refuses a whole cycle, and the six ways
/// it can say no want six different repairs.
///
/// Do NOT use the per-reason COUNTERS for that question. `bump_reason_count`
/// has exactly one caller, `record_moving_young_coverage_fallback`, which is
/// the generational collector's per-cycle accounting — so on ZGC those counters
/// never move at all and a diff of them reads `none` for every failure. That
/// mistake cost a build.
#[inline]
pub fn moving_young_incomplete_reason_mask() -> usize {
    incomplete_reason_mask_get()
}

/// Whether the current collection has observed an incomplete moving-young JIT
/// frame/safepoint coverage proof.
#[inline]
pub fn moving_young_coverage_incomplete() -> bool {
    coverage_incomplete_get()
}

/// Bump the diagnostic fallback counter and return the post-increment value.
///
/// **Emits at `warn` level, ON BY DEFAULT.** A silent regression to the
/// non-moving sweep is precisely how the moving young generation stayed
/// switched off while the architecture docs advertised it (see
/// `moving-young-precise-roots.md`): the only
/// signal was a `tracing::debug!` line reading "compaction deferred" and a
/// counter behind `CRATONVM_MOVING_YOUNG_FALLBACKS`, which nobody set.
/// Rate-limited (every occurrence up to 8, then powers of two) so a genuinely
/// non-provable workload cannot flood the log, but the FIRST one is always
/// visible; `gc_flags().moving_young_fallbacks` now asks for *all* of them
/// rather than being what makes any of them appear.
pub fn record_moving_young_coverage_fallback() -> usize {
    let n = bump_fallbacks();
    // Attribute this cycle to the obligation that actually forced it, so the
    // histogram answers "which proof is blocking moving-young?" even when the
    // rate limiter has suppressed the log line.
    bump_reason_count(moving_young_incomplete_reason());
    if n <= 8 || n.is_power_of_two() || gc_flags().moving_young_fallbacks {
        tracing::warn!(
            "[moving-young] fallback #{n}: reason={} — a live JIT frame could not prove a \
             complete rewritable root map, so this young collection runs the NON-MOVING \
             sweep (no compaction, free-list allocation). Persistent fallbacks mean the \
             young generation is not actually a copying collector.",
            incomplete_reason::label(moving_young_incomplete_reason()),
        );
    }
    // Sizing line for a prospective repair: the first-wins label above cannot
    // say whether a cycle would have become movable had one obligation been
    // provable, because other obligations may have failed in the same cycle.
    //
    // `attributable=innermost-rbp` is the metric that answers it, and it is
    // deliberately NOT "the mask holds exactly one bit". One failure of
    // `innermost_frame_method` is reported TWICE by design: the coverage refresh
    // records `FOREIGN_INNERMOST_RBP`, and the band-verification walk, which
    // calls the same helper and bails at the same `None`, records
    // `UNBOUNDED_FRAME_BAND` (see `vm/src/jit/conservative_roots.rs`, the
    // `innermost_frame_method` bail whose comment names the other reason). A
    // single-bit test can therefore NEVER fire for this cause and would price
    // the repair at zero — it did, before this was corrected.
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_GC_FALLBACK_REASONS").is_some() {
        let mask = incomplete_reason_mask_get();
        let first = moving_young_incomplete_reason();
        let innermost_pair = (1usize << incomplete_reason::FOREIGN_INNERMOST_RBP)
            | (1usize << incomplete_reason::UNBOUNDED_FRAME_BAND);
        let attributable_innermost = mask != 0
            && mask & (1usize << incomplete_reason::FOREIGN_INNERMOST_RBP) != 0
            && mask & !innermost_pair == 0;
        let mut all = String::new();
        for code in 0..incomplete_reason::COUNT {
            if mask & (1usize << code) != 0 {
                if !all.is_empty() {
                    all.push(',');
                }
                all.push_str(incomplete_reason::label(code));
            }
        }
        eprintln!(
            "[moving-young-reasons] #{n} first={} sole={} attributable-innermost-rbp={} all={}",
            incomplete_reason::label(first),
            if mask == (1usize << first) {
                "yes"
            } else {
                "no"
            },
            if attributable_innermost { "yes" } else { "no" },
            all,
        );
    }
    n
}

/// Number of moving-young cycles diverted to the non-moving sweep because at
/// least one live JIT frame did not have complete coverage.
pub fn moving_young_coverage_fallback_count() -> usize {
    read_fallbacks()
}

/// Record that a young collection actually ran the MOVING (Cheney) cycle while
/// a JIT frame was live.
///
/// The counterpart to [`record_moving_young_coverage_fallback`]: together they
/// make "is the young generation actually copying?" answerable at runtime
/// instead of by reading the collector source.
pub fn record_moving_young_cycle() -> usize {
    bump_moving_cycles()
}

/// Number of young collections that ran the moving (Cheney) cycle under a live
/// JIT frame.
pub fn moving_young_cycle_count() -> usize {
    read_moving_cycles()
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
    ENTER_COUNT.inc();
    active_depth_enter()
}

/// Decrement the global JIT-active counter. Called from the VM crate's
/// `JitEntryGuard::drop`. Returns the new depth (post-decrement). It is a
/// debug-assert error to call this when the counter is already 0; the
/// release version saturates at 0 so a stray pop never wraps the counter.
pub fn leave() -> usize {
    LEAVE_COUNT.inc();
    active_depth_leave()
}

/// Returns true if any thread is currently inside a JIT call.
#[inline]
pub fn is_active() -> bool {
    active_depth_nonzero()
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
// Residue census for the unregistered-JIT-frame probe
// ---------------------------------------------------------------------------
//
// The probe reads raw stack words and calls any word that lands inside a
// registered JIT code range a frame. A compiled method that has ALREADY
// RETURNED left exactly such a word at every depth below its own `entry_sp`,
// so "there is a JIT return address up there" and "a compiled frame is live up
// there" are not the same statement. `conservative_roots::jit_residue_hi` is
// the discriminator the VM side already maintains for it.
//
// These two counters say which of the two a run actually saw, because
// `relocation-coverage-reason: unregistered-jit-frame-on-stack=N` cannot: it
// reads identically for a run held back by a live entry-point frame and for one
// held back by the leftovers of a frame that returned minutes ago. On the H2
// `MvsCreate` ZGC OOM every hit was the second kind.

static UNREG_RESIDUE_EXPLAINED: AtomicUsize = AtomicUsize::new(0);
static UNREG_RESIDUE_LIVE: AtomicUsize = AtomicUsize::new(0);

/// A probe hit that the returned-frame residue mark fully explains: the band it
/// was found in is one a returned compiled frame may have written, and no hit
/// remains above the mark. The frame's oops are still conservatively MARKED (and
/// therefore page-pinned); only the relocation refusal is withheld.
pub fn note_unregistered_jit_frame_residue() {
    UNREG_RESIDUE_EXPLAINED.fetch_add(1, Ordering::Relaxed);
}

/// A probe hit at or above the residue mark — a band no returned frame on this
/// thread can have written, so it is treated as a genuinely live guardless
/// compiled frame and the relocation refusal stands.
pub fn note_unregistered_jit_frame_live() {
    UNREG_RESIDUE_LIVE.fetch_add(1, Ordering::Relaxed);
}

/// `(explained_by_residue, above_the_mark)` for the run so far.
pub fn unregistered_jit_frame_residue_census() -> (usize, usize) {
    (
        UNREG_RESIDUE_EXPLAINED.load(Ordering::Relaxed),
        UNREG_RESIDUE_LIVE.load(Ordering::Relaxed),
    )
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
// Explicit System.gc() full-collection request
// ---------------------------------------------------------------------------
//
// Real HotSpot's `System.gc()` triggers a FULL (young + old generation)
// collection by default (`-XX:+DisableExplicitGC` opts out; CratonVM has no
// equivalent knob yet, so the default must match it). Without this,
// `GenerationalHeap::collect_garbage_inner`'s Phase 5 only runs `major_gc`
// when old gen crosses an occupancy threshold (75% full) — an object already
// promoted to old gen that has genuinely become garbage (e.g. a per-JSP
// `ClassLoader` Tomcat/Jasper has dropped every reference to after evicting a
// JSP) is NEVER swept by a `System.gc()` call that only triggers a minor
// collection, because `VmHeap::is_addr_live` treats EVERY old-gen address as
// live during a minor cycle (old gen isn't touched at all this pass) — the
// collector simply never gets a chance to prove it dead. Symptom:
// `TestDefaultInstanceManager.testClassUnloading`'s off-by-one (an unloaded
// JSP's `Class`/`ClassLoader`, once promoted, survives forever unless old gen
// happens to independently cross the occupancy threshold on its own).
//
// `force_gc_from_native` (`System.gc()`'s native impl) sets this before
// invoking the collector; Phase 5 in `gen_heap.rs` consults it via
// `take_major_gc_request` (check-and-clear — consumed exactly once per
// collection, so a later UNRELATED allocation-triggered minor GC doesn't also
// get forced into a major cycle it didn't ask for).

thread_local! {
    static MAJOR_GC_REQUESTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static CLASS_UNLOAD_MARKING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Did the collection this thread just ran reclaim OLD-generation storage?
    ///
    /// A minor cycle never touches old gen, so every old-gen address is still
    /// exactly where it was and `VmHeap::is_addr_live` may (and does) report
    /// all of them live. Once an old-gen reclamation runs — the mark-COMPACT
    /// `major_gc` or the in-place `sweep_old_gen_non_moving` — that stops
    /// being true: a dead old-gen object is slid over by a live neighbour or
    /// returned to the free list, and the freed tail is zeroed. Post-GC
    /// reference processing must then stop trusting "the address is inside
    /// old gen" as a survival proof and require a `pointer_map` entry, which
    /// both old-gen paths now emit (identity for stationary survivors) for
    /// every watched address.
    ///
    /// Set by the collector, read by `VmHeap::watched_pre_gc_addr_survived`
    /// on the same (collecting) thread inside the same STW window.
    static OLD_GEN_RECLAIMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Record whether the collection now in flight reclaimed old-generation
/// storage. Called with `false` at the start of every cycle and `true` by
/// whichever old-gen path actually ran. See `OLD_GEN_RECLAIMED`.
#[inline]
pub fn set_old_gen_reclaimed(v: bool) {
    OLD_GEN_RECLAIMED.with(|c| c.set(v));
}

/// Did the collection whose `pointer_map` is being consumed reclaim old-gen
/// storage? See `OLD_GEN_RECLAIMED`.
#[inline]
pub fn old_gen_reclaimed_last_cycle() -> bool {
    OLD_GEN_RECLAIMED.with(std::cell::Cell::get)
}

/// Run one root-gather operation for a collector's non-moving class-unloading
/// mark. Ordinary moving/evacuating pauses keep loader metadata strongly
/// rooted; only an initial/final full-mark snapshot may publish conditional
/// loader-owned edges.
pub fn with_class_unload_marking<T>(f: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            CLASS_UNLOAD_MARKING.with(|flag| flag.set(self.0));
        }
    }

    let previous = CLASS_UNLOAD_MARKING.with(|flag| {
        let previous = flag.get();
        flag.set(true);
        previous
    });
    let _reset = Reset(previous);
    f()
}

/// Whether this thread is gathering roots for a full non-moving mark whose
/// side-edge closure understands loader-owned metadata.
#[inline]
pub fn class_unload_marking() -> bool {
    CLASS_UNLOAD_MARKING.with(std::cell::Cell::get)
}

/// Request that the next collection on this thread run a full (major) cycle
/// regardless of old-gen occupancy. Set by `System.gc()`'s native
/// implementation (`force_gc_from_native`).
pub fn request_major_gc() {
    MAJOR_GC_REQUESTED.with(|c| c.set(true));
}

/// True while this thread has an explicit `System.gc()` major-GC request
/// pending. The root gatherer and Generational collector use this to choose
/// the non-moving owner-propagating overlay marker for the requested full GC.
#[inline]
pub fn major_gc_requested() -> bool {
    MAJOR_GC_REQUESTED.with(|c| c.get())
}

/// Check-and-clear: consumed exactly once by the collector's Phase 5 check,
/// regardless of which branch of that check ends up true — so the request
/// never leaks into a later, unrelated collection.
#[inline]
pub fn take_major_gc_request() -> bool {
    MAJOR_GC_REQUESTED.with(|c| {
        let v = c.get();
        c.set(false);
        v
    })
}

/// Will the young half of the collection this thread is about to initiate
/// certainly take the NON-MOVING young marker?
///
/// The non-moving young marker (`gen_heap::mark_young_precise_object`) follows
/// the loader-scoped side-table edges — `loader_pin`, `mirror_pin` and
/// `metadata_pin` — as ordinary marking edges. The MOVING (Cheney) young
/// closure does not: it seeds strictly from the direct root set. So a young
/// object reachable ONLY through one of those side tables can safely be left
/// out of the unconditional root set exactly when this returns true, and must
/// be rooted directly otherwise.
///
/// Mirrors `GenerationalHeap::collect_garbage_inner`'s `divert_non_moving`
/// decision, but deliberately only in its *certain* direction: every arm here
/// forces the non-moving sweep on its own. A false negative merely costs one
/// extra conservative root; a false positive would DROP a live root, so this
/// errs strictly toward `false`.
///
/// # Term-by-term, against `divert_non_moving`
///
/// `divert_non_moving` is
///
/// ```text
/// (has_conservative_roots && !moving_young) || honor_promotion_oom_risk
///     || divert_for_incomplete_moving_coverage || explicit_full_gc
/// ```
///
/// and only `CRATONVM_DBG_FORCE_MOVING` can carry a cycle past it — hence the
/// veto below. Of the four terms, two are usable here:
///
/// * `explicit_full_gc` (`major_gc_requested`) diverts **on its own**, with no
///   reference to moving-young at all. This is the `System.gc()` case.
/// * `has_conservative_roots && !moving_young` — the legacy conservative-JIT-root
///   rule, live only while moving-young is off.
///
/// The other two (`honor_promotion_oom_risk`,
/// `divert_for_incomplete_moving_coverage`) are per-cycle verdicts not yet
/// decided when the root gatherer asks, so they are conservatively ignored.
///
/// # Why the `!moving_young_enabled()` term is NOT a common factor
///
/// It used to be: this function read
///
/// ```text
/// !dbg_force_moving && !moving_young_enabled() && (is_active() || … || major_gc_requested())
/// ```
///
/// which factored the `!moving_young` guard — correct for the conservative-root
/// term — across `major_gc_requested()` as well, where `divert_non_moving` has
/// no such guard. That was invisible while `DEFAULT_MOVING_YOUNG` was `false`.
/// When it flipped to `true` (2026-07-28, `67de5400a`) the whole predicate
/// became unconditionally `false` on the shipped default, silently disarming
/// [`crate::VmHeap::mirror_pin_deferrable`]'s young-mirror deferral and
/// re-opening `TestDefaultInstanceManager.testClassUnloading` for the third
/// time — a fix still present in the tree, and inert. Compare
/// `vm::memory::roots::conditional_loader_metadata`, which asks the same
/// question and does not carry the term. (It did carry it for a month; the
/// disjunct was inert for the reason the next paragraph gives, and was removed
/// on 2026-09-08 so this comparison is true again. Do not re-add it.)
///
/// Note also that `unregistered_jit_frame_on_stack()` is always `false` at the
/// mirror call site: `collect_roots` clears it (and
/// `force_non_moving_jit_roots`) before step 6 and only re-sets it at step 14's
/// JIT scan. It is kept for callers that ask later in the pass; a `false` there
/// is a false negative, which is the safe direction. `collect_roots`' own A5
/// repair (step 14a5) is the model for anything that needs the TRUE answer:
/// ask after the JIT scan, not before it.
pub fn young_marker_follows_side_tables() -> bool {
    // The one switch that can push a cycle past `divert_non_moving` entirely.
    if crate::gc_flags().dbg_force_moving {
        return false;
    }
    // `explicit_full_gc`: certain, and independent of moving-young.
    if major_gc_requested() {
        return true;
    }
    // `has_conservative_roots && !moving_young`: certain only while
    // moving-young is off.
    !moving_young_enabled() && (is_active() || unregistered_jit_frame_on_stack())
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
// Unrewritable JIT roots — the VETO over the movable set above
// ---------------------------------------------------------------------------
//
// "Movable" above is a claim about a SLOT: this frame word sits in a precise,
// rewritable channel (an oop map entry, a shadow-stack cell), so the collector
// may evacuate what it points at and fix the word up afterwards. The pin set is
// keyed by OBJECT ADDRESS, so one such claim licenses moving the object — for
// every word in the process, including words nobody can rewrite.
//
// A compiled frame has such words. `conservative_roots::band_slot_is_verifiable`
// splits a frame's band in two: the half it inspects is verified and rewritten,
// and the half it skips — the prologue's callee-saved GPR/XMM save areas, the
// per-safepoint blind GPR spill, the outgoing-argument / deopt reserve, and
// operand-spill slots above the safepoint's live cursor — is neither. The
// conservative band scan READS those words (that is what keeps the object
// alive), so the object is a root; if the SAME object is also named by an oop
// map or a shadow-stack cell it is published movable, gets evacuated, and the
// unrewritable word is left holding a from-space address.
//
// That is not hypothetical: the callee-saved GPR image a compiled prologue
// writes holds the CALLER's registers, and the epilogue pops them straight back
// — so the caller resumes from exactly the words the verifier declined to look
// at. See
// `moving-young-left-a-callee-saved-register-image-unrewritten-FIXED-20260823`
// for the detector that measured the gap.
//
// This set is the veto. A word in an unverifiable region that resolves to a
// live object publishes that object's address here, and the young sweep's pin
// decision reads it as "pin regardless of any movable claim". The cost is
// exactly the conservative cost the band scan already pays on the MARKING side
// — an `i64` that happens to equal an object address defers that object's
// promotion by one cycle — and it can never dangle. Rewriting the word instead
// would be the opposite trade: a caller's callee-saved register holding a
// non-pointer equal to a moved object's from-address would be CORRUPTED.
//
// Thread-local for the same reason `MOVABLE_JIT_ROOTS` is: it exists only to
// veto that set, and a root another thread never published as movable is
// already pinned. A missed publication leaves the legacy behaviour; a stale
// entry would only over-pin, and the per-pass clear prevents even that.

thread_local! {
    static UNREWRITABLE_JIT_ROOTS: std::cell::RefCell<std::collections::HashSet<usize>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

/// Clear the unrewritable-JIT-root set. Called by the VM's root gatherer at the
/// start of every collection, beside [`clear_movable_jit_roots`].
pub fn clear_unrewritable_jit_roots() {
    UNREWRITABLE_JIT_ROOTS.with(|s| s.borrow_mut().clear());
}

/// Record `addr` as reachable from a compiled-frame word no channel can
/// rewrite, so it must be pinned this cycle whatever else claims it is movable.
pub fn add_unrewritable_jit_root(addr: usize) {
    UNREWRITABLE_JIT_ROOTS.with(|s| {
        s.borrow_mut().insert(addr);
    });
}

/// True if `addr` was published as unrewritable this cycle. Vetoes
/// [`is_movable_jit_root`] at the pin decision.
#[inline]
pub fn is_unrewritable_jit_root(addr: usize) -> bool {
    UNREWRITABLE_JIT_ROOTS.with(|s| s.borrow().contains(&addr))
}

/// Count of unrewritable roots published this cycle (diagnostics).
pub fn unrewritable_jit_root_count() -> usize {
    UNREWRITABLE_JIT_ROOTS.with(|s| s.borrow().len())
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
// CROSS-THREAD (2026-07-10, MTChurn lost-increment fix): this registry is
// process-global, keyed by publishing thread. The original design was a plain
// `thread_local!` set on the assumption that "the JIT entry chain and the
// (self-triggered) collection run on the same thread" — but that only covers
// the GC INITIATOR's own JIT frames. Every OTHER mutator that parks at the STW
// barrier (or sits in a blocking native) with live JIT frames publishes its
// conservative roots into its root SNAPSHOT (which keeps the objects alive)
// while its thread-local pin set was invisible to the initiator's
// `pinned_jit_roots_snapshot()` — so G1 evacuated the objects anyway and the
// parked thread resumed its compiled code on dangling from-space addresses
// (observed as massive lost `synchronized` increments + zero-header field
// writes under multi-threaded churn).
//
// Model: each thread owns one entry (replace-on-publish, so stale pins drop as
// soon as the thread republishes with fewer/no JIT frames — it deposits at
// every safepoint arrival and blocking-region entry). The entry is removed by
// a TLS drop guard when the thread exits. `pinned_jit_roots_snapshot()` is the
// union across threads: by the time the initiator selects a CSet, every
// counted mutator has parked (and therefore republished), so the union is
// current. Over-pinning (an entry from a thread that left JIT after its last
// deposit) is safe — it only keeps a region out of one CSet.

static PINNED_JIT_ROOTS_BY_THREAD: std::sync::OnceLock<
    std::sync::Mutex<
        std::collections::HashMap<std::thread::ThreadId, std::collections::HashSet<usize>>,
    >,
> = std::sync::OnceLock::new();

fn pinned_jit_map() -> &'static std::sync::Mutex<
    std::collections::HashMap<std::thread::ThreadId, std::collections::HashSet<usize>>,
> {
    PINNED_JIT_ROOTS_BY_THREAD
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// TLS guard: removes this thread's pin-registry entry when the thread exits,
/// so a dead thread's regions do not stay pinned forever.
struct PinnedJitRootsGuard(std::thread::ThreadId);
impl Drop for PinnedJitRootsGuard {
    fn drop(&mut self) {
        if let Ok(mut map) = pinned_jit_map().lock() {
            map.remove(&self.0);
        }
    }
}
thread_local! {
    static PINNED_JIT_ROOTS_GUARD: std::cell::OnceCell<PinnedJitRootsGuard> =
        const { std::cell::OnceCell::new() };
}
fn arm_pinned_guard() {
    PINNED_JIT_ROOTS_GUARD.with(|g| {
        let _ = g.get_or_init(|| PinnedJitRootsGuard(std::thread::current().id()));
    });
}

/// Clear the CALLING thread's conservative-pinned-JIT-root entry. Called by
/// the VM's root gatherer (initiator) at the start of every collection,
/// before its own JIT-frame scan republishes. Other threads' entries are
/// left intact — they are owned by those threads' deposits.
pub fn clear_pinned_jit_roots() {
    if let Ok(mut map) = pinned_jit_map().lock() {
        map.remove(&std::thread::current().id());
    }
}

/// Record `addr` (an object address discovered conservatively in a JIT frame,
/// whose holder slot the collector cannot rewrite) as pin-required for G1,
/// owned by the calling thread.
pub fn add_pinned_jit_root(addr: usize) {
    arm_pinned_guard();
    CONSERVATIVE_JIT_SCANS.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut map) = pinned_jit_map().lock() {
        map.entry(std::thread::current().id())
            .or_default()
            .insert(addr);
    }
}

/// Replace the CALLING thread's pin entry wholesale with `addrs` (removing it
/// when empty). Used by the root-snapshot deposit paths so a thread's pins
/// always reflect its CURRENT live JIT frames.
pub fn publish_pinned_jit_roots(addrs: &[usize]) {
    arm_pinned_guard();
    // BEFORE the emptiness test below. "This thread looked and found nothing"
    // and "this thread never looked" are different facts and the map cannot
    // hold the difference -- see `conservative_jit_scans`.
    CONSERVATIVE_JIT_SCANS.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut map) = pinned_jit_map().lock() {
        let tid = std::thread::current().id();
        if addrs.is_empty() {
            map.remove(&tid);
        } else {
            map.insert(tid, addrs.iter().copied().collect());
        }
    }
}

/// How many threads have published a conservative JIT-frame scan since
/// [`begin_moving_young_coverage_cycle`] reset the count.
///
/// # Why a count and not just the pin set
///
/// [`pinned_jit_roots_snapshot`] is EMPTY in two completely different
/// situations: nobody found a conservative root (fine -- there is nothing to
/// pin), and nobody looked (fatal -- a collector that pins by value would then
/// pin nothing and relocate everything, believing it was protected).
///
/// A consumer that treats the empty set as a licence needs to be able to tell
/// those apart, and the set itself cannot. This is the discriminator: a zero
/// here beside live compiled frames means the instrument was armed where it
/// cannot fire, which is a refusal rather than a pass.
///
/// Bumped by both publication paths, including a publication of an EMPTY
/// vector -- "this thread looked and found nothing" is exactly the fact that
/// has to be distinguishable.
pub fn conservative_jit_scans() -> usize {
    CONSERVATIVE_JIT_SCANS.load(Ordering::Relaxed)
}

/// Conservative JIT-frame scans published this cycle. See
/// [`conservative_jit_scans`].
static CONSERVATIVE_JIT_SCANS: AtomicUsize = AtomicUsize::new(0);

/// Snapshot the conservative-pinned-JIT-root addresses published by ALL
/// threads. `G1Collector` maps these to regions it must exclude from the
/// collection set.
pub fn pinned_jit_roots_snapshot() -> Vec<usize> {
    let mut out: Vec<usize> = match pinned_jit_map().lock() {
        Ok(map) => map.values().flat_map(|s| s.iter().copied()).collect(),
        Err(_) => Vec::new(),
    };
    // Plus the peers nobody could publish FOR: see `XT_CYCLE_PINNED_JIT_ROOTS`.
    if let Ok(set) = xt_cycle_pin_set().lock() {
        out.extend(set.iter().copied());
    }
    out
}

/// Count of conservative-pinned JIT roots currently published (diagnostics).
pub fn pinned_jit_root_count() -> usize {
    let per_thread: usize = match pinned_jit_map().lock() {
        Ok(map) => map.values().map(|s| s.len()).sum(),
        Err(_) => 0,
    };
    per_thread + xt_cycle_pinned_jit_root_count()
}

// ---------------------------------------------------------------------------
// Cross-thread HELPER-WINDOW pins (this cycle only)
// ---------------------------------------------------------------------------
//
// `PINNED_JIT_ROOTS_BY_THREAD` above is published BY EACH THREAD, at its own
// safepoint arrival or blocking-region entry. A helper-window peer is exactly
// the thread that reached NEITHER: it was interrupted by the collector's signal
// while inside a Rust helper called from compiled code, so it has no entry, and
// the scan that recovers its roots runs on the COLLECTOR's thread and cannot
// publish under the peer's `ThreadId`.
//
// Without somewhere to put them, those roots were only ever marked, and the
// cycle refused to relocate at all (`incomplete_reason::XT_HELPER_WINDOW`). On
// `org.h2.test.jdbc.TestCachedQueryResults` that is 219 of 227 refusals -- the
// entire reason ZGC never compacts on the H2 fragmentation family.
//
// The set is per-CYCLE rather than per-thread because that is its real scope:
// `helper_window_pass` recomputes it from scratch on every collection, and the
// peer it describes has resumed by the next one. `begin_moving_young_coverage_cycle`
// clears it, which is the same point that clears the coverage verdict it used
// to be expressed as.
static XT_CYCLE_PINNED_JIT_ROOTS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<usize>>,
> = std::sync::OnceLock::new();

fn xt_cycle_pin_set() -> &'static std::sync::Mutex<std::collections::HashSet<usize>> {
    XT_CYCLE_PINNED_JIT_ROOTS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Pin `addrs` for the remainder of this collection.
///
/// The caller must have recovered them from a COMPLETE conservative scan of the
/// peer -- its register file AND its whole readable stack band. A partial scan
/// must keep refusing the cycle instead: pinning what you found does not help
/// when what you missed is also unrewritable.
pub fn add_xt_cycle_pinned_jit_roots(addrs: &[usize]) {
    if addrs.is_empty() {
        return;
    }
    if let Ok(mut set) = xt_cycle_pin_set().lock() {
        set.extend(addrs.iter().copied());
    }
}

/// Drop this cycle's helper-window pins. Called from
/// [`begin_moving_young_coverage_cycle`].
pub fn clear_xt_cycle_pinned_jit_roots() {
    if let Ok(mut set) = xt_cycle_pin_set().lock() {
        set.clear();
    }
}

/// How many helper-window pins the current cycle published (diagnostics).
pub fn xt_cycle_pinned_jit_root_count() -> usize {
    match xt_cycle_pin_set().lock() {
        Ok(set) => set.len(),
        Err(_) => 0,
    }
}

// ---------------------------------------------------------------------------
// Watched Weak/Soft/Phantom reference referents (non-moving-sweep pointer_map
// completeness — RandomizedContext WeakHashMap<Thread,...> fix, 2026-07-02).
//
// `GenerationalHeap::sweep_young_non_moving` keeps every non-promoted
// survivor in young gen AT ITS ORIGINAL ADDRESS: selective promotion only
// evacuates survivors old enough to tenure (or explicitly un-pinned), so the
// large majority of any cycle's survivors are simply left in place with NO
// `pointer_map` entry (nothing moved, nothing to remap). Post-GC reference
// processing (`process_references_after_gc`'s `is_marked` closure in the VM)
// treats an address absent from `pointer_map` — and not resident in old gen —
// as "did not survive this collection". That is correct for a MOVING
// collector (every survivor is relocated and therefore recorded), but wrong
// here: a live Weak/Soft/PhantomReference whose referent is a young,
// not-yet-promoted survivor gets incorrectly cleared out from under a still-
// running mutator. Observed as `com.carrotsearch.randomizedtesting.
// RandomizedContext.getPerThread()` returning null for its OWN WeakHashMap
// key — the running suite thread's `java.lang.Thread` mirror — well after
// the entry was legitimately created (see
// elasticsearch-randomizedcontext-per-thread-null.md).
//
// Fix: the VM publishes the currently-registered Weak/Soft/Phantom referent
// addresses here immediately before a collection (same thread that will run
// `collect_garbage`, mirroring `PINNED_JIT_ROOTS` above). The non-moving
// sweep checks this set for every KEPT-IN-PLACE survivor it visits and, on a
// hit, adds an IDENTITY (`addr -> addr`) entry to the `pointer_map` it
// returns — enough for `is_marked` to recognize the object as having
// survived. Bounded by the number of live Reference objects registered with
// the VM's reference processor, NOT by the size of the young generation, so
// this does not reintroduce the O(live-set) cost selective promotion exists
// to avoid (see the `sweep_young_non_moving` module comments on bt18).
thread_local! {
    static WATCHED_REFERENTS: std::cell::RefCell<std::collections::HashSet<usize>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

/// Replace the watched-referent set for the upcoming collection. Called by
/// the VM immediately before `collect_garbage`, right after nulling the
/// Java-visible referent fields (`weakref_null_referents_pre_gc`) so the
/// mark phase's normal field scan cannot ALSO keep these referents alive —
/// this set exists purely to answer "did address X survive the collection
/// some OTHER way", not to influence marking. Always call this before a
/// collection (with an empty slice if there is nothing to watch this cycle)
/// so a stale entry from a previous cycle can never leak into this one.
pub fn set_watched_referents(addrs: &[usize]) {
    WATCHED_REFERENTS.with(|s| {
        let mut s = s.borrow_mut();
        s.clear();
        s.extend(addrs.iter().copied());
    });
}

// ---------------------------------------------------------------------------
// Cross-thread STW peer-scan coverage, published per collection.
//
// `xt_root_scan::take_over_pass` runs during ROOT COLLECTION, on the collecting
// thread, immediately before the heap collection it feeds — so "last pass" is
// this cycle's pass. It is published here rather than read from
// `cratonvm_vm::jit::xt_root_scan` because the GC crate cannot depend on the VM
// crate, and because the number that matters (did this sweep mark from a
// COMPLETE root set?) belongs next to the sweep's own diagnostics rather than
// in a shutdown summary a looping reproduction never reaches.
//
// `unclassified` is the load-bearing one. On Linux a peer is taken over by
// signalling it and waiting for it to park in the handler; a peer that never
// answers within the deadline is STILL RUNNING JIT CODE, and its JIT-frame
// object references are in no root set at all. A sweep that runs with
// `unclassified > 0` therefore decided liveness from an incomplete root set,
// which is exactly the shape of a use-after-free the heap-side referrer scan
// cannot see (it scans the heap and the root slice; these roots are in neither).
// ---------------------------------------------------------------------------

/// Take-over passes actually RUN this cycle.
///
/// The load-bearing one for reading the rest. `stw_takeover_should_scan` gates
/// round 0 on the cheap `any_thread_in_jit()` hint, so a fully cooperative
/// pause legitimately runs ZERO passes — and then `taken_over=0
/// unclassified=0` means "never looked", not "looked and found nothing". Those
/// two readings point at opposite conclusions, so they must not share an
/// encoding.
/// Collections whose peer-JIT accounting was SKIPPED because `peer_depth`
/// read as zero.
///
/// `refresh_moving_young_coverage_for_collection` only runs the peer handshake
/// inside `if peer_depth > 0`. A zero therefore does not mean "the peers were
/// accounted for" -- it means nothing was asked. On a workload where peers are
/// continuously in compiled code that is the one door to the moving arm that
/// no ledger guards, so it has to be countable separately from an accepted
/// handshake.
/// Register words captured from a FROZEN peer, for the stale-register pairing.
///
/// `(os_tid, register index, value)`. The whole GPR block is recorded, not just
/// the words a root filter accepted: the question is whether a peer resumes
/// holding an address the collector MOVED, and pre-filtering with the same
/// predicate the collector already trusts would beg it.
///
/// Empty and untouched unless `CRATONVM_DBG_PEER_REG_PAIRING` is set.
pub static PEER_REG_CAPTURE: parking_lot::Mutex<Vec<(u32, u8, usize)>> =
    parking_lot::Mutex::new(Vec::new());

/// Peer register words that turned out to name a RELOCATED object.
///
/// The counter behind the pairing §10.11 asked for: a frozen peer's registers
/// are not heap, not frame-band memory, and are never rewritten by a Cheney
/// copy, so a non-zero reading is a thread that will resume with a pointer to
/// an address the collection vacated.
pub static PEER_REG_STALE: AtomicU64 = AtomicU64::new(0);

/// Record one frozen peer's register word. No-op unless the pairing is armed.
/// Default-ON. `CRATONVM_GC_NO_BLOCKED_PEER_STACK_REMAP=1` restores the
/// pre-2026-09-07 behaviour, where a blocked peer resumed with its
/// conservatively-scanned stack words still at their pre-move addresses.
pub fn blocked_peer_stack_remap_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_GC_NO_BLOCKED_PEER_STACK_REMAP").is_none()
    })
}

/// `(os_tid, addr, value)` for every native-stack word this cycle's
/// cross-thread scan resolved to a heap object. Drained by
/// `ThreadRegistry::fold_pointer_map_into_blocked_audited`, which moves each
/// entry onto its owning blocked thread.
static PEER_STACK_SLOTS: parking_lot::Mutex<Vec<(u32, usize, usize)>> =
    parking_lot::Mutex::new(Vec::new());

/// Engagement census for the blocked-peer native-stack remap. `CAPTURED` is the
/// denominator; `WRITTEN` is the repair actually storing a new address; and
/// `SKIPPED` is the wake guard declining because the word no longer reads its
/// captured value (the native call reused it). A run with `written=0` did not
/// exercise the repair at all, and no conclusion may be drawn from its result.
pub static PEER_STACK_SLOTS_CAPTURED: AtomicU64 = AtomicU64::new(0);
pub static PEER_STACK_SLOTS_ADOPTED: AtomicU64 = AtomicU64::new(0);
pub static PEER_STACK_SLOTS_WRITTEN: AtomicU64 = AtomicU64::new(0);
pub static PEER_STACK_SLOTS_SKIPPED: AtomicU64 = AtomicU64::new(0);

/// Captures the buffer refused because it was already at its cap.
///
/// Non-zero is a REPAIR OUTAGE, not a tuning note: the words this pass exists
/// to rewrite were the ones it declined to record. It reads zero only while the
/// buffer's lifetime is genuinely per-cycle — see
/// [`clear_peer_stack_slots`]'s caller.
pub static PEER_STACK_SLOTS_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Captures discarded at the next cycle's open because the cycle that took them
/// never relocated.
///
/// These are correct discards — nothing moved, so nothing needs rewriting — and
/// they are counted separately so they can never be mistaken for [`
/// PEER_STACK_SLOTS_UNROUTED`], which is the population that DID need a channel
/// and got none.
pub static PEER_STACK_SLOTS_DISCARDED: AtomicU64 = AtomicU64::new(0);

/// Captures a RELOCATING cycle's fold could not hand to any thread.
///
/// The fold adopts a capture onto its owning thread only while that thread is
/// inside a blocked region; a peer frozen by the take-over path is in
/// `CompiledUninterruptible` instead and has no wake hook to apply a fixup at.
/// A non-zero reading is therefore a word in a live peer's stack that named an
/// object this cycle moved and that nothing will ever rewrite — the defect
/// `bytebuf-multiplethreads-npe-generational-moving-young` is about, counted
/// instead of assumed absent.
pub static PEER_STACK_SLOTS_UNROUTED: AtomicU64 = AtomicU64::new(0);

/// Record one scanned native-stack word and the address it lives at.
pub fn record_peer_stack_slot(os_tid: u32, addr: usize, value: usize) {
    if !blocked_peer_stack_remap_enabled() {
        return;
    }
    let mut g = PEER_STACK_SLOTS.lock();
    // Bounded. A runaway capture would cost the pause it is trying to make
    // correct; the observed population is 19-91 words per cycle.
    if g.len() < 65536 {
        g.push((os_tid, addr, value));
        PEER_STACK_SLOTS_CAPTURED.fetch_add(1, Ordering::Relaxed);
    } else {
        PEER_STACK_SLOTS_DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Drain the cycle's captures. Called once per collection by the fold.
pub fn take_peer_stack_slots() -> Vec<(u32, usize, usize)> {
    let mut g = PEER_STACK_SLOTS.lock();
    std::mem::take(&mut *g)
}

/// Discard the cycle's captures without applying them -- for the paths that
/// scan but then do not relocate, so nothing carries into the next cycle.
///
/// # Why this has to be called, and what happened while it was not
///
/// This function shipped with the 2026-09-07 repair and **had no caller**, and
/// the drain on the other side is reached only through `update_all_roots`,
/// which returns early on an empty pointer map — that is, on every NON-moving
/// cycle. Since the non-moving cycles outnumber the moving ones by roughly
/// forty to one on the workload the repair was written for, the buffer was in
/// practice a process-lifetime accumulator of captures belonging to cycles that
/// never relocated. Two consequences, and both are correctness ones:
///
/// * **the cap silences the repair.** `record_peer_stack_slot` drops a capture
///   once the buffer holds 65536, so once the accumulation saturates, the
///   moving cycle — the only cycle whose captures matter — records nothing.
/// * **ABA.** A capture taken at cycle N carries `orig` = the word's value
///   *then*. Folded at a later cycle M, it is advanced through M's pointer map.
///   If the address was vacated at N, recycled, and moved again at M, the fold
///   computes `cur` for the *new* occupant and the wake write-back stores it
///   into a word that meant the old one — the repair manufacturing exactly the
///   wrong-address read it exists to prevent.
///
/// Clearing at the point that OPENS a pause gives the buffer the per-cycle
/// lifetime the fold already assumes, so a capture is only ever folded against
/// the pointer map of the very cycle that took it.
pub fn clear_peer_stack_slots() {
    let mut g = PEER_STACK_SLOTS.lock();
    if !g.is_empty() {
        PEER_STACK_SLOTS_DISCARDED.fetch_add(g.len() as u64, Ordering::Relaxed);
    }
    g.clear();
}

pub fn record_peer_reg(os_tid: u32, reg: u8, value: usize) {
    if !peer_reg_pairing_enabled() {
        return;
    }
    let mut g = PEER_REG_CAPTURE.lock();
    // Bounded: a runaway capture would change the timing it is measuring.
    if g.len() < 65536 {
        g.push((os_tid, reg, value));
    }
}

/// Drop the previous cycle's capture. Called where the collection begins, so a
/// hit is always attributable to the cycle that relocated.
pub fn clear_peer_reg_capture() {
    if !peer_reg_pairing_enabled() {
        return;
    }
    PEER_REG_CAPTURE.lock().clear();
}

/// Take the capture for comparison against this cycle's pointer map.
pub fn take_peer_reg_capture() -> Vec<(u32, u8, usize)> {
    let mut g = PEER_REG_CAPTURE.lock();
    std::mem::take(&mut *g)
}

/// `CRATONVM_DBG_PEER_REG_PAIRING` — arm the frozen-peer register capture and
/// the post-evacuation comparison against the pointer map.
pub fn peer_reg_pairing_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_PEER_REG_PAIRING").is_some()
    })
}

pub static PEER_DEPTH_ZERO_TOTAL: AtomicU64 = AtomicU64::new(0);

/// The subset of [`PEER_DEPTH_ZERO_TOTAL`] where the process-wide JIT depth
/// read back STRICTLY LESS THAN this thread's own chain length.
///
/// That is not a quiet moment, it is a **provably inconsistent read**: this
/// thread's frames are part of the global count, so `global >= local` holds for
/// any consistent observation. `GLOBAL_JIT_DEPTH` is a striped counter and
/// `peer_jit_depth()` reduces the difference with `saturating_sub`, whose
/// source comment calls zero "the safe reading". It is safe for the
/// subtraction; it is not safe for the CALLER, which reads zero as "no peers to
/// account for" and takes the unguarded path to relocation. Any non-zero
/// reading here is a cycle that relocated behind peers it never counted.
pub static PEER_DEPTH_ZERO_TORN: AtomicU64 = AtomicU64::new(0);

/// The subset of [`PEER_DEPTH_ZERO_TOTAL`] where the global depth was itself
/// zero -- genuinely nobody in compiled code anywhere, the one legitimate way
/// to see no peers. Split out so the legitimate case cannot inflate the
/// suspicious one.
pub static PEER_DEPTH_ZERO_GLOBAL_ZERO: AtomicU64 = AtomicU64::new(0);

/// Record a `peer_depth == 0` observation, classified by whether it is
/// explicable. Counters only -- an `eprintln` here perturbs the very timing
/// that produces the phenomenon (a per-cycle print took moving cycles from 9
/// to 0 on the netty repro).
pub fn note_peer_depth_zero(global: usize, local: usize) {
    PEER_DEPTH_ZERO_TOTAL.fetch_add(1, Ordering::Relaxed);
    if global == 0 {
        PEER_DEPTH_ZERO_GLOBAL_ZERO.fetch_add(1, Ordering::Relaxed);
    } else if global < local {
        PEER_DEPTH_ZERO_TORN.fetch_add(1, Ordering::Relaxed);
    }
}

pub static XT_PASSES_LAST_CYCLE: AtomicU64 = AtomicU64::new(0);
/// Peers frozen and conservatively scanned this cycle.
pub static XT_TAKEN_OVER_LAST_CYCLE: AtomicU64 = AtomicU64::new(0);
/// Peers this cycle could not classify — still running JIT code, roots unseen.
pub static XT_UNCLASSIFIED_LAST_CYCLE: AtomicU64 = AtomicU64::new(0);
/// Conservative roots the take-over passes contributed this cycle.
pub static XT_ROOTS_LAST_CYCLE: AtomicU64 = AtomicU64::new(0);
/// Helper windows found this cycle (blocked peers with JIT frames on stack).
pub static XT_HW_WINDOWS_LAST_CYCLE: AtomicU64 = AtomicU64::new(0);
/// Conservative roots the helper-window pass contributed this cycle.
pub static XT_HW_ROOTS_LAST_CYCLE: AtomicU64 = AtomicU64::new(0);

/// Zero the per-cycle cross-thread coverage. Called once at the top of the
/// stop-the-world take-over, so what the sweep reads describes THIS cycle.
pub fn reset_xt_cycle() {
    XT_PASSES_LAST_CYCLE.store(0, Ordering::Relaxed);
    XT_TAKEN_OVER_LAST_CYCLE.store(0, Ordering::Relaxed);
    XT_UNCLASSIFIED_LAST_CYCLE.store(0, Ordering::Relaxed);
    XT_ROOTS_LAST_CYCLE.store(0, Ordering::Relaxed);
    XT_HW_WINDOWS_LAST_CYCLE.store(0, Ordering::Relaxed);
    XT_HW_ROOTS_LAST_CYCLE.store(0, Ordering::Relaxed);
}

/// Accumulate one take-over pass's outcome into this cycle's totals.
pub fn publish_xt_pass(taken_over: u64, unclassified: u64, roots: u64) {
    XT_PASSES_LAST_CYCLE.fetch_add(1, Ordering::Relaxed);
    XT_TAKEN_OVER_LAST_CYCLE.fetch_add(taken_over, Ordering::Relaxed);
    XT_UNCLASSIFIED_LAST_CYCLE.fetch_add(unclassified, Ordering::Relaxed);
    XT_ROOTS_LAST_CYCLE.fetch_add(roots, Ordering::Relaxed);
}

/// Accumulate the post-barrier helper-window pass's outcome.
pub fn publish_xt_helper_window(windows: u64, roots: u64) {
    XT_HW_WINDOWS_LAST_CYCLE.fetch_add(windows, Ordering::Relaxed);
    XT_HW_ROOTS_LAST_CYCLE.fetch_add(roots, Ordering::Relaxed);
}


/// `(passes, taken_over, unclassified, roots, hw_windows, hw_roots)` for the
/// cycle currently in progress.
pub fn xt_cycle_coverage() -> (u64, u64, u64, u64, u64, u64) {
    (
        XT_PASSES_LAST_CYCLE.load(Ordering::Relaxed),
        XT_TAKEN_OVER_LAST_CYCLE.load(Ordering::Relaxed),
        XT_UNCLASSIFIED_LAST_CYCLE.load(Ordering::Relaxed),
        XT_ROOTS_LAST_CYCLE.load(Ordering::Relaxed),
        XT_HW_WINDOWS_LAST_CYCLE.load(Ordering::Relaxed),
        XT_HW_ROOTS_LAST_CYCLE.load(Ordering::Relaxed),
    )
}

/// Is `addr` a currently-registered Weak/Soft/Phantom referent this cycle?
/// Consulted by the non-moving young sweep for each kept-in-place survivor.
pub fn is_watched_referent(addr: usize) -> bool {
    WATCHED_REFERENTS.with(|s| s.borrow().contains(&addr))
}

/// Snapshot of the watched-referent set, or `None` when it is empty.
///
/// The set lives in a `thread_local!` owned by the collecting thread, so a
/// parallel sweep worker cannot consult it directly (it would see its own,
/// always-empty, copy). The collector snapshots it once before spawning
/// workers; the set is sized by the VM's reference processor, not by the
/// young generation, so the clone is cheap and usually skipped entirely.
pub fn watched_referents_snapshot() -> Option<std::collections::HashSet<usize>> {
    WATCHED_REFERENTS.with(|s| {
        let s = s.borrow();
        if s.is_empty() {
            None
        } else {
            Some(s.clone())
        }
    })
}

// ---------------------------------------------------------------------------
// Root-source attribution hook (diagnostic)
// ---------------------------------------------------------------------------

/// Installed by the VM when `CRATONVM_DBG_ROOT_SOURCE` is on: "which named
/// root source handed the marker this address, this cycle?"
///
/// Lives here for the same reason the quiescence flag does -- the VM crate
/// depends on the GC crate, so the GC cannot call into it, and the collector
/// is where the question gets asked. `vm/src/memory/native_roots.rs` owns the
/// inventory and the per-cycle table; this is only the doorway.
///
/// Answering `None` is meaningful, not a failure: it says NO named source
/// contributed that exact address, so whatever holds it is reachable some
/// other way -- a different finding, wanting a different fix.
static ROOT_SOURCE_HOOK: std::sync::OnceLock<fn(usize) -> Option<&'static str>> =
    std::sync::OnceLock::new();

/// Install the attribution lookup. First call wins; later calls are ignored,
/// so a second VM in-process cannot repoint a live collector's diagnostics.
pub fn install_root_source_hook(f: fn(usize) -> Option<&'static str>) {
    let _ = ROOT_SOURCE_HOOK.set(f);
}

/// Which named root source contributed `addr` this cycle, if the hook is
/// installed and the flag is on.
pub fn root_source_of(addr: usize) -> Option<&'static str> {
    ROOT_SOURCE_HOOK.get().and_then(|f| f(addr))
}

/// Installed by the VM: capture the native return-address chain as
/// `exe`-relative RVAs, ready to paste into `CRATONVM_SYMBOLIZE`.
///
/// The crash handler's `RtlCaptureStackBackTrace` + `exe+RVA` pair symbolizes
/// offline against the matching PDB. That machinery is Windows FFI living in
/// the VM crate, so the collector reaches it through a doorway, exactly as
/// with the root-source lookup above.
///
/// # This hook has NO caller, and on Linux it needs none
///
/// `install_native_rva_hook` is called nowhere in the tree, so [`native_rvas`]
/// returns empty and every caller takes its `Backtrace::force_capture`
/// fallback.
///
/// That is fine, and the claim this comment used to carry -- that
/// `std::backtrace::Backtrace` "is useless in this tree's release profile, fat
/// LTO plus `debug = "line-tables-only"` renders every frame `<unknown>`" --
/// is **false on Linux**. Measured 2026-08-24 on a fat-LTO release build:
/// `report_corpse_read`'s fallback emitted **40 fully symbolized frames with
/// file:line**, naming the offending native four frames up
/// (`native_input_stream_transfer_to` at `zip_streams.rs:1532:17`).
/// `panic = "unwind"` is set for this profile, so `.eh_frame` is emitted and
/// the unwinder walks normally, and `debug = "line-tables-only"` is precisely
/// what a backtrace needs.
///
/// The warning cost time in the other direction: believing it, a session went
/// to `gdb` for an answer the report had already printed, and then reported
/// the in-process capture as broken. On Linux, read the log lines AFTER the
/// `backtrace=` field -- `Display` is multi-line, so a one-line `grep` shows
/// the first frame and nothing else, which is exactly what "one frame, useless"
/// looks like.
///
/// Whether the MSVC build still needs the RVA path is untested here.
static NATIVE_RVA_HOOK: std::sync::OnceLock<fn() -> Vec<usize>> = std::sync::OnceLock::new();

/// Install the RVA capture. First call wins.
pub fn install_native_rva_hook(f: fn() -> Vec<usize>) {
    let _ = NATIVE_RVA_HOOK.set(f);
}

/// `exe`-relative return addresses for the current call chain, innermost
/// first. Empty when no hook is installed or the platform has none.
pub fn native_rvas() -> Vec<usize> {
    NATIVE_RVA_HOOK.get().map(|f| f()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vacated ledger is process-global and its gate is a process-global
    /// byte, so the tests that arm it must not run beside each other.
    static VACATED_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    fn pointer_map_of(pairs: &[(usize, usize)]) -> cratonvm_types::PointerMap {
        let mut m = cratonvm_types::PointerMap::default();
        for (k, v) in pairs {
            m.insert(*k, *v);
        }
        m
    }

    /// The claim the whole instrument rests on: an address the allocator has
    /// re-issued is not evidence of anything.
    ///
    /// Until 2026-09-08 the only callers of `note_allocated` were ZGC's, so on
    /// `--XX:UseGc Generational` nothing ever removed an entry and the mutator
    /// bump-allocated straight back into the semispace the previous cycle had
    /// vacated. Every detector then reported freshly allocated young objects as
    /// stale references -- the eight `stack[0]` "the frame remap did not reach
    /// this slot" reports the BindableTests moving-collector page was written
    /// around, whose producer backtrace is `Anewarray` pushing the array
    /// `gc_alloc_array` had returned two statements earlier.
    #[test]
    fn vacated_ledger_forgets_a_re_issued_address() {
        let _g = VACATED_TEST_LOCK.lock();
        set_vacated_frames_enabled_for_test(true);
        reset_vacated_ledger_for_test();

        record_vacated(&pointer_map_of(&[(0x1000, 0x9000)]), 7);
        assert_eq!(
            was_vacated_on(0x1000),
            Some((0x9000, 7)),
            "a moved-from address must be in the ledger with the cycle that moved it"
        );

        note_allocated(&[0x1000]);
        assert_eq!(
            was_vacated(0x1000),
            None,
            "an address the allocator re-issued is no longer evidence of a stale reference"
        );

        reset_vacated_ledger_for_test();
        set_vacated_frames_enabled_for_test(false);
    }

    /// A TLAB chunk is bump-allocated from without any further call into the
    /// heap, so the per-object door cannot see the objects inside it. The
    /// range door is the only one that can, and `VmHeap::refill_tlab` is the
    /// single chokepoint every backend's TLAB comes through.
    #[test]
    fn vacated_ledger_forgets_a_whole_re_issued_tlab_chunk() {
        let _g = VACATED_TEST_LOCK.lock();
        set_vacated_frames_enabled_for_test(true);
        reset_vacated_ledger_for_test();

        record_vacated(
            &pointer_map_of(&[(0x2000, 0xa000), (0x2100, 0xa100), (0x3000, 0xb000)]),
            11,
        );
        note_allocated_range(0x2000, 0x2800);

        assert_eq!(was_vacated(0x2000), None, "chunk start must be forgotten");
        assert_eq!(was_vacated(0x2100), None, "chunk interior must be forgotten");
        assert_eq!(
            was_vacated_on(0x3000),
            Some((0xb000, 11)),
            "an address OUTSIDE the chunk must survive -- the purge is a range, not a clear"
        );

        reset_vacated_ledger_for_test();
        set_vacated_frames_enabled_for_test(false);
    }

    /// The ledger accumulates across cycles on purpose (a stale reference is
    /// not necessarily consumed before the next collection), so "vacated" alone
    /// carries no date. `was_vacated_on` is what lets a report compare the
    /// vacating cycle against the thread's `last_heal_collection` instead of
    /// against the current collection count, which at a safepoint is always
    /// equal to it and therefore proves nothing.
    #[test]
    fn vacated_ledger_dates_each_entry_by_its_own_cycle() {
        let _g = VACATED_TEST_LOCK.lock();
        set_vacated_frames_enabled_for_test(true);
        reset_vacated_ledger_for_test();

        record_vacated(&pointer_map_of(&[(0x4000, 0xc000)]), 3);
        record_vacated(&pointer_map_of(&[(0x5000, 0xd000)]), 900);

        assert_eq!(was_vacated_on(0x4000), Some((0xc000, 3)));
        assert_eq!(was_vacated_on(0x5000), Some((0xd000, 900)));

        reset_vacated_ledger_for_test();
        set_vacated_frames_enabled_for_test(false);
    }

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

    /// The collector must follow the value the codegen side publishes, not its
    /// own read of `gc_flags()`. This interlock is what makes a default flip on
    /// the codegen side safe — and what makes a flip of
    /// `cratonvm_types::flags::DEFAULT_MOVING_YOUNG` *alone* harmless rather
    /// than corrupting, while `jit/src/x64.rs` still parses the raw variable.
    #[test]
    fn published_gate_wins_over_the_flags_default() {
        // Fresh test thread: unpublished, so the flags value applies.
        assert_eq!(moving_young_enabled(), gc_flags().moving_young);
        publish_moving_young_enabled(true);
        assert!(
            moving_young_enabled(),
            "the collector must honour a codegen-published moving-young decision",
        );
        publish_moving_young_enabled(false);
        assert!(
            !moving_young_enabled(),
            "and it must honour a codegen-published REFUSAL even if the typed \
             config says moving-young is on — the codegen is the side that has \
             to emit the rewritable root map",
        );
    }

    /// An explicit `System.gc()` takes `divert_non_moving`'s `explicit_full_gc`
    /// arm, which names no moving-young condition — so the side-table follow
    /// must hold on BOTH published values of the gate.
    ///
    /// This is the regression test for the third recurrence of
    /// `TestDefaultInstanceManager.testClassUnloading`: the predicate used to
    /// factor `!moving_young_enabled()` across this arm too, so flipping
    /// `DEFAULT_MOVING_YOUNG` to `true` disarmed
    /// `VmHeap::mirror_pin_deferrable`'s young-mirror deferral everywhere at
    /// once, with no test failing and the fix still sitting in the tree. Asserting
    /// both gate values is the point — a one-sided assertion would have passed
    /// before the flip and after it.
    #[test]
    fn explicit_full_gc_follows_side_tables_on_either_moving_young_gate() {
        for on in [false, true] {
            publish_moving_young_enabled(on);
            assert!(
                !young_marker_follows_side_tables(),
                "no System.gc() pending and no JIT frame: with moving-young={on} \
                 this thread cannot promise the non-moving young marker",
            );
            request_major_gc();
            assert!(
                young_marker_follows_side_tables(),
                "an explicit System.gc() diverts to the non-moving young cycle on \
                 its own (`divert_non_moving`'s `explicit_full_gc` term), so a \
                 young class mirror is reachable through `mirror_pin` and must not \
                 be rooted unconditionally — moving-young={on} is irrelevant here",
            );
            assert!(take_major_gc_request());
        }
    }

    /// The other usable arm, and the one that DOES carry the guard: a live JIT
    /// frame forces the non-moving sweep only while moving-young is off
    /// (`has_conservative_roots && !moving_young`).
    #[test]
    fn conservative_jit_roots_follow_side_tables_only_without_moving_young() {
        let _ = enter();
        assert!(is_active());

        publish_moving_young_enabled(false);
        assert!(
            young_marker_follows_side_tables(),
            "conservative JIT roots divert to the non-moving sweep when \
             moving-young is off",
        );

        publish_moving_young_enabled(true);
        assert!(
            !young_marker_follows_side_tables(),
            "with moving-young on, a live JIT frame no longer forces the \
             non-moving sweep — the cycle may relocate, and the moving closure \
             seeds strictly from the direct root set",
        );

        let _ = leave();
    }

    #[test]
    fn coverage_cycle_resets_verdict_and_reason() {
        begin_moving_young_coverage_cycle();
        assert!(!moving_young_coverage_incomplete());
        assert_eq!(moving_young_incomplete_reason(), incomplete_reason::NONE);

        mark_moving_young_coverage_incomplete_because(incomplete_reason::UNREGISTERED_JIT_FRAME);
        assert!(moving_young_coverage_incomplete());
        assert_eq!(
            moving_young_incomplete_reason(),
            incomplete_reason::UNREGISTERED_JIT_FRAME
        );

        // First reason of a cycle wins — later ones are consequences of it.
        mark_moving_young_coverage_incomplete_because(incomplete_reason::XT_TAKEOVER);
        assert_eq!(
            moving_young_incomplete_reason(),
            incomplete_reason::UNREGISTERED_JIT_FRAME
        );

        begin_moving_young_coverage_cycle();
        assert!(!moving_young_coverage_incomplete());
        assert_eq!(moving_young_incomplete_reason(), incomplete_reason::NONE);
    }

    /// HIB-GCOVERHEAD-HALFFULL.1 regression.
    ///
    /// `unrewritable_peer_state` must stay STRICTLY narrower than
    /// `moving_young_coverage_incomplete`. The two were the same flag when the
    /// promotion gate was written; once moving-young became the default, the
    /// wide verdict was set on essentially every JIT-active cycle, and reading
    /// it as "un-rewritable peer state" switched off selective promotion
    /// VM-wide — the young generation lost its only drain and the VM raised
    /// `OutOfMemoryError` on a 49%-full heap.
    ///
    /// Asserted as a DECISION, not a side effect: an end-to-end "does the heap
    /// still OOM" test cannot distinguish this from any other allocation defect,
    /// and would pass again the moment some unrelated change made young big
    /// enough to hide it.
    #[test]
    fn unrewritable_peer_state_is_narrower_than_the_coverage_verdict() {
        // The ordinary case, and the one that regressed: a compiled frame on
        // THIS thread whose oop map is unproven. Relocation is off; promotion
        // must not be, because the conservative scan still covers that frame and
        // selective promotion pins its slot values.
        for reason in [
            incomplete_reason::UNPUBLISHED_FRAME_OOP,
            incomplete_reason::MISSING_EXACT_RBP,
            incomplete_reason::ACTIVE_FRAME_MAP,
            incomplete_reason::PARENT_FRAME_MAP,
            incomplete_reason::NO_PRECISE_MAP,
            incomplete_reason::UNREGISTERED_JIT_FRAME,
            incomplete_reason::OSR_SHADOW,
            incomplete_reason::UNBOUNDED_FRAME_BAND,
            incomplete_reason::FOREIGN_INNERMOST_RBP,
            incomplete_reason::JIT_RELOCATION_UNSUPPORTED,
            // "some peer is in compiled code" — a cooperatively parked peer,
            // covered by its own deposited root snapshot.
            incomplete_reason::CROSS_THREAD_JIT_PEER,
        ] {
            begin_moving_young_coverage_cycle();
            mark_moving_young_coverage_incomplete_because(reason);
            assert!(
                moving_young_coverage_incomplete(),
                "{} must still divert the COPYING collector",
                incomplete_reason::label(reason),
            );
            assert!(
                !unrewritable_peer_state(),
                "{} must NOT disable selective promotion — it describes a frame \
                 the conservative scan covers, not state no one can rewrite. \
                 Widening this gate is HIB-GCOVERHEAD-HALFFULL.1: the young \
                 generation loses its only drain and the VM OOMs on a half-empty \
                 heap.",
                incomplete_reason::label(reason),
            );
        }

        // The two the 2026-07-03 gate was actually written for: a peer excused
        // from the STW barrier, which therefore never re-reads its own
        // registers.
        for reason in [
            incomplete_reason::XT_TAKEOVER,
            incomplete_reason::XT_HELPER_WINDOW,
        ] {
            begin_moving_young_coverage_cycle();
            mark_moving_young_coverage_incomplete_because(reason);
            assert!(
                unrewritable_peer_state(),
                "{} MUST disable selective promotion: a frozen peer's register \
                 can hold only a derived/interior pointer, which pin-by-value \
                 does not protect",
                incomplete_reason::label(reason),
            );
        }

        // Set by a later reason even when an earlier one already claimed the
        // first-wins reason slot.
        begin_moving_young_coverage_cycle();
        mark_moving_young_coverage_incomplete_because(incomplete_reason::UNPUBLISHED_FRAME_OOP);
        assert!(!unrewritable_peer_state());
        mark_moving_young_coverage_incomplete_because(incomplete_reason::XT_TAKEOVER);
        assert_eq!(
            moving_young_incomplete_reason(),
            incomplete_reason::UNPUBLISHED_FRAME_OOP,
            "the recorded reason stays first-wins",
        );
        assert!(
            unrewritable_peer_state(),
            "…but the peer-state verdict must not be first-wins: it is a safety \
             gate, so any cross-thread obligation in the cycle arms it",
        );

        // And the standalone marker (the xt-takeover call site) plus the reset.
        begin_moving_young_coverage_cycle();
        assert!(!unrewritable_peer_state());
        mark_unrewritable_peer_state();
        assert!(unrewritable_peer_state());
        begin_moving_young_coverage_cycle();
        assert!(
            !unrewritable_peer_state(),
            "the verdict is per-cycle and must be cleared with the others",
        );
    }

    #[test]
    fn every_incomplete_reason_has_a_label() {
        for code in incomplete_reason::NONE..incomplete_reason::COUNT {
            assert_ne!(
                incomplete_reason::label(code),
                "unknown",
                "reason code {code} needs a label for the warn-level fallback diagnostic — \
                 and `incomplete_reason::COUNT` must match the highest defined code + 1, \
                 because it sizes the per-reason fallback histogram",
            );
        }
        assert_eq!(
            incomplete_reason::label(incomplete_reason::COUNT),
            "unknown",
            "COUNT must be one PAST the last defined reason",
        );
    }

    /// A repeat of the 2026-07-01 "validated" run — which declared moving-young
    /// working after executing ZERO moving cycles — must be impossible to
    /// reproduce silently. The pair (cycle count, per-reason fallback
    /// histogram) is what makes that so, and the histogram must attribute the
    /// fallback to the obligation that actually forced it.
    #[test]
    fn fallback_histogram_attributes_the_blocking_obligation() {
        let before = moving_young_fallback_reason_counts();
        assert_eq!(moving_young_cycle_count(), 0, "fresh test thread");

        begin_moving_young_coverage_cycle();
        mark_moving_young_coverage_incomplete_because(incomplete_reason::UNPUBLISHED_FRAME_OOP);
        // A later, consequential reason must NOT steal the attribution.
        mark_moving_young_coverage_incomplete_because(incomplete_reason::CROSS_THREAD_JIT_PEER);
        record_moving_young_coverage_fallback();

        let after = moving_young_fallback_reason_counts();
        assert_eq!(
            after[incomplete_reason::UNPUBLISHED_FRAME_OOP],
            before[incomplete_reason::UNPUBLISHED_FRAME_OOP] + 1,
        );
        assert_eq!(
            after[incomplete_reason::CROSS_THREAD_JIT_PEER],
            before[incomplete_reason::CROSS_THREAD_JIT_PEER],
            "only the FIRST reason of a cycle is the one that forced the decision",
        );
        assert_eq!(
            moving_young_cycle_count(),
            0,
            "a fallback is the OPPOSITE of a moving cycle; the two counters must \
             never both be bumped for one collection",
        );
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

// ---------------------------------------------------------------------------
// VACATED-ADDRESS LEDGER (`CRATONVM_DBG_VACATED_FRAMES`)
// ---------------------------------------------------------------------------
//
// "A live object was relocated and one holder was never rewritten" is a verdict
// the ZGC corpse ledger can produce (see `ZgcRealHeap::corpse_lookup`) — but it
// produces it at the READER, an unbounded number of collections after the fact,
// and by then the holder is whatever frame happens to be executing. What is
// missing is the other end: WHICH frame slot still named a vacated address at
// the first safepoint after the collection that vacated it.
//
// This is that ledger. It stores the KEY set of one collection's pointer map —
// every address the collector moved an object away from — and
// `reclaim_guard::audit_thread_frames` tests each live frame slot against it at
// the next safepoint. A hit names the thread, the method, the pc and the slot,
// which is what separates "the frame remap missed this slot" from "something
// re-introduced the address afterwards" (they report on different collections).
//
// Flag-gated because the set is one entry per relocated object — a compacting
// cycle under GC stress moves hundreds of thousands — and because it answers a
// question only a run that is already suspected of this defect needs asked.

/// `(vacated -> where the object went, every destination the slide wrote to)`.
///
/// The destination set is what keeps this instrument honest. An address can be
/// BOTH a source and a destination in one compacting cycle: survivors slide
/// DOWN into the space dead objects vacated, so `ThreadPoolExecutor.runWorker`
/// holding a perfectly valid `Thread` that happens to live at an address this
/// cycle also moved something away from is not a defect — and reporting it as
/// one is how an over-approximate instrument manufactures its own finding.
///
/// The map's value carries the COLLECTION the address was vacated on as well as
/// the destination. The ledger accumulates across cycles (see
/// [`record_vacated`]), so "vacated" alone says nothing about WHEN — and the
/// report `reclaim_guard::audit_thread_frames` prints off it used to compare
/// the thread's `last_heal_collection` against the CURRENT collection count,
/// which is always equal at a safepoint and therefore proved nothing. With the
/// vacating cycle in hand the comparison is the real one: `vacated_on <=
/// thread_last_heal` means the remap ran for that thread on that cycle and
/// missed the slot; `vacated_on > thread_last_heal` means the thread was never
/// healed for it.
type VacatedLedger = (
    rustc_hash::FxHashMap<usize, (usize, u64)>,
    rustc_hash::FxHashSet<usize>,
);

static VACATED_ADDRS: parking_lot::RwLock<Option<VacatedLedger>> = parking_lot::RwLock::new(None);

/// `CRATONVM_DBG_VACATED_FRAMES=1` — arm the vacated-address ledger.
///
/// Interpreter hot paths read this on every operand-stack push and every heap
/// accessor (`load_and_forward`, `get_field`, ...). A `OnceLock` is an acquire
/// load plus an out-of-line init check; this is one relaxed byte load with the
/// init on a cold path. 0 = unset, 1 = off, 2 = on.
static VACATED_FRAMES_STATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

#[inline]
pub fn vacated_frames_enabled() -> bool {
    let s = VACATED_FRAMES_STATE.load(std::sync::atomic::Ordering::Relaxed);
    if s != 0 {
        return s == 2;
    }
    vacated_frames_enabled_init(&VACATED_FRAMES_STATE)
}

/// Test-only arming door, so the ledger's re-issue accounting can be exercised
/// without an environment variable set before the process started.
#[cfg(test)]
pub(crate) fn set_vacated_frames_enabled_for_test(on: bool) {
    VACATED_FRAMES_STATE.store(if on { 2 } else { 1 }, std::sync::atomic::Ordering::Relaxed);
}

/// Test-only: drop the ledger so a test starts from a known state.
#[cfg(test)]
pub(crate) fn reset_vacated_ledger_for_test() {
    *VACATED_ADDRS.write() = None;
}

#[cold]
#[inline(never)]
fn vacated_frames_enabled_init(state: &std::sync::atomic::AtomicU8) -> bool {
    let on = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_VACATED_FRAMES").is_some();
    state.store(if on { 2 } else { 1 }, std::sync::atomic::Ordering::Relaxed);
    on
}

/// Replace the ledger with THIS collection's vacated addresses (the pointer
/// map's keys). One collection at a time, deliberately: the question is "did a
/// slot survive the collection that moved its object", and carrying older
/// cycles would answer a different, much noisier one.
///
/// **The ledger is only exact because [`note_allocated`] empties it as the
/// allocator re-issues the space.** A compacting cycle zeroes what it vacated
/// and hands it straight back, so a reference to a vacated address is
/// ambiguous the moment a NEW object is allocated there — and the first
/// version of this instrument, which did not track that, reported eight
/// perfectly valid frame slots per run (`MVTable.updateRows` local[4] holding
/// exactly the `SessionLocal$Savepoint` its `astore 4` had put there). With
/// re-issued addresses removed, a hit is unambiguous: nothing has been
/// allocated at that address since the collector moved its occupant away.
/// Relocating collections this process has completed (every cycle that
/// produced a non-empty pointer map).
///
/// Paired with [`note_pointer_map_applied`] it answers the question a stale
/// register otherwise leaves open: was this thread ever handed the map it is
/// missing? A thread whose last applied cycle EQUALS this counter was rewritten
/// and is stale anyway -- a hole in the rewrite. One whose number is smaller
/// never got the map at all, which is a different defect with a different fix.
pub static RELOCATING_CYCLES: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// `(relocating-cycle number, path)` of the last pointer map THIS thread
    /// applied to itself. Path: 1 = the stop-the-world resume
    /// (`apply_pointer_map_to_thread`), 2 = the ordinary blocked-region wake
    /// (`check_post_block_gc_refs`), 3 = the leaked-region fallback
    /// (`apply_pending_blocked_fixups`).
    ///
    /// Read from the fatal-signal handler, which runs on the faulting thread,
    /// so a plain thread-local `Cell` is the one storage class that is both
    /// correct and reachable there.
    static LAST_MAP_APPLIED: std::cell::Cell<(u64, u8)> = const { std::cell::Cell::new((0, 0)) };
}

/// Record that this thread has just applied a relocation pointer map. See
/// [`LAST_MAP_APPLIED`].
pub fn note_pointer_map_applied(path: u8) {
    let n = RELOCATING_CYCLES.load(Ordering::Relaxed);
    let _ = LAST_MAP_APPLIED.try_with(|c| c.set((n, path)));
}

/// `(cycle, path)` for this thread; `(0, 0)` if it never applied one.
pub fn last_pointer_map_applied() -> (u64, u8) {
    LAST_MAP_APPLIED.try_with(|c| c.get()).unwrap_or((0, 0))
}

pub fn record_vacated(pointer_map: &cratonvm_types::PointerMap, collection: u64) {
    if !vacated_frames_enabled() {
        return;
    }
    let to: rustc_hash::FxHashSet<usize> = pointer_map.values().copied().collect();
    let mut g = VACATED_ADDRS.write();
    let (from, dests) = g.get_or_insert_with(Default::default);
    // ACCUMULATE across collections rather than replace. A stale reference is
    // not necessarily consumed before the next cycle, and a ledger that only
    // knew the last one answered "not vacated" for every older one — which
    // reads exactly like "no defect". Entries leave only when the allocator
    // re-issues the address (`note_allocated`), so the set stays bounded by the
    // arena and never lies in the other direction either.
    for (k, v) in pointer_map.iter() {
        // A source this cycle also wrote a survivor TO is ambiguous: a slot
        // naming it may legitimately hold that survivor.
        if to.contains(k) {
            from.remove(k);
            continue;
        }
        from.insert(*k, (*v, collection));
    }
    // A destination is a live object's base now, so anything the ledger still
    // held for it is stale bookkeeping, not a stale reference.
    for d in &to {
        from.remove(d);
    }
    *dests = to;
}

/// `vacated address -> (where the object went, its class there)`, kept for the
/// whole run.
///
/// # Why a SECOND ledger, and why this one keeps history
///
/// The exact ledger above is exact precisely because [`note_allocated`] drops
/// an address the instant it is re-issued — and that is why every use-site
/// detector built on it reports ZERO on a failing run. A stale holder is
/// INVISIBLE until re-issue (until then it reads the zeroed corpse and nothing
/// looks wrong) and the exact ledger has forgotten the address by the time the
/// damage becomes visible. The two windows do not overlap.
///
/// This one keeps the history, and uses the CLASS as the discriminator: if the
/// object now at the address is not the class of the object that moved away,
/// the holder is naming the wrong object. Equal classes are declined rather
/// than guessed — a same-class re-issue is real but indistinguishable here, and
/// guessing is what made the first vacated-frames instrument manufacture eight
/// findings a run.
///
/// Bounded and flag-gated: one entry per relocated object is far too much to
/// carry on a production run.
static MOVED_HISTORY: parking_lot::RwLock<Option<rustc_hash::FxHashMap<usize, (usize, u32)>>> =
    parking_lot::RwLock::new(None);

const MOVED_HISTORY_MAX: usize = 2_000_000;

/// Record one slide's `from -> (to, class at to)` pairs.
pub fn record_moved_history(pairs: &[(usize, usize, u32)]) {
    if !vacated_frames_enabled() {
        return;
    }
    let mut g = MOVED_HISTORY.write();
    let map = g.get_or_insert_with(Default::default);
    if map.len() + pairs.len() > MOVED_HISTORY_MAX {
        map.clear();
    }
    for (from, to, class_at_to) in pairs {
        map.insert(*from, (*to, *class_at_to));
    }
}

/// Is `addr` a reference to an object the collector moved away, whose space has
/// since been handed out to an object of a DIFFERENT class?
///
/// Returns `(moved_to, class_at_moved_to, class_at_addr)`. Reads the class id
/// straight out of the header at `addr` — it is at offset 0 by the layout
/// contract every JIT type guard also relies on — so this is callable from any
/// site that has a reference and no heap handle.
pub fn stale_use_verdict(addr: usize) -> Option<(usize, u32, u32)> {
    if !vacated_frames_enabled() || addr == 0 || addr % 8 != 0 {
        return None;
    }
    let (to, class_at_to) = {
        let g = MOVED_HISTORY.read();
        *g.as_ref()?.get(&addr)?
    };
    // SAFETY: `addr` is an address a live reference names and the caller is
    // about to use it as an object; the first four header bytes are mapped
    // managed memory whatever they contain.
    let here = unsafe { std::ptr::read_unaligned(addr as *const u32) };
    (here != class_at_to).then_some((to, class_at_to, here))
}

/// Report a USE of such a reference, with the Rust caller chain — the one thing
/// every other instrument in this family has been unable to say.
#[cold]
pub fn report_stale_use(
    addr: usize,
    moved_to: usize,
    class_at_moved_to: u32,
    class_at_addr: u32,
    site: &'static str,
) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 8 {
        return;
    }
    tracing::error!(
        target: "cratonvm::gc::guard",
        obj = format!("{addr:#x}"),
        moved_to = format!("{moved_to:#x}"),
        class_at_moved_to,
        class_at_addr,
        site,
        backtrace = %std::backtrace::Backtrace::force_capture(),
        "a STALE reference is being USED: the collector moved this object to `moved_to`, the          allocator has since re-issued the address, and the object now there is of a different          class. The backtrace names the VM code still holding it."
    );
}

/// [`stale_use_verdict`] + [`report_stale_use`], for a use site that only wants
/// one call.
#[inline(always)]
pub fn check_stale_use(addr: usize, site: &'static str) {
    if !vacated_frames_enabled() {
        return;
    }
    if let Some((to, cto, chere)) = stale_use_verdict(addr) {
        report_stale_use(addr, to, cto, chere, site);
    }
}

/// Addresses the per-bci local-liveness filter kept OUT of a root snapshot,
/// with the frame that held them.
///
/// `CRATONVM_DBG_VACATED_FRAMES` only. The filter's contract is that a slot it
/// reports dead can never be read again under bytecode semantics — so if an
/// address it dropped later turns up as a failing receiver, the analysis was
/// wrong about that slot, and this names the method and the slot to look at.
/// Bounded; oldest entries are simply overwritten.
///
/// # Why the collection number is part of the value
///
/// The map is keyed by ADDRESS, and the allocator re-serves addresses. On a
/// workload that recycles the front of a semispace thousands of times — any
/// `CRATONVM_DBG_GC_STRESS` run — a hit says "SOME object at this address was
/// filtered here", which is not the claim `vm::memory::reclaim_guard` prints
/// off it ("The filter guarantees such a slot is never read again; it was").
/// It printed exactly that about a 1200-cycles-stale entry on 2026-09-08,
/// while `CRATONVM_NO_LOCAL_LIVENESS=1` reproduced the failure the entry was
/// being blamed for — see
/// `docs/internal/springboot/bindabletests-moving-young-leaves-a-frame-slot-unremapped-20260908.md`.
/// Carrying the collection index lets the reporter print the entry's age beside
/// the claim, so a stale attribution can be discounted instead of acted on.
static LIVENESS_FILTERED: parking_lot::RwLock<Option<rustc_hash::FxHashMap<usize, (String, u64)>>> =
    parking_lot::RwLock::new(None);

const LIVENESS_FILTERED_MAX: usize = 8192;

/// Record that `addr` was in `where_` and the liveness filter dropped it, on
/// heap collection `collection`.
pub fn note_liveness_filtered(addr: usize, collection: u64, where_: impl FnOnce() -> String) {
    if !vacated_frames_enabled() {
        return;
    }
    let mut g = LIVENESS_FILTERED.write();
    let map = g.get_or_insert_with(Default::default);
    if map.len() >= LIVENESS_FILTERED_MAX {
        map.clear();
    }
    map.insert(addr, (where_(), collection));
}

/// Was `addr` dropped from a root snapshot by the liveness filter, where, and
/// on which collection? Print the collection beside the current one — a bare
/// hit is a lead, not a verdict (see the type's doc).
pub fn liveness_filtered_at(addr: usize) -> Option<(String, u64)> {
    if !vacated_frames_enabled() {
        return None;
    }
    LIVENESS_FILTERED.read().as_ref()?.get(&addr).cloned()
}

/// Report a heap access whose RECEIVER is an address this collector moved an
/// object away from, with the Rust caller chain.
///
/// A stale receiver is worse than a stale value: every field read off it
/// returns whatever now occupies the memory, which is a perfectly valid object
/// of an unrelated class. The value that reaches the operand stack therefore
/// looks clean to every other instrument, and only the `checkcast` one
/// instruction later disagrees.
#[inline(always)]
pub fn report_vacated_receiver(addr: usize, site: &'static str) {
    if !vacated_frames_enabled() {
        return;
    }
    if let Some(moved_to) = was_vacated(addr) {
        report_vacated_receiver_cold(addr, moved_to, site);
    }
}

#[cold]
fn report_vacated_receiver_cold(addr: usize, moved_to: usize, site: &'static str) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 12 {
        return;
    }
    tracing::error!(
        target: "cratonvm::gc::guard",
        obj = format!("{addr:#x}"),
        moved_to = format!("{moved_to:#x}"),
        site,
        backtrace = %std::backtrace::Backtrace::force_capture(),
        "a heap access RECEIVER is an address the collector moved an object away from —          every field read through it returns whatever now occupies that memory. The          backtrace names the VM code holding it."
    );
}

/// Forget every address in `addrs` — the allocator has re-issued it, so a
/// reference to it is no longer evidence of anything. Called from the
/// allocation paths; a no-op unless the ledger is armed.
pub fn note_allocated(addrs: &[usize]) {
    if !vacated_frames_enabled() {
        return;
    }
    let mut g = VACATED_ADDRS.write();
    let Some((from, _to)) = g.as_mut() else {
        return;
    };
    if from.is_empty() {
        return;
    }
    for a in addrs {
        from.remove(a);
    }
}

/// Forget every ledger entry inside `[lo, hi)` — the allocator has just handed
/// that whole span out as a TLAB chunk, so every address in it is about to be
/// re-issued.
///
/// # Why a RANGE, and why this is what made the instrument honest
///
/// [`note_allocated`] is the per-object door, and until 2026-09-08 the ONLY
/// callers of it were ZGC's (`zgc/arena_tlab.rs`, `zgc/vm_tlab.rs`, `zgc.rs`).
/// Under `--XX:UseGc Generational` — the configuration both BindableTests pages
/// were written against — nothing ever removed an entry, and the mutator
/// bump-allocates straight back into the semispace the previous cycle vacated.
/// The ledger therefore answered "vacated" for every FRESHLY ALLOCATED object
/// in the young generation, and every detector built on it
/// (`ValueStack::check_vacated_push`, `reclaim_guard::audit_thread_frames`,
/// `VmHeap::note_dead_base_deref`, `load_and_forward`) reported the allocation
/// itself as a stale reference. That is the whole content of the "eight
/// `stack[0]` reports in one run" table in
/// `docs/internal/springboot/bindabletests-moving-young-leaves-a-frame-slot-unremapped-20260908.md`:
/// the producer backtrace on every one of them is `Anewarray`'s
/// `push(Value::Object(Some(arr)))`, two statements after `gc_alloc_array`
/// returned `arr`, with no collection in between.
///
/// A TLAB chunk is handed out as one span and then bump-allocated from without
/// any further call into the heap, so the per-object door cannot see those
/// objects at all — the range door is the only one that can. Purging the whole
/// chunk at refill is also strictly conservative in the safe direction: it can
/// only ever DROP a claim, never manufacture one.
pub fn note_allocated_range(lo: usize, hi: usize) {
    if !vacated_frames_enabled() || hi <= lo {
        return;
    }
    let mut g = VACATED_ADDRS.write();
    let Some((from, _to)) = g.as_mut() else {
        return;
    };
    if from.is_empty() {
        return;
    }
    from.retain(|k, _| *k < lo || *k >= hi);
}

/// Did the last recorded collection move an object away from `addr`, and if so
/// where to?
///
/// `None` when the address was not a source, and — deliberately — also when it
/// was a source but is ALSO a destination this cycle wrote a survivor to: a
/// slot naming that address may legitimately hold the survivor.
pub fn was_vacated(addr: usize) -> Option<usize> {
    was_vacated_on(addr).map(|(to, _)| to)
}

/// [`was_vacated`] plus the COLLECTION the address was vacated on.
///
/// The ledger accumulates, so an entry can be arbitrarily many cycles old; a
/// report that does not print this cannot tell "the remap missed this slot"
/// from "the thread was never healed for that cycle". See [`VacatedLedger`].
pub fn was_vacated_on(addr: usize) -> Option<(usize, u64)> {
    if !vacated_frames_enabled() {
        return None;
    }
    let g = VACATED_ADDRS.read();
    let (from, dests) = g.as_ref()?;
    if dests.contains(&addr) {
        return None;
    }
    from.get(&addr).copied()
}

/// [`was_vacated`] for a SIGNAL HANDLER: never blocks.
///
/// The fatal-signal reporter runs on the faulting thread, which may itself hold
/// the ledger's lock -- a blocking `read()` there turns a diagnosable crash into
/// a hang, and a hang produces no report at all. `try_read` answers "cannot
/// tell" instead, and the caller prints that rather than pretending the register
/// was clean.
pub fn was_vacated_try(addr: usize) -> Result<Option<usize>, ()> {
    if !vacated_frames_enabled() {
        return Ok(None);
    }
    let Some(g) = VACATED_ADDRS.try_read() else {
        return Err(());
    };
    let Some((from, dests)) = g.as_ref() else {
        return Ok(None);
    };
    if dests.contains(&addr) {
        return Ok(None);
    }
    // The ledger's value carries the vacating COLLECTION as well as the
    // destination (see `VacatedLedger`); a signal handler only wants the
    // address it should have been reading.
    Ok(from.get(&addr).map(|&(to, _)| to))
}

// ---------------------------------------------------------------------------
// Per-OS-thread published JIT depth, and the pinned-peer depth ledger.
// ---------------------------------------------------------------------------
//
// `refresh_moving_young_coverage_for_collection` accounts for cross-thread JIT
// coverage with a DEPTH comparison (`proven >= peer`), because
// `GLOBAL_JIT_DEPTH` is the only process-wide view of compiled frames and it is
// a depth, not a set of threads. That works for a cooperatively-parked peer,
// which deposits its own depth into `peer_proven_jit_depth` at its park.
//
// A BLOCKED peer never reaches that park, so it deposits nothing -- and until
// now the resulting shortfall refused the cycle. That is the
// `cross-thread-jit-peer` term, 448 of the 877 relocation refusals on
// `TestCachedQueryResults`.
//
// Since 2026-09-02 `helper_window_pass` PINS such a peer: it freezes the
// thread, scans its whole register file and its whole `[rsp, stack_base)` band
// conservatively, and pins every heap address it finds for the rest of the
// cycle. A pinned peer's objects cannot move, so its frames need no
// rewritability proof -- the obligation is discharged by immobility instead of
// by proof.
//
// Turning that into an accounting entry needs the peer's DEPTH, and the depth
// lives in a thread-local (`JIT_ENTRY_CHAIN`) the initiator cannot read. Hence
// this registry: each thread publishes its own depth into a slot keyed by OS
// tid, and the initiator reads the slot of a peer it has just frozen.
//
// Why publish from the JIT push/pop and not from the blocked-region
// transition, which is far colder: `in_blocked_region` is raised at several
// sites (`mark_native_thread_blocked`, the `BlockedGuard`, the JNI paths), and
// a site that raised it without publishing would leave a STALE depth behind.
// Stale-too-small merely under-credits and refuses a cycle it could have run;
// stale-too-LARGE credits depth that nothing pinned, which is the unsound
// direction -- it would let the collector move an object a peer's unscanned
// frame still names. Publishing on every chain mutation cannot go stale.

static PER_TID_JIT_DEPTH: std::sync::OnceLock<
    std::sync::RwLock<
        std::collections::HashMap<u32, std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    >,
> = std::sync::OnceLock::new();

fn per_tid_jit_depth()
-> &'static std::sync::RwLock<
    std::collections::HashMap<u32, std::sync::Arc<std::sync::atomic::AtomicUsize>>,
> {
    PER_TID_JIT_DEPTH.get_or_init(|| std::sync::RwLock::new(std::collections::HashMap::new()))
}

/// Hand the calling thread the slot it should publish its JIT depth into.
///
/// Called once per thread (the caller caches the `Arc` in TLS and stores
/// through it on every chain mutation), so the map lock is never taken on the
/// hot path -- only here, and by [`jit_depth_of_tid`] while a peer is frozen.
///
/// Re-registering the same `os_tid` returns the EXISTING slot rather than
/// replacing it: OS tids are recycled after a thread exits, and handing the
/// recycled thread a fresh slot while some cycle still holds the old `Arc`
/// would split one tid's depth across two cells.
pub fn register_self_jit_depth_slot(os_tid: u32) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
    let mut map = match per_tid_jit_depth().write() {
        Ok(m) => m,
        // A poisoned registry means some thread panicked mid-publish. Hand back
        // a detached slot: the owner's stores go nowhere the initiator can read,
        // so that thread simply never gets credited and its cycles keep
        // refusing. Degrading to the old behaviour is the safe direction.
        Err(_) => return std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let slot = std::sync::Arc::clone(
        map.entry(os_tid)
            .or_insert_with(|| std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0))),
    );
    // Reset on (re-)registration. An entry can survive its owner: OS tids are
    // recycled, and a thread that died without running its TLS destructor
    // leaves its last depth behind. Handing the recycled thread that value
    // would credit depth NOBODY holds -- the over-credit direction. The caller
    // stores its real depth immediately after this returns.
    slot.store(0, std::sync::atomic::Ordering::Release);
    slot
}

/// Address of each thread's own `ShadowStack`, published by its owner.
///
/// A JIT frame's oops live in the shadow stack, which is a per-thread heap
/// `Box<[usize]>` and NOT the machine stack -- so `helper_window_pass`, which
/// scans registers plus `[rsp, stack_base)`, cannot see them. For the initiator
/// and for a cooperatively parked peer that is fine (`collect_roots` scans its
/// own; a parked peer publishes its own and remaps on resume). A BLOCKED peer
/// does neither, so without this its shadow-stack oops are unpinned during the
/// collection -- which is what this map exists to fix, by letting the initiator
/// find and pin them.
///
/// The REMAP half is a separate repair and has since landed beside it:
/// `apply_blocked_wake_jit_remap` (both wake paths, `check_post_block_gc_refs`
/// and the leaked-region fallback `apply_pending_blocked_fixups`) now remaps the
/// waking peer's shadow stack, active JIT frames and register image. The two are
/// complementary and neither subsumes the other -- a pin keeps the objects still
/// for the cycle, a remap fixes up a peer whose objects moved on a cycle that
/// did not pin it.
///
/// The initiator cannot recover the window from the peer's frames the way
/// `shadow_window_from_frame` does: that helper only trusts a frame whose
/// cached `JvmThread` is the CURRENT thread's, and attributing a
/// `CompiledMethod` to a conservatively-found frame is the mis-attribution that
/// has already SIGSEGV'd the band verifier. So the owner publishes the address
/// instead -- authoritative, no attribution -- and the initiator reads `base`
/// and `top` out of it while the peer is blocked and therefore stable.
static PER_TID_SHADOW_ADDR: std::sync::OnceLock<
    std::sync::RwLock<std::collections::HashMap<u32, (usize, usize, usize)>>,
> = std::sync::OnceLock::new();

fn per_tid_shadow_addr()
-> &'static std::sync::RwLock<std::collections::HashMap<u32, (usize, usize, usize)>> {
    PER_TID_SHADOW_ADDR.get_or_init(|| std::sync::RwLock::new(std::collections::HashMap::new()))
}

/// Publish the calling thread's `ShadowStack` address together with the `base`
/// and `end` of its backing buffer.
///
/// Why all three. The live extent of the window is `[base, top)`, and `top` is
/// mutated INLINE by compiled code -- no Rust runs on a push -- so the only
/// current value lives in the struct, and reading it needs the struct's
/// address. But `ShadowStack`'s own contract says `base`/`end` "remain valid
/// even if the `ShadowStack` struct itself is moved", i.e. the struct address
/// is NOT guaranteed stable in general.
///
/// It is stable in the case that matters: the JIT caches `*mut JvmThread` in
/// every compiled frame and reaches the shadow stack as
/// `thread + shadow_off_in_thread`, so the thread cannot move while any
/// compiled frame is live -- and a blocked peer worth scanning has live
/// compiled frames. The gap is the narrow case where a thread enters JIT
/// (publishing), returns from every JIT frame, moves, and then blocks with a
/// FALSE-POSITIVE `has_jit`: the reader would dereference a stale address.
///
/// `base`/`end` close it. They point into the heap `Box`, never change after
/// `ensure_allocated`, and the reader requires the struct's own `base`/`end` to
/// equal these before trusting `top`. A moved or freed struct matching both
/// exactly is not a case that arises.
pub fn publish_self_shadow_addr(os_tid: u32, addr: usize, base: usize, end: usize) {
    if addr == 0 || base == 0 || end <= base {
        return;
    }
    if let Ok(mut map) = per_tid_shadow_addr().write() {
        map.insert(os_tid, (addr, base, end));
    }
}

/// The `(addr, base, end)` triple `os_tid` published, if any.
pub fn shadow_window_of_tid(os_tid: u32) -> Option<(usize, usize, usize)> {
    let map = per_tid_shadow_addr().read().ok()?;
    map.get(&os_tid).copied()
}

/// Drop `os_tid`'s slot when its owning thread exits.
///
/// Without this a dead thread's last depth stays readable under a tid the OS
/// will hand to someone else, and a recycled thread that never enters JIT never
/// overwrites it -- so the initiator would credit phantom depth for a peer with
/// no compiled frames at all.
pub fn unregister_jit_depth_slot(os_tid: u32) {
    if let Ok(mut map) = per_tid_jit_depth().write() {
        map.remove(&os_tid);
    }
    // The shadow address dies with the thread too: its `JvmThread` (and the
    // `Box` the window points into) goes with it, so a leftover entry is a
    // dangling pointer under a tid the OS will recycle.
    if let Ok(mut map) = per_tid_shadow_addr().write() {
        map.remove(&os_tid);
    }
}

/// The JIT depth `os_tid` last published, or `None` if it never registered.
///
/// `None` and `Some(0)` are NOT interchangeable for the caller: a thread that
/// never registered has an UNKNOWN depth, and crediting zero for it would claim
/// its frames are accounted for. See [`add_xt_cycle_pinned_jit_depth`].
pub fn jit_depth_of_tid(os_tid: u32) -> Option<usize> {
    let map = per_tid_jit_depth().read().ok()?;
    map.get(&os_tid)
        .map(|slot| slot.load(std::sync::atomic::Ordering::Acquire))
}

/// Peer JIT depth this cycle discharged by PINNING rather than by proof, and
/// whether every frozen peer could be attributed a published depth.
///
/// One peer whose depth is unknown poisons the whole ledger: the accounting is
/// a single process-wide subtraction, so it cannot say "credit these threads
/// and keep refusing for that one".
///
/// Production-global / `cfg(test)`-thread-local, for the same reason
/// `MOVING_YOUNG_COVERAGE_INCOMPLETE` is: gc unit tests run in parallel threads
/// of one process, and a shared ledger would let one test's deliberate credit
/// satisfy another test's deliberate shortfall.
#[cfg(not(test))]
static XT_CYCLE_PINNED_JIT_DEPTH: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(not(test))]
static XT_CYCLE_PINNED_DEPTH_EXACT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

#[cfg(test)]
thread_local! {
    static XT_CYCLE_PINNED_JIT_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static XT_CYCLE_PINNED_DEPTH_EXACT: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

#[cfg(not(test))]
fn pinned_depth_add(d: usize) {
    XT_CYCLE_PINNED_JIT_DEPTH.fetch_add(d, std::sync::atomic::Ordering::AcqRel);
}
#[cfg(not(test))]
fn pinned_depth_get() -> usize {
    XT_CYCLE_PINNED_JIT_DEPTH.load(std::sync::atomic::Ordering::Acquire)
}
#[cfg(not(test))]
fn pinned_depth_reset() {
    XT_CYCLE_PINNED_JIT_DEPTH.store(0, std::sync::atomic::Ordering::Release);
}
#[cfg(not(test))]
fn pinned_exact_set(v: bool) {
    XT_CYCLE_PINNED_DEPTH_EXACT.store(v, std::sync::atomic::Ordering::Release);
}
#[cfg(not(test))]
fn pinned_exact_get() -> bool {
    XT_CYCLE_PINNED_DEPTH_EXACT.load(std::sync::atomic::Ordering::Acquire)
}

#[cfg(test)]
fn pinned_depth_add(d: usize) {
    XT_CYCLE_PINNED_JIT_DEPTH.with(|c| c.set(c.get().saturating_add(d)));
}
#[cfg(test)]
fn pinned_depth_get() -> usize {
    XT_CYCLE_PINNED_JIT_DEPTH.with(std::cell::Cell::get)
}
#[cfg(test)]
fn pinned_depth_reset() {
    XT_CYCLE_PINNED_JIT_DEPTH.with(|c| c.set(0));
}
#[cfg(test)]
fn pinned_exact_set(v: bool) {
    XT_CYCLE_PINNED_DEPTH_EXACT.with(|c| c.set(v));
}
#[cfg(test)]
fn pinned_exact_get() -> bool {
    XT_CYCLE_PINNED_DEPTH_EXACT.with(std::cell::Cell::get)
}

/// Credit `depth` JIT entries belonging to a peer whose ENTIRE stack this cycle
/// pinned.
///
/// `depth == None` means the peer froze without ever having registered a slot,
/// so its depth is unknown; that marks the ledger inexact and
/// [`xt_cycle_pinned_jit_depth`] then refuses to credit anything at all.
pub fn add_xt_cycle_pinned_jit_depth(depth: Option<usize>) {
    match depth {
        Some(d) => pinned_depth_add(d),
        None => pinned_exact_set(false),
    }
}

/// Depth discharged by pinning this cycle, or 0 when the ledger is inexact.
pub fn xt_cycle_pinned_jit_depth() -> usize {
    if !pinned_exact_get() {
        return 0;
    }
    pinned_depth_get()
}

/// Reset the pinned-depth ledger. Shares the lifecycle of
/// [`clear_xt_cycle_pinned_jit_roots`] -- the pins and the depth they discharge
/// must appear and disappear together.
pub fn clear_xt_cycle_pinned_jit_depth() {
    pinned_depth_reset();
    pinned_exact_set(true);
}

#[cfg(test)]
mod pinned_peer_depth_tests {
    use super::*;

    /// The ledger sums the depths of peers whose stacks were pinned.
    #[test]
    fn pinned_depths_accumulate() {
        clear_xt_cycle_pinned_jit_depth();
        add_xt_cycle_pinned_jit_depth(Some(3));
        add_xt_cycle_pinned_jit_depth(Some(4));
        assert_eq!(xt_cycle_pinned_jit_depth(), 7);
    }

    /// THE safety property: one peer of unknown depth voids the whole credit,
    /// rather than being counted as zero.
    ///
    /// Counting it as zero is the unsound direction -- it would claim a peer's
    /// frames are accounted for when nothing pinned or proved them, and the
    /// collector would then relocate an object that peer's frame still names.
    #[test]
    fn one_unknown_depth_voids_the_whole_credit() {
        clear_xt_cycle_pinned_jit_depth();
        add_xt_cycle_pinned_jit_depth(Some(5));
        add_xt_cycle_pinned_jit_depth(None);
        add_xt_cycle_pinned_jit_depth(Some(6));
        assert_eq!(
            xt_cycle_pinned_jit_depth(),
            0,
            "an unattributable peer must void the credit, not contribute 0 to it"
        );
    }

    /// The poison does not outlive the cycle that set it.
    #[test]
    fn clearing_lifts_the_poison() {
        clear_xt_cycle_pinned_jit_depth();
        add_xt_cycle_pinned_jit_depth(None);
        assert_eq!(xt_cycle_pinned_jit_depth(), 0);
        clear_xt_cycle_pinned_jit_depth();
        add_xt_cycle_pinned_jit_depth(Some(2));
        assert_eq!(xt_cycle_pinned_jit_depth(), 2);
    }

    /// A registered thread reads back what it published; an unregistered tid is
    /// `None`, which is what makes the distinction above expressible.
    #[test]
    fn depth_slot_round_trips_and_unknown_tid_is_none() {
        let tid = 0xFEED_0001;
        let slot = register_self_jit_depth_slot(tid);
        slot.store(9, Ordering::Release);
        assert_eq!(jit_depth_of_tid(tid), Some(9));
        assert_eq!(jit_depth_of_tid(0xFEED_0002), None);
    }

    /// Re-registering a recycled OS tid hands back the SAME cell -- one tid's
    /// depth must never be split across two of them -- but RESET, because the
    /// previous owner may be dead and its leftover depth is held by nobody.
    #[test]
    fn re_registering_a_tid_returns_the_same_slot_reset_to_zero() {
        let tid = 0xFEED_0003;
        let a = register_self_jit_depth_slot(tid);
        a.store(4, Ordering::Release);
        let b = register_self_jit_depth_slot(tid);
        assert!(std::sync::Arc::ptr_eq(&a, &b), "one tid, one cell");
        assert_eq!(
            b.load(Ordering::Acquire),
            0,
            "a recycled tid must not inherit the dead thread's depth"
        );
    }

    /// A departed thread leaves nothing readable behind.
    #[test]
    fn unregistering_removes_the_slot() {
        let tid = 0xFEED_0004;
        register_self_jit_depth_slot(tid).store(7, Ordering::Release);
        assert_eq!(jit_depth_of_tid(tid), Some(7));
        unregister_jit_depth_slot(tid);
        assert_eq!(
            jit_depth_of_tid(tid),
            None,
            "a dead thread's depth must read as UNKNOWN, not as a stale number"
        );
    }
}
