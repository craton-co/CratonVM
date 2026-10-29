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
///
/// t1 wave 8, lane I25 (`CRATONVM_JIT_DOOR_ONE_DEPTH_COUNTER`): a door entry
/// pushed in the one-counter form is counted in
/// [`cratonvm_types::striped_counter::DOOR_DEPTH`] instead of here, and the
/// folded readers (`is_active`, `depth`) add it back.
#[cfg(not(test))]
static JIT_ACTIVE_DEPTH: cratonvm_types::striped_counter::DoorFoldedCounter =
    cratonvm_types::striped_counter::DoorFoldedCounter::new();

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

// ---- The per-pause moving-young coverage rows (gc-common w21-e) -----------
//
// The coverage verdict ([`moving_young_coverage_incomplete`]), its first reason
// and its reason mask, the un-rewritable-peer flag ([`unrewritable_peer_state`]),
// the conservative-scan count ([`conservative_jit_scans`]) and the helper-window
// pins ([`add_xt_cycle_pinned_jit_roots`]) describe ONE VM's pause. Until
// gc-common w21-e they were six process statics (`MOVING_YOUNG_COVERAGE_INCOMPLETE`,
// `MOVING_YOUNG_INCOMPLETE_REASON`, `MOVING_YOUNG_INCOMPLETE_REASON_MASK`,
// `UNREWRITABLE_PEER_STATE`, `CONSERVATIVE_JIT_SCANS`, `XT_CYCLE_PINNED_JIT_ROOTS`),
// kept to one VM's pause at a time only by the w6-a coverage slot
// (`vm/src/threading/gc_barrier.rs`, `COVERAGE_SLOT_OWNER`), which gives up
// after 250 ms. With the slot given up, VM B's opener erased VM A's verdict and
// pins mid-pause, and at any time another VM's mutators could bump
// `CONSERVATIVE_JIT_SCANS` and defeat ZGC's `conservative_jit_scans() == 0`
// refusal for this VM. They are now a [`CoverageCycle`], the `coverage` field of
// the VM's [`PauseLedger`], reached through the thread binding the other ledger
// rows use.
//
// # Why these rows needed more than the binding
//
// Unlike the rows moved before them, these have WRITERS off the pause's
// requesting thread, and for every one of them a lost write is the unsafe
// direction: a lost "incomplete" or un-rewritable mark, or a lost pin, licenses
// a move, and a lost scan count disarms the generational divert.
//
// | writer | thread | binding |
// |---|---|---|
// | `collect_roots`; the take-over and helper-window passes (`xt_root_scan.rs`, `stw_take_over_and_wait`, `pin_frozen_peer_roots_for_g1`); the initiator's own `update_root_snapshot` after it wins | the requester | its winning request binds it until `complete_gc` |
// | a parking peer's coverage proof (`publish_peer_jit_coverage_for_stw`) | the peer | `with_pause_ledger` in `safepoint_check` (w8-d) |
// | a parking peer's root deposit (`update_root_snapshot` from `safepoint_check`: the coverage proof, the conservative JIT scan and its `publish_pinned_jit_roots`, the opt-in `pin_unnamed_frame_refs`) | the peer | `scoped_pause_ledger` inside `update_root_snapshot` (`handoff-w21e-bind-the-safepoint-root-deposit`, applied; gci i44/sp corrected this row, which still said "none") |
// | a blocking thread's deposit (`deposit_root_snapshot_inner`) | the blocking thread, inside or outside a pause | `scoped_pause_ledger` (w18-c) |
//
// A write by a BOUND thread lands in its VM's ledger whether or not a pause is
// open. A blocking deposit made between two pauses therefore lands in its VM's
// rows and is cleared by that VM's next open, which is what the process static
// did with it; no writer outside every pause is lost, and no other VM's open
// clears it any more.
//
// A write by an UNBOUND thread must not go to the thread-local fallback the
// other rows use: its pause would never read it (the collector runs on another
// thread) and nothing would ever clear it. It lands in the process-wide ORPHAN
// rows instead ([`ORPHAN_COVERAGE`]), which every open clears and every read
// ORs in. They are the pre-w21 process static, now holding only what no binding
// claims, so a writer the audit missed keeps exactly its pre-w21 effect (the
// peer deposit was one until its handoff landed). [`coverage_orphan_writes`] counts
// those writes.
//
// # The readers
//
// Every collector read runs on the collecting thread, which is the requester
// (`run_collection_pause`) and so bound; the audit is in
// `docs/feature-designs/generative-gc-architecture.md`
// (w21-e status). A bound read sees its ledger's rows OR the orphan rows; an
// unbound read sees the orphan rows alone.
//
// Under `cfg(test)` the orphan rows are the calling thread's own fallback
// ledger's (`UNBOUND_PAUSE_LEDGER`), which keeps the per-thread isolation the
// old `cfg(test)` thread-locals gave the gc unit tests, which run in parallel
// threads of one process.

/// The per-pause moving-young coverage rows of ONE VM's pause (gc-common
/// w21-e): the `coverage` field of its [`PauseLedger`]. See the section comment
/// above for the thread audit and the orphan rows.
#[derive(Debug)]
pub struct CoverageCycle {
    /// See [`moving_young_coverage_incomplete`].
    incomplete: AtomicBool,
    /// The first [`incomplete_reason`] recorded this cycle; first wins.
    reason: AtomicUsize,
    /// EVERY reason this cycle recorded, as a bitmask over `incomplete_reason`
    /// codes, alongside the first-wins scalar above.
    ///
    /// Read by two deciders (ZGC's relocation licence, `zgc.rs` `UNPINNABLE_COVERAGE_REASONS`; the
    /// old-pin plan, through `peer_state_reason_recorded`), so a new writer changes behaviour, not
    /// only the census. It exists because
    /// the scalar is first-wins, which is right for "what forced this verdict"
    /// and wrong for "would repairing reason X have helped": a cycle reported
    /// as `active-safepoint-map-incomplete` may ALSO have hit
    /// `innermost-rbp-belongs-to-unguarded-callee`, and a cycle reported as the
    /// latter may have hit nothing else at all. Only that second kind turns
    /// into a moving collection if the innermost-rbp resolution is repaired, so
    /// sizing that repair needs the whole set, not its first element. Printed
    /// by `CRATONVM_DBG_GC_FALLBACK_REASONS=1`.
    reason_mask: AtomicUsize,
    /// A STRICTLY NARROWER verdict than `incomplete`: this cycle scanned state
    /// belonging to a peer thread that will never apply the collection's
    /// pointer map to itself (an OS-suspended in-JIT peer, or a blocked peer's
    /// JIT helper window). See [`unrewritable_peer_state`] for why the two must
    /// not be conflated.
    unrewritable: AtomicBool,
    /// See [`conservative_jit_scans`].
    scans: AtomicUsize,
    /// This cycle's helper-window pins, sorted and without duplicates. See
    /// [`add_xt_cycle_pinned_jit_roots`].
    xt_pins: parking_lot::Mutex<Vec<usize>>,
}

/// A copy of one [`CoverageCycle`]'s rows ([`PauseLedger::coverage_snapshot`]).
/// Diagnostics and tests; the collectors read through the free functions.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CoverageSnapshot {
    pub incomplete: bool,
    pub reason: usize,
    pub reason_mask: usize,
    pub unrewritable: bool,
    pub scans: usize,
    pub xt_pins: Vec<usize>,
}

impl CoverageCycle {
    /// An open cycle: complete, no reason, nothing scanned, nothing pinned.
    pub const fn new() -> Self {
        Self {
            incomplete: AtomicBool::new(false),
            reason: AtomicUsize::new(incomplete_reason::NONE),
            reason_mask: AtomicUsize::new(0),
            unrewritable: AtomicBool::new(false),
            scans: AtomicUsize::new(0),
            xt_pins: parking_lot::Mutex::new(Vec::new()),
        }
    }

    /// Open a coverage cycle: every row back to its empty state.
    fn open(&self) {
        self.incomplete.store(false, Ordering::Release);
        self.reason
            .store(incomplete_reason::NONE, Ordering::Release);
        self.reason_mask.store(0, Ordering::Release);
        self.unrewritable.store(false, Ordering::Release);
        self.scans.store(0, Ordering::Relaxed);
        self.clear_xt_pins();
    }

    fn clear_xt_pins(&self) {
        self.xt_pins.lock().clear();
    }

    fn mark_incomplete(&self) {
        self.incomplete.store(true, Ordering::Release);
    }

    /// Record `reason`: the first reason of the cycle wins the scalar, every
    /// reason joins the mask.
    fn note_reason(&self, reason: usize) {
        let _ = self.reason.compare_exchange(
            incomplete_reason::NONE,
            reason,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        if reason < incomplete_reason::COUNT {
            self.reason_mask
                .fetch_or(1usize << reason, Ordering::AcqRel);
        }
    }

    fn mark_unrewritable(&self) {
        self.unrewritable.store(true, Ordering::Release);
    }

    fn bump_scans(&self) {
        self.scans.fetch_add(1, Ordering::Relaxed);
    }

    fn add_xt_pins(&self, addrs: &[usize]) {
        let mut pins = self.xt_pins.lock();
        pins.extend_from_slice(addrs);
        pins.sort_unstable();
        pins.dedup();
    }

    /// A copy of the rows.
    pub fn snapshot(&self) -> CoverageSnapshot {
        CoverageSnapshot {
            incomplete: self.incomplete.load(Ordering::Acquire),
            reason: self.reason.load(Ordering::Acquire),
            reason_mask: self.reason_mask.load(Ordering::Acquire),
            unrewritable: self.unrewritable.load(Ordering::Acquire),
            scans: self.scans.load(Ordering::Relaxed),
            xt_pins: self.xt_pins.lock().clone(),
        }
    }
}

impl Default for CoverageCycle {
    fn default() -> Self {
        Self::new()
    }
}

/// The coverage rows of writes no pause-ledger binding claims (gc-common
/// w21-e). Cleared by every coverage open, ORed into every read. See the
/// section comment above: this is where a write from an unbound thread goes
/// instead of being lost.
#[cfg(not(test))]
static ORPHAN_COVERAGE: CoverageCycle = CoverageCycle::new();

/// Coverage writes that no binding claimed and so landed in the orphan rows.
/// Monotone diagnostic; see [`coverage_orphan_writes`].
static COVERAGE_ORPHAN_WRITES: AtomicU64 = AtomicU64::new(0);

/// How many coverage writes (a mark, a reason, a scan count, a helper-window
/// pin set) were made by a thread bound to no VM's pause ledger, since the
/// process started.
///
/// Such a write is not lost (it lands in the orphan rows every read sees), but
/// it is not per VM either. With `handoff-w21e-bind-the-safepoint-root-deposit`
/// applied, a production run should count 0 here outside collections that run
/// with no barrier request at all; a non-zero count names a writer the w21-e
/// audit missed.
pub fn coverage_orphan_writes() -> u64 {
    COVERAGE_ORPHAN_WRITES.load(Ordering::Relaxed)
}

#[cfg(not(test))]
#[inline]
fn with_orphan_coverage<R>(f: impl FnOnce(&CoverageCycle) -> R) -> Option<R> {
    Some(f(&ORPHAN_COVERAGE))
}

#[cfg(test)]
#[inline]
fn with_orphan_coverage<R>(f: impl FnOnce(&CoverageCycle) -> R) -> Option<R> {
    UNBOUND_PAUSE_LEDGER
        .try_with(|ledger| f(&ledger.coverage))
        .ok()
}

/// Run `f` on the rows a coverage WRITE by the calling thread belongs to: its
/// bound ledger's, else the orphan rows (counted in [`COVERAGE_ORPHAN_WRITES`]).
#[inline]
fn coverage_write(f: impl Fn(&CoverageCycle)) {
    let bound = BOUND_PAUSE_LEDGER
        .try_with(|bound| {
            if let Ok(slot) = bound.try_borrow() {
                if let Some(ledger) = slot.as_ref() {
                    f(&ledger.coverage);
                    return true;
                }
            }
            false
        })
        .unwrap_or(false);
    if !bound {
        COVERAGE_ORPHAN_WRITES.fetch_add(1, Ordering::Relaxed);
        let _ = with_orphan_coverage(|orphan| f(orphan));
    }
}

/// Run `f` on every set of coverage rows a READ (or an open) by the calling
/// thread covers: its bound ledger's first, if any, then the orphan rows.
#[inline]
fn coverage_each(mut f: impl FnMut(&CoverageCycle)) {
    let _ = BOUND_PAUSE_LEDGER.try_with(|bound| {
        if let Ok(slot) = bound.try_borrow() {
            if let Some(ledger) = slot.as_ref() {
                f(&ledger.coverage);
            }
        }
    });
    let _ = with_orphan_coverage(|orphan| f(orphan));
}

#[inline]
fn coverage_incomplete_get() -> bool {
    let mut any = false;
    coverage_each(|c| any |= c.incomplete.load(Ordering::Acquire));
    any
}

/// The bound ledger's first reason if it has one, else the orphan rows'.
#[inline]
fn incomplete_reason_get() -> usize {
    let mut first = incomplete_reason::NONE;
    coverage_each(|c| {
        if first == incomplete_reason::NONE {
            first = c.reason.load(Ordering::Acquire);
        }
    });
    first
}

#[inline]
fn incomplete_reason_mask_get() -> usize {
    let mut mask = 0usize;
    coverage_each(|c| mask |= c.reason_mask.load(Ordering::Acquire));
    mask
}

#[inline]
fn unrewritable_peer_state_get() -> bool {
    let mut any = false;
    coverage_each(|c| any |= c.unrewritable.load(Ordering::Acquire));
    any
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
//
// PER VM since gc-common w8-d (2026-09-24): the ledger lives in the depositing
// VM's [`PauseLedger`], not in a process static -- see the next section.

// ---- The per-VM pause ledger (gc-common w8-d, 2026-09-24) -----------------
//
// Five per-pause rows of this module were process statics although only
// VM-side code reads them, and although each describes ONE VM's pause:
//
// * `PEER_PROVEN_JIT_DEPTH` -- written by a parking peer (`safepoint_check`),
//   read by the initiator (`collect_roots`);
// * `XT_CYCLE_PINNED_JIT_DEPTH`, `XT_CYCLE_PINNED_DEPTH_EXACT` -- written by
//   the initiator (`helper_window_pass`), read by the initiator
//   (`collect_roots`);
// * `XT_POST_QUOTA_NS_LAST_CYCLE`, `XT_PASS_NS_LAST_CYCLE` -- written by the
//   initiator (`stw_take_over_and_wait`), read by the initiator (the pause
//   line).
//
// No collector reads any of them, so they could move without changing a
// collector's call site: they now live in a [`PauseLedger`] that each VM's
// `GcBarrier` owns, and every free function below reaches "the ledger of the
// pause the CALLING THREAD is part of" through a thread-local binding:
//
// * the initiator binds its barrier's ledger when its request WINS
//   (`GcBarrier::request_stw_counted_locked`, before the reset that opens the
//   pause) and unbinds it in `complete_gc` -- so its root scan, its take-over
//   and its pause line all read and write its own VM's ledger;
// * a parking peer binds ITS barrier's ledger around its deposit only
//   ([`with_pause_ledger`], in `safepoint_check`).
//
// A thread with no binding (a unit test, a caller outside every pause) gets a
// THREAD-LOCAL fallback ledger. That is the same isolation the `cfg(test)`
// thread-locals gave the gc unit tests, and in production it can only ever
// answer in the safe direction: a deposit into it is invisible to every
// initiator (its peer is refused, not credited) and a read of it by an
// unbound thread is 0 (no credit, no take-over time).
//
// What this closes: VM B's pause used to be able to write VM A's proven depth
// (B's peers deposit, B's request resets) whenever the two pauses overlapped
// -- which the w6-a coverage slot prevents except after its 250 ms give-up.
// An inflated `proven` is the ONE unsound state of the peer ledger (it
// licenses a relocation nobody proved). With the ledger per VM, no other VM
// can write it at all, slot or no slot.
//
// `PEER_COVERAGE_ACCEPTED` / `REFUSED` / `DEPOSITS` stay process counters:
// monotone census totals nothing decides on.
//
// gc-common w9-f moved six more rows in, the take-over counts
// (`XT_*_LAST_CYCLE`), after a thread audit of their `gen_heap.rs` readers:
// see [`XtCounts`].

/// The per-pause rows of ONE VM's stop-the-world pause that only VM-side code
/// reads. Owned by the VM's `GcBarrier` (one per VM); reached by the free
/// functions of this module through the calling thread's binding
/// ([`bind_pause_ledger`], [`with_pause_ledger`]). See the section comment
/// above for why a thread-local route is exact here.
#[derive(Debug)]
pub struct PauseLedger {
    /// JIT depth cooperatively-parked peers PROVED rewritable this pause. See
    /// [`add_peer_proven_jit_depth`].
    proven_depth: AtomicUsize,
    /// Peer JIT depth this pause discharged by PINNING. See
    /// [`add_xt_cycle_pinned_jit_depth`].
    pinned_depth: AtomicUsize,
    /// Whether every pinned peer could be attributed a published depth.
    pinned_exact: AtomicBool,
    /// See [`xt_post_quota_ns`].
    post_quota_ns: AtomicU64,
    /// See [`xt_pass_ns`].
    pass_ns: AtomicU64,
    /// This pause's take-over counts, in [`xt_cycle_coverage`]'s order:
    /// passes run, peers taken over, peers unclassified, take-over roots,
    /// helper windows, helper-window roots. The `XT_*_LAST_CYCLE` process
    /// statics until gc-common w9-f; see [`XtCounts`].
    xt: XtCounts,
    /// This pause's [`TakeoverVerdict`]. The `TAKEOVER_VERDICT` process static
    /// until gc-common w10-g; see [`takeover_verdict`] for the thread audit
    /// that makes the move exact.
    verdict: parking_lot::Mutex<TakeoverVerdict>,
    /// This pause's blocked/frozen-peer native-stack captures,
    /// `(os_tid, addr, value)`. The `PEER_STACK_SLOTS` process static until
    /// gc-common w18-c; see [`record_peer_stack_slot`] for the thread audit.
    stack_slots: parking_lot::Mutex<Vec<(u32, usize, usize)>>,
    /// gcg g6/jn: the words of this pause's conservatively deposited thread
    /// snapshots (`publish_conservative_deposit_words`), for the Generational
    /// unrecorded-young-root guard to exclude.
    conservative_deposit_words: parking_lot::Mutex<Vec<usize>>,
    /// This pause's peer register-word capture for the
    /// `CRATONVM_DBG_PEER_REG_PAIRING` diagnostic, `(os_tid, reg, value)`. The
    /// `PEER_REG_CAPTURE` process static until gc-common w18-c.
    reg_capture: parking_lot::Mutex<Vec<(u32, u8, usize)>>,
    /// This pause's moving-young coverage rows: the verdict, its reasons, the
    /// un-rewritable-peer flag, the conservative-scan count and the
    /// helper-window pins. Six process statics until gc-common w21-e; see
    /// [`CoverageCycle`]. Opened by [`begin_moving_young_coverage_cycle`] (the
    /// pins also by [`reset_peer_proven_jit_depth`]), NOT by [`PauseLedger::open`],
    /// which is what the statics did too: a pause that opens no coverage cycle
    /// (a frame-trace or heap-dump pause) leaves the verdict to the next
    /// collection's opener.
    coverage: CoverageCycle,
    /// This VM's young pin-word ledger: its pause stamp, its threads' deposit
    /// slots and its peer register words. The process static
    /// `YOUNG_PIN_LEDGER` until gc-common w36-c; see [`YoungPinOwner`] for how
    /// a thread reaches it and why an unbound thread fails closed. Stamped by
    /// [`open_young_pin_pause`], never cleared (see the ledger's section).
    young_pins: YoungPinLedger,
    /// gcd d3/m: `(os_tid, depth)` of the blocked-monitor peers
    /// `credit_proven_blocked_monitor_peers` credited by proof this pause,
    /// for the helper-window pass to read into the young pin ledger as bands
    /// ([`stash_proven_monitor_peer_for_ledger`]). Cleared at every pause open
    /// and take-over start.
    proven_monitor_peers: parking_lot::Mutex<Vec<(u32, usize)>>,
    /// gcd d4/m: how many of this VM's threads hold RAW JNI local references
    /// right now -- inside a JNI native whose locals are raw addresses, or a
    /// foreign-attached thread whose attach-level locals are. A STANDING count
    /// (never cleared by a pause open): see [`RawJniLocalsScope`] and
    /// [`raw_jni_locals_open`].
    raw_jni_locals: AtomicUsize,
    /// gch h2/yg: [`note_raw_jni_locals_closed`] calls that found the count
    /// already at zero (the saturation used to make an unbalanced close
    /// silent). Monotone; read through [`raw_jni_locals_census`].
    raw_jni_locals_underflows: AtomicU64,
}

/// The six per-pause take-over counts [`xt_cycle_coverage`] reports.
///
/// `passes` is the load-bearing one for reading the rest. `stw_takeover_should_scan`
/// gates round 0 on the cheap `any_thread_in_jit()` hint, so a fully
/// cooperative pause legitimately runs ZERO passes -- and then `taken_over=0
/// unclassified=0` means "never looked", not "looked and found nothing". Those
/// two readings point at opposite conclusions, so they must not share an
/// encoding.
///
/// # Why these rows are per VM (gc-common w9-f, 2026-09-24)
///
/// They were six process statics (`XT_PASSES_LAST_CYCLE`,
/// `XT_TAKEN_OVER_LAST_CYCLE`, `XT_UNCLASSIFIED_LAST_CYCLE`,
/// `XT_ROOTS_LAST_CYCLE`, `XT_HW_WINDOWS_LAST_CYCLE`, `XT_HW_ROOTS_LAST_CYCLE`),
/// kept static at w8-d only because `gen_heap.rs` reads them. A thread audit
/// says every access is made by the thread running the pause, which is bound
/// to its own VM's ledger from its winning request to `complete_gc`:
///
/// * the writers: [`reset_xt_cycle`] at the top of `stw_take_over_and_wait`;
///   [`publish_xt_pass`] and [`publish_xt_helper_window`] from
///   `vm/src/jit/xt_root_scan.rs` (which spawns no thread) and from the
///   take-over's own helper-window step. All of them run inside the
///   take-over or the root gathering of the pause they describe.
/// * the decision reader: `takeover_verdict_unreadable`'s `unclassified`
///   (`gc_and_alloc.rs`, inside `stw_take_over_and_wait`). It runs on the
///   SAME thread as the writers, so it reads what they wrote whether or not
///   the thread is bound. A thread-local route therefore cannot lose an
///   unclassified peer.
/// * the diagnostic readers: the pause line (`gc_events::pause_safepoint_facts`,
///   on the initiator) and `gen_heap`'s `record_young_span_freed` and doomed-
///   referrer report. Both run on the collecting thread, which is the one
///   that requested the pause (`run_collection_pause`, and the concurrent-mark
///   pause drivers, each request and collect on one thread).
///
/// What this closes: another VM's take-over no longer zeroes or adds to this
/// VM's counts. Before, when two VMs' pauses overlapped (the w6-a coverage
/// slot's 250 ms give-up), VM B's `reset_xt_cycle` could erase VM A's
/// `unclassified` between A's passes and A's verdict. The verdict then read
/// "every peer answered", i.e. licence `Move` with a peer whose JIT frames
/// nobody saw. The verdict itself followed in gc-common w10-g (the
/// `verdict` field; see [`takeover_verdict`]).
///
/// gcd d2/i (2026-09-27) added a seventh row, `hw_unpinned`: see
/// [`set_xt_helper_windows_unpinned`].
#[derive(Debug)]
struct XtCounts {
    passes: AtomicU64,
    taken_over: AtomicU64,
    unclassified: AtomicU64,
    roots: AtomicU64,
    hw_windows: AtomicU64,
    hw_roots: AtomicU64,
    /// This pause's helper windows that REFUSE the discharge (unpinned
    /// windows plus unreadable blocked peers). Not part of
    /// [`xt_cycle_coverage`]'s tuple; read through
    /// [`xt_helper_windows_unpinned`].
    hw_unpinned: AtomicU64,
    /// gcd d10/t: take-over peers this pause whose machine stack could not be
    /// read whole (the per-pause twin of the process total
    /// `xt_root_scan::XT_TAKEOVER_STACK_INCOMPLETE`). Read through
    /// [`xt_takeover_stack_incomplete_this_pause`].
    stack_incomplete: AtomicU64,
}

impl XtCounts {
    const fn new() -> Self {
        Self {
            passes: AtomicU64::new(0),
            taken_over: AtomicU64::new(0),
            unclassified: AtomicU64::new(0),
            roots: AtomicU64::new(0),
            hw_windows: AtomicU64::new(0),
            hw_roots: AtomicU64::new(0),
            hw_unpinned: AtomicU64::new(0),
            stack_incomplete: AtomicU64::new(0),
        }
    }

    fn clear(&self) {
        self.passes.store(0, Ordering::Relaxed);
        self.taken_over.store(0, Ordering::Relaxed);
        self.unclassified.store(0, Ordering::Relaxed);
        self.roots.store(0, Ordering::Relaxed);
        self.hw_windows.store(0, Ordering::Relaxed);
        self.hw_roots.store(0, Ordering::Relaxed);
        self.hw_unpinned.store(0, Ordering::Release);
        self.stack_incomplete.store(0, Ordering::Release);
    }

    fn snapshot(&self) -> (u64, u64, u64, u64, u64, u64) {
        (
            self.passes.load(Ordering::Relaxed),
            self.taken_over.load(Ordering::Relaxed),
            self.unclassified.load(Ordering::Relaxed),
            self.roots.load(Ordering::Relaxed),
            self.hw_windows.load(Ordering::Relaxed),
            self.hw_roots.load(Ordering::Relaxed),
        )
    }
}

impl PauseLedger {
    /// An empty ledger: nothing proven, nothing pinned, exact.
    pub const fn new() -> Self {
        Self {
            proven_depth: AtomicUsize::new(0),
            pinned_depth: AtomicUsize::new(0),
            pinned_exact: AtomicBool::new(true),
            post_quota_ns: AtomicU64::new(0),
            pass_ns: AtomicU64::new(0),
            xt: XtCounts::new(),
            verdict: parking_lot::Mutex::new(TakeoverVerdict::NONE),
            stack_slots: parking_lot::Mutex::new(Vec::new()),
            conservative_deposit_words: parking_lot::Mutex::new(Vec::new()),
            reg_capture: parking_lot::Mutex::new(Vec::new()),
            coverage: CoverageCycle::new(),
            young_pins: YoungPinLedger::new(),
            proven_monitor_peers: parking_lot::Mutex::new(Vec::new()),
            raw_jni_locals: AtomicUsize::new(0),
            raw_jni_locals_underflows: AtomicU64::new(0),
        }
    }

    /// Depth proven in THIS ledger, whoever is bound. Diagnostics and tests;
    /// the collection reads [`peer_proven_jit_depth`].
    pub fn proven_jit_depth(&self) -> usize {
        self.proven_depth.load(Ordering::Acquire)
    }

    /// THIS ledger's coverage rows, whoever is bound, without the orphan rows a
    /// bound read also sees. Diagnostics and tests (gc-common w21-e); the
    /// collectors read through [`moving_young_coverage_incomplete`] & co.
    pub fn coverage_snapshot(&self) -> CoverageSnapshot {
        self.coverage.snapshot()
    }

    /// THIS ledger's young pin-word pause stamp, whoever is bound. Diagnostics
    /// and tests (gc-common w36-c); see [`young_pin_pause_stamp`].
    pub fn young_pin_stamp(&self) -> u64 {
        self.young_pins.stamp()
    }

    /// Open a pause: every row back to its empty state.
    ///
    /// gc-common w18-c: the peer captures too. Until then only a COLLECTION's
    /// opener (`begin_moving_young_coverage_cycle`) cleared them, so a
    /// non-collection pause's take-over captures (a frame-trace or
    /// concurrent-mark pause, which never folds) sat in the buffer until the
    /// next collection opened. Nothing folded them, so this changes no answer;
    /// it gives the buffer the per-pause lifetime every other row has.
    fn open(&self) {
        self.clear_proven();
        self.clear_pinned();
        self.clear_takeover_times();
        self.xt.clear();
        self.clear_verdict();
        self.clear_stack_slots();
        self.clear_reg_capture();
        self.proven_monitor_peers.lock().clear();
        // gch h1/yp: the conservatively deposited snapshot words describe ONE
        // pause's gather. Only `publish_conservative_deposit_words` (the
        // gather, under `CRATONVM_GEN_UNRECORDED_ROOT_GUARD_DEPOSITS`) ever
        // replaced them, so a pause whose gather did not run read the previous
        // pause's words -- addresses since recycled -- as excluded. Empty (a
        // no-op) with the switch off.
        self.conservative_deposit_words.lock().clear();
    }

    /// Discard this ledger's peer native-stack captures, counting them in
    /// [`PEER_STACK_SLOTS_DISCARDED`]. See [`clear_peer_stack_slots`].
    fn clear_stack_slots(&self) {
        let mut g = self.stack_slots.lock();
        if !g.is_empty() {
            PEER_STACK_SLOTS_DISCARDED.fetch_add(g.len() as u64, Ordering::Relaxed);
            g.clear();
        }
    }

    /// Drop this ledger's register-pairing capture (armed runs only; the
    /// buffer is never filled otherwise).
    fn clear_reg_capture(&self) {
        if peer_reg_pairing_enabled() {
            self.reg_capture.lock().clear();
        }
    }

    /// Back to [`TakeoverVerdict::NONE`]: no take-over has run this pause.
    fn clear_verdict(&self) {
        *self.verdict.lock() = TakeoverVerdict::NONE;
    }

    fn clear_proven(&self) {
        self.proven_depth.store(0, Ordering::Release);
    }

    fn clear_pinned(&self) {
        self.pinned_depth.store(0, Ordering::Release);
        self.pinned_exact.store(true, Ordering::Release);
    }

    fn clear_takeover_times(&self) {
        self.post_quota_ns.store(0, Ordering::Relaxed);
        self.pass_ns.store(0, Ordering::Relaxed);
    }
}

impl Default for PauseLedger {
    fn default() -> Self {
        Self::new()
    }
}

thread_local! {
    /// The ledger of the pause the calling thread is part of, if any. Set by
    /// [`bind_pause_ledger`] (the initiator) and [`with_pause_ledger`] (a
    /// peer's deposit).
    static BOUND_PAUSE_LEDGER: std::cell::RefCell<Option<std::sync::Arc<PauseLedger>>> =
        const { std::cell::RefCell::new(None) };
    /// The ledger of a thread with no binding. See the section comment.
    static UNBOUND_PAUSE_LEDGER: PauseLedger = const { PauseLedger::new() };
}

/// Run `f` on the calling thread's ledger: its binding, else its thread-local
/// fallback. `None` only during thread-local teardown, which every caller
/// treats as "nothing recorded" -- the safe reading of each row.
#[inline]
fn with_ledger<R>(f: impl FnOnce(&PauseLedger) -> R) -> Option<R> {
    BOUND_PAUSE_LEDGER
        .try_with(|bound| {
            if let Ok(slot) = bound.try_borrow() {
                if let Some(ledger) = slot.as_ref() {
                    let ledger: &PauseLedger = ledger;
                    return Some(f(ledger));
                }
            }
            UNBOUND_PAUSE_LEDGER.try_with(|ledger| f(ledger)).ok()
        })
        .ok()
        .flatten()
}

/// Bind `ledger` as the calling thread's pause ledger until
/// [`unbind_pause_ledger`] (or the next bind). The initiator's half: called by
/// `GcBarrier` when a request wins, before the reset that opens the pause.
pub fn bind_pause_ledger(ledger: &std::sync::Arc<PauseLedger>) {
    let _ = BOUND_PAUSE_LEDGER.try_with(|bound| {
        if let Ok(mut slot) = bound.try_borrow_mut() {
            *slot = Some(std::sync::Arc::clone(ledger));
        }
    });
}

/// Drop the calling thread's binding if it is `ledger`. Called by
/// `GcBarrier::complete_gc`; a no-op on any other thread, or when the thread
/// was since bound to another VM's ledger.
pub fn unbind_pause_ledger(ledger: &std::sync::Arc<PauseLedger>) {
    let _ = BOUND_PAUSE_LEDGER.try_with(|bound| {
        if let Ok(mut slot) = bound.try_borrow_mut() {
            if slot
                .as_ref()
                .is_some_and(|mine| std::sync::Arc::ptr_eq(mine, ledger))
            {
                *slot = None;
            }
        }
    });
}

/// Whether the calling thread is bound to `ledger`. Tests and assertions.
pub fn pause_ledger_bound(ledger: &std::sync::Arc<PauseLedger>) -> bool {
    BOUND_PAUSE_LEDGER
        .try_with(|bound| {
            bound
                .try_borrow()
                .ok()
                .and_then(|slot| {
                    slot.as_ref()
                        .map(|mine| std::sync::Arc::ptr_eq(mine, ledger))
                })
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

/// Run `f` with `ledger` bound on the calling thread, restoring the previous
/// binding afterwards (also on unwind). A parking peer's half: its deposit
/// lands in the ledger of ITS VM's pause, whatever the thread was bound to.
pub fn with_pause_ledger<R>(ledger: &std::sync::Arc<PauseLedger>, f: impl FnOnce() -> R) -> R {
    let _restore = scoped_pause_ledger(ledger);
    f()
}

/// The guard form of [`with_pause_ledger`]: `ledger` is the calling thread's
/// binding until the guard drops (also on unwind), then the previous binding
/// is back. For a caller whose bound span is a run of statements rather than
/// one expression (the blocking deposit in `vm_exec.rs`, gc-common w18-c).
///
/// Drop guards in the reverse order of creation on one thread, as for any
/// scope; the guard is `!Send` so it cannot be dropped on another thread.
#[must_use = "the binding ends when the guard drops"]
pub fn scoped_pause_ledger(ledger: &std::sync::Arc<PauseLedger>) -> PauseLedgerScope {
    let previous = BOUND_PAUSE_LEDGER
        .try_with(|bound| {
            bound
                .try_borrow_mut()
                .ok()
                .map(|mut slot| slot.replace(std::sync::Arc::clone(ledger)))
        })
        .ok()
        .flatten();
    PauseLedgerScope {
        previous,
        _not_send: std::marker::PhantomData,
    }
}

/// See [`scoped_pause_ledger`].
pub struct PauseLedgerScope {
    /// `Some(previous binding)` when the bind happened, `None` when TLS was
    /// unavailable (nothing to restore).
    previous: Option<Option<std::sync::Arc<PauseLedger>>>,
    /// The binding is the creating thread's; restoring it elsewhere would
    /// rebind the wrong thread.
    _not_send: std::marker::PhantomData<*const ()>,
}

impl Drop for PauseLedgerScope {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            let _ = BOUND_PAUSE_LEDGER.try_with(|bound| {
                if let Ok(mut slot) = bound.try_borrow_mut() {
                    *slot = previous;
                }
            });
        }
    }
}

#[cfg(not(test))]
static PEER_COVERAGE_ACCEPTED: AtomicUsize = AtomicUsize::new(0);
#[cfg(not(test))]
static PEER_COVERAGE_REFUSED: AtomicUsize = AtomicUsize::new(0);
#[cfg(not(test))]
static PEER_COVERAGE_DEPOSITS: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
thread_local! {
    static PEER_COVERAGE_ACCEPTED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PEER_COVERAGE_REFUSED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PEER_COVERAGE_DEPOSITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(not(test))]
#[inline]
fn peer_coverage_deposit_bump() {
    PEER_COVERAGE_DEPOSITS.fetch_add(1, Ordering::Relaxed);
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
fn peer_coverage_deposit_bump() {
    PEER_COVERAGE_DEPOSITS.with(|c| c.set(c.get() + 1));
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

/// Clear the per-pause ledgers for a pause that is about to open.
///
/// Called from [`begin_moving_young_coverage_cycle`] and from the VM's
/// `GcBarrier::request_stw` — both run strictly BEFORE any peer can park and
/// deposit, and a pause that reaches only one of them is still cleared.
///
/// The per-VM rows -- the proven and pinned depths and the take-over times --
/// are cleared in the calling thread's [`PauseLedger`]. The barrier binds its
/// own ledger before calling this, so a request clears only its own VM's
/// rows (gc-common w8-d). Clearing the take-over times here is also what makes
/// [`xt_post_quota_ns`]'s "0 when no take-over ran" true: until w8-d only a
/// take-over (`reset_xt_cycle`) zeroed them, so a pause that ran none printed
/// the previous take-over's figures.
pub fn reset_peer_proven_jit_depth() {
    // `PauseLedger::open` also clears the take-over verdict (gc-common w3-g).
    // `reset_xt_cycle` clears it only when a take-over RUNS; a pause that never
    // reaches `stw_take_over_and_wait` (a backend without JIT TLAB skips, the
    // take-over switched off) would otherwise hand its collector the verdict of
    // whichever earlier pause last froze a peer -- now that the collectors
    // consume it. Per VM since gc-common w10-g: another VM's request no longer
    // resets THIS VM's verdict to `NONE` (licence `Move`) mid-pause.
    let _ = with_ledger(PauseLedger::open);
    // gen r4w5/pinwords5: STAMP, not clear — see `open_young_pin_pause`.
    open_young_pin_pause();
    // The helper-window pins describe peers frozen during THIS cycle only.
    clear_xt_cycle_pinned_jit_roots();
}

/// A cooperatively-parking peer deposits `depth` JIT entries it has just PROVEN
/// rewritable for this pause.
///
/// `depth` must be the depositing thread's own `JIT_ENTRY_CHAIN` length read
/// AFTER its per-thread coverage proof returned `true`: the proof prunes
/// returned entries, and a length read before it can be too LARGE — which is
/// the unsound direction here, since the initiator's test is a comparison
/// against the process-wide depth.
///
/// Lands in the calling thread's [`PauseLedger`]: `safepoint_check` binds the
/// peer's own VM's ledger around the deposit ([`with_pause_ledger`]), so a
/// deposit is only ever credited to its own VM's pause (gc-common w8-d).
pub fn add_peer_proven_jit_depth(depth: usize) {
    if depth != 0 && with_ledger(|l| l.proven_depth.fetch_add(depth, Ordering::AcqRel)).is_some() {
        peer_coverage_deposit_bump();
    }
}

/// Total peer JIT depth proven rewritable for this pause, in the calling
/// thread's [`PauseLedger`] -- for the initiator, its own VM's.
pub fn peer_proven_jit_depth() -> usize {
    with_ledger(PauseLedger::proven_jit_depth).unwrap_or(0)
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
    /// gcd d5/s (for gcd d2/f): a compiled shadow-stack push on THIS thread
    /// bailed (it would have passed the buffer's end) and may still be in
    /// flight, so that safepoint's oops are published nowhere
    /// (`conservative_roots::shadow_bail_refuses_moving`). Until the proof
    /// switches to it, that refusal is recorded as [`UNPUBLISHED_FRAME_OOP`].
    pub const SHADOW_OVERFLOW_BAIL: usize = 17;
    /// gcj j2/yg2: the root gatherer forced the in-place young sweep to keep a
    /// side-table promise (`set_force_non_moving_jit_roots`: the 14w weak-mode
    /// promise or the 14w' young-mirror deferral), not a coverage proof.
    pub const FORCED_IN_PLACE_PROMISE: usize = 18;

    /// One past the highest defined reason code. Sizes the per-reason counter
    /// array; a new variant must bump it (asserted by
    /// `every_incomplete_reason_has_a_label`).
    pub const COUNT: usize = 19;

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
            SHADOW_OVERFLOW_BAIL => "shadow-stack-push-bailed",
            FORCED_IN_PLACE_PROMISE => "forced-in-place-side-table-promise",
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
static MOVING_YOUNG_REASON_COUNTS: [AtomicUsize; incomplete_reason::COUNT] =
    [const { AtomicUsize::new(0) }; incomplete_reason::COUNT];

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
    // The verdict, its reasons, the un-rewritable flag, the scan count and the
    // helper-window pins: the bound ledger's coverage rows and the orphan rows
    // (gc-common w21-e; see `CoverageCycle`). The pins describe peers frozen
    // during THIS cycle, so they are cleared here as well as in
    // `reset_peer_proven_jit_depth`: this entry point does not go through it,
    // and a pause that reaches only one of the two is still cleared.
    coverage_each(CoverageCycle::open);
    // The cross-thread handshake ledger is per-PAUSE and only ever read as
    // "does this account for every peer JIT entry?", so a value carried over
    // from the previous pause would be an over-count — the one direction that
    // could license a relocation nobody proved. Clear it here and again in the
    // barrier's `request_stw`; see `reset_peer_proven_jit_depth`. In the
    // calling thread's `PauseLedger`, i.e. this VM's (gc-common w8-d).
    //
    // Same scope, same reason: the depth the helper-window pins discharge
    // describes peers frozen during THIS cycle (the pins themselves were
    // cleared with the coverage rows above).
    let _ = with_ledger(|l| {
        l.clear_proven();
        l.clear_pinned();
    });
    // Same scope, and this one is load-bearing rather than belt-and-braces.
    // The blocked-peer native-stack captures are produced by exactly the
    // conservative scans `conservative_jit_scans` counts above, and their only
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
    coverage_write(CoverageCycle::mark_unrewritable);
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
    coverage_write(CoverageCycle::mark_incomplete);
}

/// Same as [`mark_moving_young_coverage_incomplete`], but also records WHY, so
/// the warn-level fallback diagnostic names the specific unproven obligation
/// instead of just "incomplete". First reason of a cycle wins (it is the one
/// that actually forced the decision; later ones are consequences).
///
/// Lands in the calling thread's bound [`PauseLedger`], else in the orphan rows
/// (gc-common w21-e; see [`CoverageCycle`]): never lost.
pub fn mark_moving_young_coverage_incomplete_because(reason: usize) {
    coverage_write(|c| {
        c.note_reason(reason);
        c.mark_incomplete();
        // Classify off the reason the CALLER passed, not the stored one: the
        // stored reason is first-wins (it names what forced the decision), so a
        // later cross-thread obligation would otherwise never arm the
        // promotion gate.
        if reason_implies_unrewritable_peer_state(reason) {
            c.mark_unrewritable();
        }
    });
}

/// [`mark_moving_young_coverage_incomplete_because`] WITHOUT the
/// un-rewritable bit: the reason (first-wins scalar and mask) and the
/// incomplete verdict only (gci i3/tk, after gci i2/jn's X2 on
/// `gci-i1jn-takeover-of-a-compiled-peer-forbids-promotion-on-every-young-cycle-RETIRED-RULING-20261018`).
///
/// For a peer-state reason ([`incomplete_reason::XT_TAKEOVER`],
/// [`incomplete_reason::XT_HELPER_WINDOW`]) whose CALLER licensed selective
/// promotion: every word the peer could name was rooted through a
/// derived-pointer-resolving probe, so the Generational pin-by-value set
/// (which also pins an interior root's base) holds every object it reaches.
/// The cycle still may not Cheney-copy (incomplete). The take-over driver
/// (`gc_and_alloc::stw_take_over_and_wait`) still raises
/// [`mark_unrewritable_peer_state`] when its pause-wide licence fails, so
/// this door alone never decides; only Generational callers use it (ZGC
/// reads the un-rewritable bit for other reasons). The mask keeps the
/// reason, so a reader that must refuse ANY peer state this pause (the
/// old-pin plan, `gen_heap`) still sees it.
pub fn mark_moving_young_coverage_incomplete_pinned_because(reason: usize) {
    coverage_write(|c| {
        c.note_reason(reason);
        c.mark_incomplete();
    });
}

/// gci i9/ng (i8/pe finding B): name `reason` as this cycle's first-wins LABEL
/// and nothing else -- no incomplete verdict, no mask bit, no un-rewritable
/// bit. For a caller that has ALREADY refused through the unlabelled
/// [`mark_moving_young_coverage_incomplete`] and whose decision must not move:
/// the mask is read by deciders (ZGC's relocation licence refuses on a zero
/// mask; [`peer_state_reason_recorded`] refuses Generational's old-pin plan),
/// so a label that joined it would change them. The scalar is read only by
/// diagnostics (the fallback histogram, the decision record, G1's
/// `root_coverage_incomplete_reason`, ZGC's coverage-reason counters, crash
/// reports). First wins, like every door: an earlier labelled reason stays.
pub fn label_moving_young_coverage_incomplete(reason: usize) {
    coverage_write(|c| {
        let _ = c.reason.compare_exchange(
            incomplete_reason::NONE,
            reason,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    });
}

/// Whether this cycle recorded a peer-state reason
/// ([`incomplete_reason::XT_TAKEOVER`] or
/// [`incomplete_reason::XT_HELPER_WINDOW`]) through either door, with or
/// without the un-rewritable bit (gci i3/tk). On the default path every such
/// record also raised [`unrewritable_peer_state`].
#[inline]
pub fn peer_state_reason_recorded() -> bool {
    let peer = (1usize << incomplete_reason::XT_TAKEOVER)
        | (1usize << incomplete_reason::XT_HELPER_WINDOW);
    moving_young_incomplete_reason_mask() & peer != 0
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
    //
    // gcd d9/c: read once. This runs on EVERY fallback cycle, and the
    // heap-full livelocks of `gcd-d8x-heap-full-oome-shapes-livelock-on-fallback-young-cycles`
    // ran tens of thousands of them; an uncached flag read per cycle is the
    // waste `CRATONVM_DBG_FLAGREADS` exists to find.
    static REASONS_DBG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *REASONS_DBG.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_GC_FALLBACK_REASONS")
    }) {
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

/// [`enter`] without the [`ENTER_COUNT`] tally (t1 wave 5, lane S;
/// `CRATONVM_JIT_DOOR_THREAD_BLOCK`, opt-in). The depth counter the collector
/// reads moves exactly as in [`enter`]; only the diagnostic count the
/// non-moving sweep's first-corruption line prints is skipped (one locked
/// increment per interpreter-to-compiled door). Pair with [`leave_uncounted`].
#[inline]
pub fn enter_uncounted() -> usize {
    active_depth_enter()
}

/// [`leave`] without the [`LEAVE_COUNT`] tally; see [`enter_uncounted`].
#[inline]
pub fn leave_uncounted() -> usize {
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

/// A compiled frame is live, by either detector: a guarded JIT entry on any
/// thread ([`is_active`]) or a guard-less compiled frame the calling thread's
/// root scan found this pass ([`unregistered_jit_frame_on_stack`], the A5
/// case). The one spelling every collector's "must I honour conservative JIT
/// roots?" gate uses, so no consumer can ask one half and forget the other.
#[inline]
pub fn compiled_frames_live() -> bool {
    is_active() || unregistered_jit_frame_on_stack()
}

/// Frozen or unrewritable native peers held by this VM's current pause.
/// This does not change the JIT entry count or the peer coverage arithmetic.
pub fn frozen_peers_live() -> bool {
    takeover_verdict().frozen > 0
}
/// This pause holds conservative machine words, either from a frozen peer
/// or a complete pinned helper/native band. A pinning collector may spend the
/// latter's licence; Cheney/old-slide guards must still account for it.
pub fn conservative_peers_live() -> bool {
    let verdict = takeover_verdict();
    verdict.frozen > 0 || verdict.helper_windows > 0
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
// collection by default (`-XX:+DisableExplicitGC` opts out, and since gc-common
// w8-g CratonVM honours it per VM: the door returns before this request is
// made). Without this,
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
    /// zt t11/tg4: this thread is running the collection of an allocation
    /// stall (a ZGC forced door no occupancy verdict asked for). See
    /// [`with_allocation_stall_door`].
    static ALLOCATION_STALL_DOOR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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
    /// gci i47w/sg47 -- is this thread's `System.gc()`-door collection in
    /// flight one that tenures every live young object
    /// (`CRATONVM_GC_SYSTEM_GC_TENURES_ALL`)? Armed by
    /// [`request_system_gc_tenure`] next to [`request_major_gc`], read (not
    /// consumed) by the collector inside that door's pause, withdrawn by the
    /// door with [`take_system_gc_tenure_request`] once its collecting loop
    /// ends -- so only the door's own pause (and its refusal-and-retry) reads
    /// it, never a later allocation-triggered collection.
    static SYSTEM_GC_TENURE_REQUESTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// gci i47w/sg47 -- `CRATONVM_GC_SYSTEM_GC_TENURES_ALL` (default ON since the
/// 2026-10-17 user-approved WARNING flip, `=0` kills; marker
/// `WARNING gci-i47w-sg47-flip-20261017`): does the `System.gc()` door's collection
/// end with every movable live young object in the old generation, as
/// HotSpot 25's full collection does (Serial's mark-compact slides young into
/// Tenured, G1's Full GC compacts into old regions)?
///
/// Page `gci-i47w-j46-system-gc-leaves-live-young-data-out-of-the-old-pool`.
/// Exceptions (stay young, as before): an object pinned by a root value or a
/// late pin on the Generational in-place sweep, a sweep whose promotion walk
/// is gated off (`unrewritable_peer_state`, `CRATONVM_NO_SELECTIVE_PROMOTE`),
/// a full old generation, and a G1 copy whose Old allocation failed (the
/// region is kept, an evacuation failure).
pub fn system_gc_tenures_all_enabled() -> bool {
    // WARNING gci-i47w-sg47-flip-20261017: default ON (user-approved flip).
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GC_SYSTEM_GC_TENURES_ALL")
}

/// gci i47w/sg47 -- arm `SYSTEM_GC_TENURE_REQUESTED` for this thread's
/// `System.gc()`-door collection when [`system_gc_tenures_all_enabled`];
/// a no-op otherwise (the default: byte-for-byte the previous behaviour).
pub fn request_system_gc_tenure() {
    if system_gc_tenures_all_enabled() {
        SYSTEM_GC_TENURE_REQUESTED.with(|c| c.set(true));
    }
}

/// gci i47w/sg47 -- does the collection this thread is running belong to a
/// `System.gc()` door that tenures every live young object? Not consumed:
/// the door withdraws it ([`take_system_gc_tenure_request`]).
#[inline]
pub fn system_gc_tenure_requested() -> bool {
    SYSTEM_GC_TENURE_REQUESTED.with(std::cell::Cell::get)
}

/// gci i47w/sg47 -- check-and-clear of [`system_gc_tenure_requested`]; the
/// `System.gc()` door calls it once its collecting loop ends (won, lost or
/// coalesced), so the request never reaches a later collection.
#[inline]
pub fn take_system_gc_tenure_request() -> bool {
    SYSTEM_GC_TENURE_REQUESTED.with(|c| c.replace(false))
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

/// zt t11/tg4 (`t11-x8-report.md` T3, `zgc-zt7x4-*` item 7): run `f` (the
/// forced door's pause) with this thread marked as collecting for an
/// allocation STALL when `stall` -- the VM's classification of the forced
/// site (`VmHeap::zgc_forced_collection_is_stall`), not the door, which every
/// ZGC forced pause shares with the occupancy sites. The previous value is
/// restored after `f`, also on unwind. Read by ZGC's warm-up accounting
/// ([`allocation_stall_door`]).
pub fn with_allocation_stall_door<T>(stall: bool, f: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            ALLOCATION_STALL_DOOR.with(|flag| flag.set(self.0));
        }
    }

    let previous = ALLOCATION_STALL_DOOR.with(|flag| flag.replace(stall));
    let _reset = Reset(previous);
    f()
}

/// zt t11/tg4: whether this thread is inside
/// [`with_allocation_stall_door`] for a stall.
#[inline]
pub fn allocation_stall_door() -> bool {
    ALLOCATION_STALL_DOOR.with(std::cell::Cell::get)
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

/// Does this thread's pending `System.gc()` request FORCE the young half of the
/// collection onto the non-moving sweep?
///
/// The single answer to "may the root gatherer thin the side-table roots on the
/// promise of an in-place `System.gc()`?" and "must the collector keep that
/// promise?" — asked by [`young_marker_follows_side_tables`] (which feeds
/// `VmHeap::mirror_pin_deferrable` and the collection-overlay root scan),
/// `vm::memory::roots::conditional_loader_metadata`, and
/// `GenerationalHeap::collect_garbage_inner`'s `explicit_full_gc` term. One
/// function, so the promise and its keeper cannot disagree.
///
/// `major_gc_requested()` unless `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG` is set
/// (default off: byte-for-byte the previous behaviour). With it set the
/// request still runs the old-generation half (Phase 5 consumes it through
/// [`take_major_gc_request`]); it merely stops deciding whether young copies.
/// See `docs/feature-designs/generative-gc-architecture.md`.
#[inline]
pub fn explicit_full_gc_sweeps_young_in_place() -> bool {
    major_gc_requested() && !gc_flags().gc_system_gc_moving_young
}

/// gci i8/pa — `CRATONVM_GC_SYSTEM_GC_MOVING_SIDE_TABLES` (opt-in, default
/// OFF; P-A of `gengc-core-system-gc-forces-non-moving-20260920`): may a
/// MOVING `System.gc()` (`CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG`) keep the
/// side-table licence the in-place one has?
///
/// Read only when a request is pending under the moving-young opt-in (see
/// [`moving_system_gc_follows_side_tables`]), so every other root scan pays no
/// flag lookup.
fn system_gc_moving_side_tables_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_GC_SYSTEM_GC_MOVING_SIDE_TABLES")
}

/// gci i8/pa — does this thread's pending `System.gc()` take the MOVING young
/// cycle (`CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG`) and still follow the side
/// tables (`CRATONVM_GC_SYSTEM_GC_MOVING_SIDE_TABLES`)?
///
/// When it does, the moving cycle keeps the licence's promise itself: its
/// Phase 1 forwards every YOUNG value of the `metadata_pin` and `mirror_pin`
/// registries as a young-phase root that is NOT a true root of the same
/// cycle's major (`gen_heap::young_side_table_values`), and an OLD deferred
/// value is followed by the major's BFS from its owner (re-keyed by post-copy
/// owner under P-C). So the root gatherer may defer a young value to its owner
/// exactly as on the in-place `System.gc()`. The request is still pending
/// during the root scan and during Phase 1 (Phase 5 consumes it), so both
/// sides read the same answer on the collecting thread.
#[inline]
pub fn moving_system_gc_follows_side_tables() -> bool {
    major_gc_requested()
        && gc_flags().gc_system_gc_moving_young
        && system_gc_moving_side_tables_enabled()
}

/// gci i8/pa — does this thread's pending `System.gc()` license the root
/// gatherer to defer side-table values to their owners? The in-place
/// `System.gc()` ([`explicit_full_gc_sweeps_young_in_place`], the default),
/// or the moving one under `CRATONVM_GC_SYSTEM_GC_MOVING_SIDE_TABLES`
/// ([`moving_system_gc_follows_side_tables`]).
///
/// Read by [`young_marker_follows_side_tables`] and by the VM's
/// `roots::conditional_loader_metadata` / step 14w. The collector's
/// `explicit_full_gc` divert keeps reading
/// [`explicit_full_gc_sweeps_young_in_place`]: the licence no longer implies
/// the in-place sweep, only that the cycle follows the side tables. Without
/// the switch this is exactly [`explicit_full_gc_sweeps_young_in_place`].
#[inline]
pub fn explicit_full_gc_follows_side_tables() -> bool {
    explicit_full_gc_sweeps_young_in_place() || moving_system_gc_follows_side_tables()
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
/// gci i8/pa: one more arm answers `true` without the non-moving marker: the
/// moving `System.gc()` under `CRATONVM_GC_SYSTEM_GC_MOVING_SIDE_TABLES`
/// ([`moving_system_gc_follows_side_tables`]), whose Phase 1 forwards every
/// young side-table value itself. Off that opt-in the answer is unchanged.
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
    // `explicit_full_gc`: certain, and independent of moving-young — unless
    // `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG` withdrew the in-place promise (gen
    // r4w2/youngpolicy), in which case the request no longer diverts and falls
    // through to the remaining certain term.
    // gci i8/pa: or the moving `System.gc()` that follows the side tables
    // itself (`CRATONVM_GC_SYSTEM_GC_MOVING_SIDE_TABLES`, opt-in): its Phase 1
    // forwards every young `mirror_pin` / `metadata_pin` value, so a young
    // value left out of the root set is still copied. Not "the non-moving
    // marker" there, but the same promise: the cycle follows the side tables.
    if explicit_full_gc_follows_side_tables() {
        return true;
    }
    // `has_conservative_roots && !moving_young`: certain only while
    // moving-young is off.
    !moving_young_enabled() && (compiled_frames_live() || conservative_peers_live())
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
    // nio21 wave 17 L3: Fx-hashed (membership only), like the pin registry.
    static MOVABLE_JIT_ROOTS: std::cell::RefCell<rustc_hash::FxHashSet<usize>> =
        std::cell::RefCell::new(rustc_hash::FxHashSet::default());
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
// veto that set. (gci i6/pn: "a root another thread never published as movable
// is already pinned" used to close this sentence, and it is true only of an
// address NO thread published movable. The sweep's pin decision is keyed by
// address over the whole root slice, so once the collecting thread published
// an address movable, a peer's or a fallback band's word naming the same
// address does not pin it unless this set names it too -- see
// `gci-i6pn-remap-audit-finding-on-proven-cycles-is-not-precise-root-promotion-FIXED-20261013.md`.)
// A missed publication leaves the legacy behaviour; a stale entry would only
// over-pin, and the per-pass clear prevents even that.

thread_local! {
    // nio21 wave 17 L3: Fx-hashed (membership only), like the pin registry.
    static UNREWRITABLE_JIT_ROOTS: std::cell::RefCell<rustc_hash::FxHashSet<usize>> =
        std::cell::RefCell::new(rustc_hash::FxHashSet::default());
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
// gci i6/mv: the root-remap audit's census of MAP-DEAD band words
// ---------------------------------------------------------------------------
//
// The movable claim above has two arms for a VERIFIABLE band word
// (`conservative_roots::publish_validated_band_word`): the active oop map
// NAMES the slot, so `remap_one_jit_frame` rewrites it; or the map does NOT
// name it in a region the JIT's dataflow models, so the word is DEAD and
// needs no rewrite. The band scan still ROOTS that dead word (it is a
// conservative scan), so on a cycle that honours the claim the object moves,
// the dead word keeps the old address by design, and the root-remap audit's
// re-scan (`CRATONVM_DBG_ROOT_REMAP_AUDIT`, `vm/src/memory/gc.rs`) roots it
// again and reports it -- the finding of
// `gci-i6pn-remap-audit-finding-on-proven-cycles-is-not-precise-root-promotion-FIXED-20261013.md`.
//
// This census lets the audit tell those words from a real miss. The audit
// arms it around its own re-scan only; the band scan then records, per rooted
// band word, its value and whether the frame's active map proves it dead. The
// audit skips a vacated address only when EVERY occurrence of it in the
// re-scan is such a word (`BandRootCensus::only_map_dead`), so a live or
// unclassified word naming the same address is still reported. Never armed
// outside the audit: the collection's own scan pays one thread-local read per
// compiled frame and nothing per word.

/// Per-address counts of the band words one root scan rooted: all of them,
/// and those the frame's active oop map proves dead.
#[derive(Debug, Default)]
pub struct BandRootCensus {
    rooted: rustc_hash::FxHashMap<usize, u32>,
    map_dead: rustc_hash::FxHashMap<usize, u32>,
}

impl BandRootCensus {
    /// Record one rooted band word naming `addr`; `map_dead` when the active
    /// map proves the word dead.
    pub fn note(&mut self, addr: usize, map_dead: bool) {
        *self.rooted.entry(addr).or_insert(0) += 1;
        if map_dead {
            *self.map_dead.entry(addr).or_insert(0) += 1;
        }
    }

    /// Band words naming `addr` the scan rooted.
    pub fn rooted(&self, addr: usize) -> u32 {
        self.rooted.get(&addr).copied().unwrap_or(0)
    }

    /// Of those, the words the active map proves dead.
    pub fn map_dead(&self, addr: usize) -> u32 {
        self.map_dead.get(&addr).copied().unwrap_or(0)
    }

    /// Is every one of the `occurrences` of `addr` in the scanned root list a
    /// map-dead band word? Over-reports, never under-reports: any other band
    /// word naming `addr`, any other source naming it, or a list that dropped
    /// or merged entries makes the counts disagree, and the answer is `false`.
    pub fn only_map_dead(&self, addr: usize, occurrences: u32) -> bool {
        let dead = self.map_dead(addr);
        dead > 0 && self.rooted(addr) == dead && occurrences == dead
    }
}

thread_local! {
    static BAND_ROOT_CENSUS: std::cell::RefCell<Option<BandRootCensus>> =
        const { std::cell::RefCell::new(None) };
}

/// Arm the calling thread's [`BandRootCensus`] (empty). The root-remap audit
/// calls this right before its re-scan.
pub fn arm_band_root_census() {
    let _ = BAND_ROOT_CENSUS.try_with(|c| {
        if let Ok(mut v) = c.try_borrow_mut() {
            *v = Some(BandRootCensus::default());
        }
    });
}

/// Is the calling thread's census armed? Read once per compiled frame.
#[inline]
pub fn band_root_census_armed() -> bool {
    BAND_ROOT_CENSUS
        .try_with(|c| c.try_borrow().map(|v| v.is_some()).unwrap_or(false))
        .unwrap_or(false)
}

/// Record one rooted band word in the armed census; a no-op unarmed.
pub fn note_band_root_census(addr: usize, map_dead: bool) {
    let _ = BAND_ROOT_CENSUS.try_with(|c| {
        if let Ok(mut v) = c.try_borrow_mut() {
            if let Some(census) = v.as_mut() {
                census.note(addr, map_dead);
            }
        }
    });
}

/// Disarm the calling thread's census and return what it recorded (`None`
/// when it was not armed).
pub fn take_band_root_census() -> Option<BandRootCensus> {
    BAND_ROOT_CENSUS
        .try_with(|c| c.try_borrow_mut().ok().and_then(|mut v| v.take()))
        .ok()
        .flatten()
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

/// The per-thread pin registry's shape. nio21 wave 17 L3: Fx-hashed, both
/// levels. Every blocking deposit from compiled code whose pin set differs
/// from its previous one (a pipe round trip alternates two sets) rebuilt the
/// inner set with `RandomState` (SipHash-1-3): `HashMap<usize, (),
/// RandomState>::insert` + `hash_one` + `Sip13Rounds::write` were ~5% of the
/// `Nio21UnsafeVolatile pipe` row (w16 profile). Membership and iteration
/// only; no consumer depends on the hasher (the snapshot sorts nothing and
/// the order of `pinned_jit_roots_snapshot` was already unspecified).
type PinnedJitMap = rustc_hash::FxHashMap<std::thread::ThreadId, rustc_hash::FxHashSet<usize>>;

static PINNED_JIT_ROOTS_BY_THREAD: std::sync::OnceLock<std::sync::Mutex<PinnedJitMap>> =
    std::sync::OnceLock::new();

/// Lock a pin registry, recovering from poisoning instead of failing open.
///
/// Every mutator of the pin registries used to be written `if let Ok(mut map) =
/// ..lock()`, which silently does NOTHING once the mutex is poisoned — and
/// `pinned_jit_roots_snapshot` silently returned an EMPTY vector. That is the
/// unsound direction. (`jit_depth_of_tid` and `register_self_jit_depth_slot`
/// used to degrade to `None` / a detached slot as the safe direction; since
/// the never-published helper-window rule reads `None` as "no compiled frame",
/// they recover the poison too -- gci i5/tk.) The pin registry's degradation is
/// the opposite — an empty snapshot tells G1 that no conservative JIT root
/// needs a region kept out of the collection set, so it evacuates the objects a
/// frozen peer's unrewritable frames still name.
///
/// A poisoned mutex here means some thread panicked inside a `HashMap`
/// insert/remove/clone, so the map may be mid-mutation but it is not
/// *unsound* to read: the worst case is a pin published a moment ago is
/// missing, which is the same over/under-pin risk a racing publish already
/// carries. Recovering the guard keeps every later cycle pinning, which is
/// strictly better than every later cycle pinning nothing.
fn recover<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn pinned_jit_map() -> &'static std::sync::Mutex<PinnedJitMap> {
    PINNED_JIT_ROOTS_BY_THREAD.get_or_init(|| std::sync::Mutex::new(PinnedJitMap::default()))
}

/// TLS guard: removes this thread's pin-registry entry when the thread exits,
/// so a dead thread's regions do not stay pinned forever.
///
/// gci i17/dk (i16/dp D3): it also carries `published`, this thread's own
/// record of the entry it last put in the map, so an unchanged republication
/// can return without the process-global lock (see
/// [`published_pins_answer`]). A field of the existing guard, not a new
/// thread-local.
struct PinnedJitRootsGuard {
    tid: std::thread::ThreadId,
    published: std::cell::RefCell<PublishedPins>,
}
impl Drop for PinnedJitRootsGuard {
    fn drop(&mut self) {
        recover(pinned_jit_map()).remove(&self.tid);
    }
}

/// gci i17/dk: what the calling thread last published into its own entry of
/// the pin map. Only the owning thread ever inserts, replaces or removes its
/// entry (`publish_pinned_jit_roots`, `add_pinned_jit_root`,
/// `clear_pinned_jit_roots`, the guard's drop), and no collector rewrites it,
/// so while `exact` holds the entry is, as a set, exactly `addrs`.
/// `exact` drops on every other change (an empty publication, an add, a
/// clear, a publication longer than [`PIN_REPUBLISH_COMPARE_MAX`]).
#[derive(Default)]
struct PublishedPins {
    exact: bool,
    addrs: Vec<usize>,
}

/// gci i17/dk: does the calling thread's entry already hold exactly the set
/// `addrs` names? Lock-free (thread-local only); `false` whenever the record
/// is not exact, the guard is not armed or the TLS is being torn down, which
/// sends the caller to the locked path (always correct).
fn published_pins_answer(addrs: &[usize]) -> bool {
    PINNED_JIT_ROOTS_GUARD
        .try_with(|g| {
            g.get().is_some_and(|g| {
                g.published
                    .try_borrow()
                    .is_ok_and(|p| p.exact && same_pin_set(&p.addrs, addrs))
            })
        })
        .unwrap_or(false)
}

/// Set equality of two pin lists (duplicates allowed in either).
fn same_pin_set(prev: &[usize], addrs: &[usize]) -> bool {
    addrs.iter().all(|a| prev.contains(a)) && prev.iter().all(|p| addrs.contains(p))
}

/// gci i17/dk: record what the calling thread's entry now holds: `Some(addrs)`
/// right after the entry was set to the set of `addrs`, `None` after any
/// other change. Reuses the record's buffer (no allocation once it has grown
/// to [`PIN_REPUBLISH_COMPARE_MAX`]).
fn note_published_pins(addrs: Option<&[usize]>) {
    let _ = PINNED_JIT_ROOTS_GUARD.try_with(|g| {
        if let Some(g) = g.get() {
            if let Ok(mut p) = g.published.try_borrow_mut() {
                p.addrs.clear();
                match addrs {
                    Some(a) if !a.is_empty() && a.len() <= PIN_REPUBLISH_COMPARE_MAX => {
                        p.addrs.extend_from_slice(a);
                        p.exact = true;
                    }
                    _ => p.exact = false,
                }
            }
        }
    });
}
thread_local! {
    static PINNED_JIT_ROOTS_GUARD: std::cell::OnceCell<PinnedJitRootsGuard> =
        const { std::cell::OnceCell::new() };
    /// Whether a JIT-root scan on THIS thread records provenance strings. See
    /// [`jit_root_provenance_wanted`].
    static JIT_ROOT_PROVENANCE_ARMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Whether THIS thread may currently own an entry in the process-global
    /// pin map. A hint in one direction only: `true` means "take the lock and
    /// look", `false` is written only after this thread itself removed (or
    /// never inserted) its entry, and no other thread ever inserts under this
    /// thread's key — so a `false` is exact.
    ///
    /// It exists for the empty publication. `update_root_snapshot` publishes
    /// on every call — every object-returning native call, every blocked
    /// deposit, every safepoint park — and on a thread with no live compiled
    /// frame that publication is EMPTY and finds nothing to remove. It used
    /// to take the process-global `std::sync::Mutex` anyway (plus a
    /// `thread::current()` `Arc` round trip), so every native call on every
    /// thread serialised on one lock to do nothing.
    static PINNED_JIT_ROOTS_MAY_OWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
fn arm_pinned_guard() {
    PINNED_JIT_ROOTS_GUARD.with(|g| {
        let _ = g.get_or_init(|| PinnedJitRootsGuard {
            tid: std::thread::current().id(),
            published: std::cell::RefCell::new(PublishedPins::default()),
        });
    });
}

static JIT_ROOT_PROVENANCES: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<usize, String>>,
> = std::sync::OnceLock::new();

fn jit_root_provenances() -> &'static std::sync::Mutex<std::collections::HashMap<usize, String>> {
    JIT_ROOT_PROVENANCES.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

pub fn record_jit_root_provenance(addr: usize, prov: String) {
    recover(jit_root_provenances()).insert(addr, prov);
}

/// Where a band word the root screen REJECTED was read (its slot and frame),
/// keyed by the pin address it owes. Diagnostic only: written by the band
/// scans under `CRATONVM_DBG_JIT_ROOTSCAN=1`, read by G1's remark census to
/// name the frame behind each orphan. Unlike the root provenance map it is not
/// cleared per collection (a parked peer deposits before the initiator's
/// clear); it is emptied wholesale when it grows past a bound.
static JIT_REJECT_PROVENANCES: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<usize, String>>,
> = std::sync::OnceLock::new();

const JIT_REJECT_PROVENANCE_CAP: usize = 1 << 16;

fn jit_reject_provenances() -> &'static std::sync::Mutex<std::collections::HashMap<usize, String>> {
    JIT_REJECT_PROVENANCES.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Record the slot provenance of a rejected band word whose pin is `addr`.
pub fn record_jit_reject_provenance(addr: usize, prov: String) {
    let mut map = recover(jit_reject_provenances());
    if map.len() >= JIT_REJECT_PROVENANCE_CAP {
        map.clear();
    }
    map.insert(addr, prov);
}

/// The provenance [`record_jit_reject_provenance`] last recorded for `addr`.
pub fn lookup_jit_reject_provenance(addr: usize) -> Option<String> {
    recover(jit_reject_provenances()).get(&addr).cloned()
}

/// JIT round 21 lane refs (`CRATONVM_DBG_REFERENT_HOLDER`, diagnostic only):
/// where a conservative root word was read by a scan that ran OUTSIDE a
/// collection's armed root pass -- a blocking deposit on the depositing thread,
/// the cross-thread helper-window pass over a blocked peer's stack -- keyed by
/// the object it rooted. Not cleared per collection (those scans run before or
/// beside the initiator's clear of the root provenance map); emptied wholesale
/// past a bound, like the reject map above. Written only under the switch.
static JIT_HOLDER_PROVENANCES: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<usize, String>>,
> = std::sync::OnceLock::new();

fn jit_holder_provenances() -> &'static std::sync::Mutex<std::collections::HashMap<usize, String>> {
    JIT_HOLDER_PROVENANCES.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Record the last unarmed-scan provenance of root `addr` (see above).
pub fn record_jit_holder_provenance(addr: usize, prov: String) {
    let mut map = recover(jit_holder_provenances());
    if map.len() >= JIT_REJECT_PROVENANCE_CAP {
        map.clear();
    }
    map.insert(addr, prov);
}

/// The provenance [`record_jit_holder_provenance`] last recorded for `addr`.
pub fn lookup_jit_holder_provenance(addr: usize) -> Option<String> {
    recover(jit_holder_provenances()).get(&addr).cloned()
}

pub fn lookup_jit_root_provenance(addr: usize) -> Option<String> {
    recover(jit_root_provenances()).get(&addr).cloned()
}

pub fn clear_jit_root_provenances() {
    recover(jit_root_provenances()).clear();
}

/// gci i41/hc: while `CRATONVM_G1_DBG_CLOSURE_ROOT_ENTRIES` is on, the
/// provenance a parked peer's deposit recorded (under
/// `CRATONVM_DBG_OLDMARK_ROOT_CENSUS`, before this pause's clear) is kept for
/// the closure census (`vm/src/memory/roots.rs`); last write per address
/// wins; emptied when it reaches the reject map's bound. Diagnostic only.
fn keep_jit_root_provenances() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_G1_DBG_CLOSURE_ROOT_ENTRIES")
        && recover(jit_root_provenances()).len() < JIT_REJECT_PROVENANCE_CAP
}

/// Should a conservative JIT-root scan on the calling thread record where each
/// root came from ([`record_jit_root_provenance`])?
///
/// Only between [`clear_pinned_jit_roots`] and the same thread's next
/// [`publish_pinned_jit_roots`], i.e. during a collection's own root scan.
/// The one reader (G1's non-object-root warning) looks roots up during that
/// collection, and the provenance map is emptied wholesale when the
/// collection starts (`clear_pinned_jit_roots`). So a record made by any
/// earlier scan (a peer's deposit at its safepoint park, or the snapshot scan
/// of an object-returning native call) is erased before anything can read it.
/// Those scans used to pay a `format!` and a process-wide lock per root for
/// nothing.
pub fn jit_root_provenance_wanted() -> bool {
    JIT_ROOT_PROVENANCE_ARMED
        .try_with(|c| c.get())
        .unwrap_or(false)
}

/// Clear the CALLING thread's conservative-pinned-JIT-root entry. Called by
/// the VM's root gatherer (initiator) at the start of every collection,
/// before its own JIT-frame scan republishes. Other threads' entries are
/// left intact — they are owned by those threads' deposits.
pub fn clear_pinned_jit_roots() {
    if !keep_jit_root_provenances() {
        clear_jit_root_provenances();
    }
    let _ = JIT_ROOT_PROVENANCE_ARMED.try_with(|c| c.set(true));
    recover(pinned_jit_map()).remove(&std::thread::current().id());
    let _ = PINNED_JIT_ROOTS_MAY_OWN.try_with(|c| c.set(false));
    note_published_pins(None);
}

/// Record `addr` (an object address discovered conservatively in a JIT frame,
/// whose holder slot the collector cannot rewrite) as pin-required for G1,
/// owned by the calling thread.
pub fn add_pinned_jit_root(addr: usize) {
    arm_pinned_guard();
    // Once per ADDRESS on this path, once per CALL on `publish_pinned_jit_roots`
    // -- see `conservative_jit_scans`, which documents the mixed unit and why
    // only its non-zero-ness is relied on.
    conservative_jit_scans_bump();
    // `recover`, NOT `if let Ok(..)`. This is the one op in the file whose
    // lock-poisoning behaviour is fail-OPEN: dropping the insert loses a pin,
    // and `pinned_jit_roots_snapshot()` then tells G1 that no region needs
    // keeping out of the collection set — so it evacuates objects a frozen
    // peer's unrewritable compiled frames still name. Every other accessor
    // here fails closed and says so. Recovering the poisoned guard keeps the
    // registry authoritative instead of silently empty.
    let mut map = recover(pinned_jit_map());
    // Raised under the lock, before the insert, so the empty-publication fast
    // path in `publish_pinned_jit_roots` can never read `false` while an entry
    // exists.
    let _ = PINNED_JIT_ROOTS_MAY_OWN.try_with(|c| c.set(true));
    // gci i17/dk: the entry is no longer the last publication's set.
    note_published_pins(None);
    map.entry(std::thread::current().id())
        .or_default()
        .insert(addr);
}

/// Replace the CALLING thread's pin entry wholesale with `addrs` (removing it
/// when empty). Used by the root-snapshot deposit paths so a thread's pins
/// always reflect its CURRENT live JIT frames.
pub fn publish_pinned_jit_roots(addrs: &[usize]) {
    arm_pinned_guard();
    // This thread's scan is over; see `jit_root_provenance_wanted`.
    let _ = JIT_ROOT_PROVENANCE_ARMED.try_with(|c| c.set(false));
    // BEFORE the emptiness test below. "This thread looked and found nothing"
    // and "this thread never looked" are different facts and the map cannot
    // hold the difference -- see `conservative_jit_scans`.
    conservative_jit_scans_bump();
    // Fast path (gc-common 2026-09-23, lane A): an empty publication from a
    // thread that owns no entry changes nothing, so it does not need the
    // process-global lock. The count above is still bumped — an empty look is
    // still a look. `try_with` failing (TLS teardown) falls through to the
    // locked path, which is always correct.
    if addrs.is_empty() && matches!(PINNED_JIT_ROOTS_MAY_OWN.try_with(|c| c.get()), Ok(false)) {
        return;
    }
    // gci i17/dk (i16/dp D3): the non-empty twin of the fast path above. A
    // republication of exactly the set this thread's entry holds (its own
    // record says so: only this thread changes its entry) leaves the entry as
    // the locked compare below would -- untouched -- without the
    // process-global lock and the `thread::current()` round trip. The JNI
    // in-native package republishes one unchanged handful per native call
    // from a compiled loop.
    if !addrs.is_empty()
        && addrs.len() <= PIN_REPUBLISH_COMPARE_MAX
        && published_pins_answer(addrs)
    {
        return;
    }
    // Same fail-open hazard as `add_pinned_jit_root`, same repair: a dropped
    // publication is a lost pin set, which reads to G1 as "nothing to keep".
    let mut map = recover(pinned_jit_map());
    let tid = std::thread::current().id();
    if addrs.is_empty() {
        map.remove(&tid);
        let _ = PINNED_JIT_ROOTS_MAY_OWN.try_with(|c| c.set(false));
        note_published_pins(None);
    } else {
        let _ = PINNED_JIT_ROOTS_MAY_OWN.try_with(|c| c.set(true));
        // gcf f1/j: a republication of the set this thread published last
        // leaves the entry as it is. The JNI in-native package deposits once
        // per native call, and a compiled loop calling a native republishes
        // one unchanged handful each time (G1 `dep_jit_pins` 322 ns). Compared
        // without building a set, up to a bound past which the replace below
        // is as cheap as the comparison. Same entry either way.
        let unchanged = addrs.len() <= PIN_REPUBLISH_COMPARE_MAX
            && map.get(&tid).is_some_and(|prev| {
                prev.len() <= addrs.len()
                    && addrs.iter().all(|a| prev.contains(a))
                    && prev.iter().all(|p| addrs.contains(p))
            });
        if !unchanged {
            map.insert(tid, addrs.iter().copied().collect());
        }
        // gci i17/dk: the entry is now exactly the set of `addrs` (either
        // arm), recorded for the lock-free compare above (not past the
        // compare bound).
        note_published_pins(Some(addrs));
    }
}

/// gcf f1/j: the largest publication [`publish_pinned_jit_roots`] compares
/// with the calling thread's previous one before replacing it.
const PIN_REPUBLISH_COMPARE_MAX: usize = 32;

/// Conservative JIT-root PUBLICATION EVENTS since
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
/// # Its UNIT, which is not what the name used to promise
///
/// This said "how many THREADS have published" until 2026-09-20, and that is
/// false on one of the two publication paths:
///
/// * [`publish_pinned_jit_roots`] bumps it **once per call**, including a call
///   with an EMPTY slice -- "this thread looked and found nothing" is exactly
///   the fact that has to be distinguishable. One bump, one thread.
/// * [`add_pinned_jit_root`] bumps it **once per ADDRESS**. Its live callers
///   (`vm/src/runtime/interpreter/gc_and_alloc.rs:973,980`) call it in a loop,
///   so one thread's scan of forty roots contributes forty.
///
/// So the magnitude is a mix of two units and must not be read as a thread
/// count, a root count, or a scan count. The only property either in-tree
/// consumer relies on -- `gen_heap::collect_garbage_inner`'s
/// `conservative_jit_scans() > 0` and `zgc.rs`'s `== 0` -- is whether it is
/// non-zero, and that IS well defined on both paths.
///
/// # The reset-ordering hazard, closed by the request-time open
///
/// Every other per-pause ledger in this module is cleared at BOTH points that
/// open a pause ([`reset_peer_proven_jit_depth`], called from the barrier's
/// `request_stw`, and [`begin_moving_young_coverage_cycle`]). This one is
/// cleared only by the second, which the initiator runs during root gathering
/// -- i.e. AFTER cooperatively parked peers have already deposited and bumped.
/// Their bumps are therefore discarded, and only the initiator's own
/// publication keeps the count non-zero. See
/// `docs/feature-designs/generative-gc-architecture.md`.
///
/// **Still open after round 2 (2026-09-20), deliberately.** All three candidate
/// repairs move *when* this counter reads zero, and this counter is not only
/// the generational collector's: `zgc.rs:5504` refuses on
/// `is_active() && conservative_jit_scans() == 0`. Moving the clear earlier
/// makes that refusal fire on cycles where it does not fire today, which is a
/// behaviour change to a backend neither this round nor its owner was
/// chartered to alter. The cheap-looking variant — clear it from
/// [`reset_peer_proven_jit_depth`] as well — is *not* a fix on its own and is
/// worth stating because it looks like one:
/// [`begin_moving_young_coverage_cycle`] still runs later and still erases.
/// And the "stop clearing it here" variant is only safe if the barrier's
/// `request_stw` runs before **every** generational young collection; if any
/// door skips it the count never resets and the collector diverts to the
/// non-moving sweep for the rest of the process. That is the safe direction
/// but a throughput cliff, and establishing it needs a run, not a read.
/// The characterization test
/// `tests::conservative_jit_scans_are_erased_by_the_coverage_cycle_open` pins
/// the current behaviour so the fix cannot land silently.
///
/// **Closed 2026-09-24 (gen r4w5/pinwords5), without touching this counter.**
/// The premise above stopped holding at gc-common w2-a: the only production
/// caller of [`begin_moving_young_coverage_cycle`] is now the barrier's
/// `request_stw_opening_cycle`, which runs it under the barrier lock BEFORE
/// `stw_requested` is visible, so no peer of the pause has deposited yet and
/// none of their bumps is erased. What the characterization test still pins —
/// a bump made before the open is cleared by it — is now the intended
/// behaviour. A consumer that reads a per-pause ledger for its MAGNITUDE uses
/// the pause stamp instead ([`pause_young_pin_words`]); this counter is still
/// only ever read for non-zero-ness. See
/// `docs/feature-designs/generative-gc-architecture.md`.
pub fn conservative_jit_scans() -> usize {
    conservative_jit_scans_get()
}

/// Record that the calling thread ran a conservative JIT-root scan WITHOUT
/// publishing its pins — the one bump [`publish_pinned_jit_roots`] makes,
/// and nothing else (the pin map is not touched).
///
/// gen r4w3/young2 (2026-09-23). Its one caller is the initiator's arm in
/// `vm/src/memory/roots.rs::collect_roots` that `CRATONVM_GC_G1_ONLY_JIT_PINS`
/// makes skip the publication on a GENERATIONAL heap. Without it the count
/// read zero there on a pause no parked peer published into (peers' bumps
/// were erased by [`begin_moving_young_coverage_cycle`] before w2-a moved
/// that open to the request; see [`conservative_jit_scans`]), and the
/// generational collector's `conservative_jit_scans() > 0` divert term went
/// false behind live unrewritable peer words. With it the count is what the default path
/// produces. Not called on ZGC: ZGC pins by value and refuses on `== 0`, and
/// with its pins withheld "nobody published" is the truthful answer there.
/// See `docs/feature-designs/generative-gc-architecture.md`.
pub fn note_conservative_jit_scan() {
    conservative_jit_scans_bump();
}

// Conservative JIT-root publication events this cycle. See
// [`conservative_jit_scans`].
//
// The `scans` row of the bound ledger's [`CoverageCycle`] since gc-common
// w21-e (the `CONSERVATIVE_JIT_SCANS` process static until then, thread-local
// under `cfg(test)`). A bump lands in the calling thread's bound ledger, else in
// the orphan rows; a read sums the two. Per VM, another VM's mutators can no
// longer make this VM's count non-zero and so defeat ZGC's `== 0` refusal for
// it -- the one non-conservative cross-VM write the w6-a coverage slot never
// covered. The `cfg(test)` isolation is kept: the orphan rows are then the
// calling thread's own fallback's, so one gc test opening a coverage cycle
// cannot flip another test's `> 0` / `== 0` collector decision.

#[inline]
fn conservative_jit_scans_get() -> usize {
    let mut sum = 0usize;
    coverage_each(|c| sum = sum.saturating_add(c.scans.load(Ordering::Relaxed)));
    sum
}

#[inline]
fn conservative_jit_scans_bump() {
    coverage_write(CoverageCycle::bump_scans);
}

/// gkr (`T1W13BigAfterChurn big-then-chain` on ZGC): [`pinned_jit_roots_snapshot`]
/// without the CALLING thread's words its own band scan claimed movable and
/// nothing vetoed ([`is_movable_jit_root`], [`is_unrewritable_jit_root`]) --
/// G1's rule (`g1_kept_pin_words`) and the generational one. A movable claim
/// says every band sighting of the object is rewritten by the frame remap or
/// is dead, so moving it needs no pin. `honour` false is the plain snapshot.
/// The claims are the calling thread's thread-locals, so only its own words
/// can be released; every other publisher's stay.
pub fn pinned_jit_roots_snapshot_honouring_movable(honour: bool) -> Vec<usize> {
    if !honour {
        return pinned_jit_roots_snapshot();
    }
    let (own, mut others) = pinned_jit_roots_snapshot_split();
    others.extend(
        own.into_iter()
            .filter(|&a| !is_movable_jit_root(a) || is_unrewritable_jit_root(a)),
    );
    others
}

/// Snapshot the conservative-pinned-JIT-root addresses published by ALL
/// threads. `G1Collector` maps these to regions it must exclude from the
/// collection set.
pub fn pinned_jit_roots_snapshot() -> Vec<usize> {
    // `recover`, NOT `Err(_) => Vec::new()`: an empty snapshot is a licence to
    // evacuate every region, so losing the set to a poisoned mutex is the one
    // degradation in this file that corrupts rather than refuses. See `recover`.
    let mut out: Vec<usize> = recover(pinned_jit_map())
        .values()
        .flat_map(|s| s.iter().copied())
        .collect();
    // Plus the peers nobody could publish FOR: this cycle's helper-window pins,
    // the calling thread's bound ledger's and the orphan rows' (see
    // `add_xt_cycle_pinned_jit_roots`).
    coverage_each(|c| out.extend_from_slice(&c.xt_pins.lock()));
    out
}

/// gcg g3/e (hunk H1, applied by gch h1/yp) -- [`pinned_jit_roots_snapshot`]
/// split by publisher: the CALLING thread's own entry, and everything else
/// (every other thread's entry and this cycle's helper-window words). The
/// unrewritable-only pinned compaction may unpin only the caller's words: the
/// movable / unrewritable sets it consults are the caller's thread-locals
/// (`gcg-g3e-unrewritable-only-plan-unpins-words-other-threads-published`).
/// The union of the two halves is exactly [`pinned_jit_roots_snapshot`].
pub fn pinned_jit_roots_snapshot_split() -> (Vec<usize>, Vec<usize>) {
    let me = std::thread::current().id();
    let mut own: Vec<usize> = Vec::new();
    let mut others: Vec<usize> = Vec::new();
    {
        // `recover`, as in `pinned_jit_roots_snapshot`: a lost set is a licence.
        let map = recover(pinned_jit_map());
        for (tid, s) in map.iter() {
            if *tid == me {
                own.extend(s.iter().copied());
            } else {
                others.extend(s.iter().copied());
            }
        }
    }
    coverage_each(|c| others.extend_from_slice(&c.xt_pins.lock()));
    (own, others)
}

/// Count of conservative-pinned JIT roots currently published (diagnostics).
pub fn pinned_jit_root_count() -> usize {
    let per_thread: usize = recover(pinned_jit_map()).values().map(|s| s.len()).sum();
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
//
// PER VM since gc-common w21-e: the `xt_pins` row of the pause's
// [`CoverageCycle`] (it was the `XT_CYCLE_PINNED_JIT_ROOTS` process static).
// The READ audit that makes the move exact: every reader of
// [`pinned_jit_roots_snapshot`] / [`pinned_jit_root_count`] runs on the
// collecting thread, at the top level of the collection, never in a worker
// closure --
//
// * G1: `collect_garbage` (`empty_jit_publication`,
//   `retry_after_evacuation_failure`), `young_collection_serial` /
//   `young_collection_parallel` / `mixed_collection` /
//   `mixed_collection_parallel` (`pinned_region_set_including_non_object_roots`
//   -> `jit_pinned_region_set`, and `describe_pin_set`, all before any
//   `pool.scope`);
// * ZGC: `relocate_stw_admitted`, from `relocate_stw` / `collect_garbage`
//   (`begin_page_evac_cycle` reads it too, and has no production caller);
// * Generational: `collect_garbage_inner_with_pins` (the conservative-divert
//   sample), `OldPinnedCompact::for_this_pause`, and the opt-in
//   `oldmark_census::run` inside `major_gc` on the young cycle's thread.
//
// No `zgc-concurrent-mark` driver, `zgc_par_chunks`, `g1-concurrent-mark-*`,
// `EvacPool` or young-wipe thread reads it (`rg "gc_quiescence::"` over their
// files finds none of these functions). The collecting thread is the requester
// (`run_collection_pause`), bound from its winning request to `complete_gc`.
// The writers are the requester's take-over and helper-window passes and, under
// the opt-in `CRATONVM_JIT_PIN_UNNAMED_FRAME_REFS`, any thread's own oop-map
// scan; a writer with no binding lands in the orphan rows, which every read
// includes, so no pin is lost.

/// Pin `addrs` for the remainder of this collection.
///
/// The caller must have recovered them from a COMPLETE conservative scan of the
/// peer -- its register file AND its whole readable stack band. A partial scan
/// must keep refusing the cycle instead: pinning what you found does not help
/// when what you missed is also unrewritable.
///
/// Lands in the calling thread's bound [`PauseLedger`], else in the orphan rows
/// (gc-common w21-e).
pub fn add_xt_cycle_pinned_jit_roots(addrs: &[usize]) {
    if addrs.is_empty() {
        return;
    }
    coverage_write(|c| c.add_xt_pins(addrs));
}

/// Drop this cycle's helper-window pins: the bound ledger's and the orphan
/// rows'. Called from [`begin_moving_young_coverage_cycle`] and
/// [`reset_peer_proven_jit_depth`].
pub fn clear_xt_cycle_pinned_jit_roots() {
    coverage_each(CoverageCycle::clear_xt_pins);
}

/// How many helper-window pins the current cycle published (diagnostics): the
/// bound ledger's plus the orphan rows'.
pub fn xt_cycle_pinned_jit_root_count() -> usize {
    let mut n = 0usize;
    coverage_each(|c| n = n.saturating_add(c.xt_pins.lock().len()));
    n
}

/// The current cycle's cross-thread pins alone (the bound ledger's plus the
/// orphan rows'): the half [`pinned_jit_roots_snapshot`] adds to the
/// per-thread publications. Diagnostics (gci i24/jp, G1's `by_xt` column of
/// `[GC] g1 pin-attribution:`); read on the collecting thread, as every reader
/// of this set is.
pub fn xt_cycle_pinned_jit_roots_snapshot() -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    coverage_each(|c| out.extend_from_slice(&c.xt_pins.lock()));
    out
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
//
// gci i21/ws (`gcg-g2r-proposal-watched-set-built-once-per-pause-with-siphash`
// steps 1-2, default path: a pure data-structure change, every consumer asks
// membership or sorts what it iterates): the set is an `FxHashSet` (the keys
// are heap addresses this process minted, the reason `reference.rs` moved
// every processor table to Fx in gc-common w1-d; it was SipHash-1-3, up to
// three inserts per reference row per pause and one SipHash per survivor the
// young sweep asks about) behind an `Arc`, so [`watched_referents_snapshot`]
// is a reference-count bump instead of a full copy of the set (it was cloned
// once per young sweep and once per true-root / walked major). Revives the
// gcg g3/rw patch (`docs/feature-designs/generative-gc-architecture.md`),
// withdrawn then only because a type change cannot be opt-in.
//
// gcj j1/rf (`common-watched-referent-set-is-per-thread-not-per-heap-20261012`
// section 2): the thread's set is a small TABLE of publications keyed by the
// publishing heap ([`WatchedTable`]), not one slot. A Generational heap reads
// the row of its own key ([`watched_referents_snapshot_for`]; the key is its
// old generation's base, `GenerationalHeap::watched_set_key`), so a second
// VM's publication on the same thread -- nested inside the first VM's window
// between its publish and its collection -- can neither replace the first
// VM's set nor be read by the first VM's collection. The unkeyed calls keep
// their meaning exactly: an unkeyed publish ([`set_watched_referents`]) resets
// the table to one key-0 row (what the kill switch
// `CRATONVM_GC_WATCHED_SET_PER_HEAP=0` and every test publish), an unkeyed add
// or read answers for the most recent publication, and a keyed read with no
// row of its own falls back to the key-0 row. With one VM per thread the
// table holds one row and every reader sees what it saw before.
thread_local! {
    static WATCHED_REFERENTS: std::cell::RefCell<WatchedTable> =
        const { std::cell::RefCell::new(WatchedTable::new()) };
}

/// gcj j1/rf — the key of an UNKEYED publication ([`set_watched_referents`]),
/// and the row a keyed reader falls back to when its own key has none.
pub const WATCHED_UNKEYED: usize = 0;

/// gcj j1/rf — at most this many heaps' publications are held per thread; a
/// publication for a new key evicts the least recently published row. Only a
/// thread interleaving more than this many VMs' collections can evict a row
/// between its publish and its read.
const WATCHED_TABLE_ROWS: usize = 8;

/// gcj j1/rf — one thread's watched-referent publications, one row per
/// publishing heap, least recently published first.
struct WatchedTable {
    rows: Vec<(usize, std::sync::Arc<WatchedSet>)>,
    /// The key of the most recent publication: the row the unkeyed add and
    /// reads answer for.
    current: usize,
    /// gcj j3/rf3 (`CRATONVM_GEN_WATCHED_SET_REUSE_UNCHANGED`, opt-in): the
    /// memo of each row last published through [`Self::publish_reusing`]
    /// ([`WatchedRowMemo`]). A row published any other way has none.
    memos: Vec<(usize, WatchedRowMemo)>,
    /// gcj j3/rf3 (`CRATONVM_GC_STRING_TABLE_SEED_ONLY_YOUNG`, opt-in): each
    /// heap's SEED-ONLY row, addresses only a major's young seed and its
    /// watched-dead verdict read ([`watched_major_snapshot_for`]); the young
    /// sweeps read the watched row alone. Cleared whenever its key's watched
    /// row is published, dropped with it.
    seed_only: Vec<(usize, std::sync::Arc<WatchedSet>)>,
}

/// gcj j3/rf3 — what a row published through [`WatchedTable::publish_reusing`]
/// holds beyond its set: the exact input it was built from, and every address
/// a later [`WatchedTable::add`] inserted that was NOT already in the set
/// (`insert` answered `true`). Invariant while the memo exists: the row's set
/// is exactly `input` plus `added`, and no `added` address is in `input`. So a
/// next publication of the same `input` restores the set by removing `added`,
/// instead of clearing it and inserting every address again.
struct WatchedRowMemo {
    input: Vec<usize>,
    added: Vec<usize>,
}

impl WatchedTable {
    const fn new() -> Self {
        Self {
            rows: Vec::new(),
            current: WATCHED_UNKEYED,
            memos: Vec::new(),
            seed_only: Vec::new(),
        }
    }

    fn position(&self, key: usize) -> Option<usize> {
        self.rows.iter().position(|(k, _)| *k == key)
    }

    fn row(&self, key: usize) -> Option<&std::sync::Arc<WatchedSet>> {
        self.position(key).map(|i| &self.rows[i].1)
    }

    /// The row a reader for `key` answers from: its own, else the key-0 row.
    fn row_for_reader(&self, key: usize) -> Option<&std::sync::Arc<WatchedSet>> {
        self.row(key).or_else(|| self.row(WATCHED_UNKEYED))
    }

    /// gcj j3/rf3: forget `key`'s memo and its seed-only row (its watched row
    /// is being replaced or dropped).
    fn drop_memo(&mut self, key: usize) {
        self.memos.retain(|(k, _)| *k != key);
        self.drop_seed_only(key);
    }

    /// gcj j3/rf3: forget `key`'s seed-only row (a new publication starts).
    fn drop_seed_only(&mut self, key: usize) {
        self.seed_only.retain(|(k, _)| *k != key);
    }

    /// gcj j3/rf3: the seed-only row of exactly `key`.
    fn seed_only_row(&self, key: usize) -> Option<&std::sync::Arc<WatchedSet>> {
        self.seed_only.iter().find(|(k, _)| *k == key).map(|(_, s)| s)
    }

    /// gcj j3/rf3: the seed-only row a reader for `key` answers from: its own,
    /// else the key-0 row (as [`Self::row_for_reader`]).
    fn seed_only_for_reader(&self, key: usize) -> Option<&std::sync::Arc<WatchedSet>> {
        self.seed_only_row(key).or_else(|| self.seed_only_row(WATCHED_UNKEYED))
    }

    /// gcj j3/rf3: add `addrs` to `key`'s seed-only row (created when absent).
    fn add_seed_only(&mut self, key: usize, addrs: &[usize]) {
        if let Some((_, set)) = self.seed_only.iter_mut().find(|(k, _)| *k == key) {
            std::sync::Arc::make_mut(set).extend(addrs.iter().copied());
            return;
        }
        let fresh: WatchedSet = addrs.iter().copied().collect();
        self.seed_only.push((key, std::sync::Arc::new(fresh)));
    }

    /// Make room for one more row (evicting the least recently published).
    fn reserve_row(&mut self) {
        if self.rows.len() >= WATCHED_TABLE_ROWS {
            let (evicted, _) = self.rows.remove(0);
            self.drop_memo(evicted);
        }
    }

    /// Replace `key`'s row with `addrs` and make it the most recent. The
    /// published set is reused in place when no snapshot of it is held (gci
    /// i21/ws); otherwise a fresh set replaces it and the snapshot keeps the
    /// previous contents.
    fn publish(&mut self, key: usize, addrs: &[usize]) {
        self.drop_memo(key);
        let mut set = match self.position(key) {
            Some(i) => self.rows.remove(i).1,
            None => {
                self.reserve_row();
                std::sync::Arc::new(WatchedSet::default())
            }
        };
        match std::sync::Arc::get_mut(&mut set) {
            Some(s) => {
                s.clear();
                s.extend(addrs.iter().copied());
            }
            None => set = std::sync::Arc::new(addrs.iter().copied().collect()),
        }
        self.rows.push((key, set));
        self.current = key;
    }

    /// gcj j3/rf3 — [`Self::publish`], reusing the row's set when `addrs` is
    /// exactly the input its memo was built from and no snapshot of it is
    /// held: the addresses later adds inserted are removed and the row becomes
    /// the most recent, with no other set operation. Otherwise the row is
    /// rebuilt as [`Self::publish`] does and `addrs` becomes its memo. The
    /// published contents are `addrs` either way. Returns whether the set was
    /// reused.
    fn publish_reusing(&mut self, key: usize, addrs: Vec<usize>) -> bool {
        let row = self.position(key);
        let memo = self.memos.iter().position(|(k, _)| *k == key);
        if let (Some(i), Some(m)) = (row, memo) {
            if self.memos[m].1.input == addrs {
                let added = std::mem::take(&mut self.memos[m].1.added);
                if let Some(s) = std::sync::Arc::get_mut(&mut self.rows[i].1) {
                    for a in &added {
                        s.remove(a);
                    }
                    self.drop_seed_only(key);
                    let entry = self.rows.remove(i);
                    self.rows.push(entry);
                    self.current = key;
                    return true;
                }
                // A snapshot is still held: put the record back and rebuild.
                self.memos[m].1.added = added;
            }
        }
        self.publish(key, &addrs);
        self.memos.push((
            key,
            WatchedRowMemo {
                input: addrs,
                added: Vec::new(),
            },
        ));
        false
    }

    /// Add `addrs` to `key`'s row (copy-on-write), creating it when absent.
    /// gcj j3/rf3: a row with a memo records each address the add inserted.
    fn add(&mut self, key: usize, addrs: &[usize]) {
        let i = match self.position(key) {
            Some(i) => i,
            None => {
                self.reserve_row();
                let fresh = std::sync::Arc::new(WatchedSet::default());
                self.rows.push((key, fresh));
                self.rows.len() - 1
            }
        };
        let set = std::sync::Arc::make_mut(&mut self.rows[i].1);
        match self.memos.iter_mut().find(|(k, _)| *k == key) {
            Some((_, memo)) => {
                for &a in addrs {
                    if set.insert(a) {
                        memo.added.push(a);
                    }
                }
            }
            None => set.extend(addrs.iter().copied()),
        }
    }
}

/// gci i21/ws — the watched-referent set's storage type: Fx-hashed heap
/// addresses (see the `WATCHED_REFERENTS` comment).
pub type WatchedSet = rustc_hash::FxHashSet<usize>;

/// gci i21/ws — a shared, read-only view of the watched-referent set one
/// collection was published with ([`watched_referents_snapshot`]). Cloning
/// it is a reference-count bump; it is `Send + Sync`, so a parallel sweep's
/// workers borrow it directly. It answers exactly as the set it shares:
/// [`Self::contains`], and `for &addr in &snapshot` / [`Self::iter`] visit
/// the members (in no particular order: every consumer sorts or only asks
/// membership).
#[derive(Clone, Debug, Default)]
pub struct WatchedSnapshot(std::sync::Arc<WatchedSet>);

impl WatchedSnapshot {
    /// A snapshot holding exactly `addrs` (tests, and callers that build a set
    /// by hand).
    pub fn from_addrs(addrs: &[usize]) -> Self {
        Self(std::sync::Arc::new(addrs.iter().copied().collect()))
    }

    /// Is `addr` watched?
    #[inline]
    pub fn contains(&self, addr: &usize) -> bool {
        self.0.contains(addr)
    }

    /// Number of distinct watched addresses.
    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether nothing is watched.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The members, in no particular order.
    #[inline]
    pub fn iter(&self) -> std::collections::hash_set::Iter<'_, usize> {
        self.0.iter()
    }

    /// Do two snapshots share one published set (no copy was made)?
    pub fn shares_storage_with(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

impl<'a> IntoIterator for &'a WatchedSnapshot {
    type Item = &'a usize;
    type IntoIter = std::collections::hash_set::Iter<'a, usize>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl FromIterator<usize> for WatchedSnapshot {
    fn from_iter<I: IntoIterator<Item = usize>>(iter: I) -> Self {
        Self(std::sync::Arc::new(iter.into_iter().collect()))
    }
}

/// Replace the watched-referent set for the upcoming collection. Called by
/// the VM immediately before `collect_garbage`, right after nulling the
/// Java-visible referent fields (`weakref_null_referents_pre_gc`) so the
/// mark phase's normal field scan cannot ALSO keep these referents alive —
/// this set exists purely to answer "did address X survive the collection
/// some OTHER way", not to influence marking. Always call this before a
/// collection (with an empty slice if there is nothing to watch this cycle)
/// so a stale entry from a previous cycle can never leak into this one.
///
/// gci i21/ws: the published set is reused in place (its capacity kept, as
/// the std set's `clear` kept it) when no snapshot of the previous one is
/// still held; otherwise a fresh set replaces it and the old snapshot keeps
/// the previous contents (a snapshot never changes under its holder).
///
/// gcj j1/rf: an UNKEYED publication. It resets the thread's table to this
/// one key-0 row (so no keyed row a previous VM published on this thread
/// outlives it), which every heap's keyed reader then falls back to. The VM
/// publishes keyed ([`set_watched_referents_for`]) unless
/// `CRATONVM_GC_WATCHED_SET_PER_HEAP=0`.
pub fn set_watched_referents(addrs: &[usize]) {
    WATCHED_REFERENTS.with(|s| {
        let mut s = s.borrow_mut();
        s.rows.retain(|(k, _)| *k == WATCHED_UNKEYED);
        // gcj j3/rf3: a memo never outlives its row.
        s.memos.retain(|(k, _)| *k == WATCHED_UNKEYED);
        s.seed_only.retain(|(k, _)| *k == WATCHED_UNKEYED);
        s.publish(WATCHED_UNKEYED, addrs);
    });
}

/// gcj j1/rf — [`set_watched_referents`] for one heap: replace only `key`'s
/// row and make it the most recent publication; another heap's row on this
/// thread is untouched. `key` is a Generational heap's
/// `GenerationalHeap::watched_set_key` (its old generation's base), or any
/// other non-zero word for a heap that reads no row (G1, ZGC).
/// `key == WATCHED_UNKEYED` is [`set_watched_referents`].
pub fn set_watched_referents_for(key: usize, addrs: &[usize]) {
    if key == WATCHED_UNKEYED {
        set_watched_referents(addrs);
        return;
    }
    WATCHED_REFERENTS.with(|s| s.borrow_mut().publish(key, addrs));
}

/// gcj j3/rf3 — publications [`set_watched_referents_reusing_for`] answered by
/// reusing the row's set, run total (the positive control of
/// `CRATONVM_GEN_WATCHED_SET_REUSE_UNCHANGED`).
pub static WATCHED_SET_REUSED: AtomicU64 = AtomicU64::new(0);
/// gcj j3/rf3 — publications [`set_watched_referents_reusing_for`] had to
/// rebuild (the input changed, a snapshot was held, or the row was new).
pub static WATCHED_SET_REBUILT: AtomicU64 = AtomicU64::new(0);

/// gcj j3/rf3 (`gcj-j1rf-proposal-watched-set-maintained-by-the-processor`,
/// the publication slice; `CRATONVM_GEN_WATCHED_SET_REUSE_UNCHANGED`, opt-in,
/// Generational only: the VM decides) — [`set_watched_referents_for`], taking
/// the processor's list by value: when `addrs` is element for element the list
/// `key`'s row was last built from through this call and no snapshot of the
/// row is held, the row's set is reused (the addresses later
/// [`add_watched_referents`] calls inserted are removed again) instead of
/// being cleared and re-hashed. The published contents are exactly `addrs`
/// on both arms. Returns whether the set was reused. `key ==
/// WATCHED_UNKEYED` is [`set_watched_referents`] (never reused).
pub fn set_watched_referents_reusing_for(key: usize, addrs: Vec<usize>) -> bool {
    if key == WATCHED_UNKEYED {
        set_watched_referents(&addrs);
        WATCHED_SET_REBUILT.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    let reused = WATCHED_REFERENTS.with(|s| s.borrow_mut().publish_reusing(key, addrs));
    if reused {
        WATCHED_SET_REUSED.fetch_add(1, Ordering::Relaxed);
    } else {
        WATCHED_SET_REBUILT.fetch_add(1, Ordering::Relaxed);
    }
    reused
}

/// Add `addrs` to this thread's watched-referent set WITHOUT replacing it
/// (gc-common w9-g). For a caller that watches a few more addresses after
/// the reference processor published its set: the JNI weak-global sweep
/// (`jni::watch_weak_global_referents`), which used to clone the whole set
/// and rebuild it through [`set_watched_referents`] to add them.
///
/// gci i21/ws: copy-on-write -- a snapshot taken before this call keeps the
/// contents it was taken with (no caller holds one across an add today: the
/// adds run before the collection takes its snapshots).
///
/// gcj j1/rf: adds to the MOST RECENT publication's row (every caller runs
/// right after its VM's publish); [`add_watched_referents_for`] names the
/// heap instead.
pub fn add_watched_referents(addrs: &[usize]) {
    if addrs.is_empty() {
        return;
    }
    WATCHED_REFERENTS.with(|s| {
        let mut s = s.borrow_mut();
        let key = s.current;
        s.add(key, addrs);
    });
}

/// gcj j1/rf — [`add_watched_referents`] into `key`'s row (created when
/// absent), whatever this thread published last.
pub fn add_watched_referents_for(key: usize, addrs: &[usize]) {
    if addrs.is_empty() {
        return;
    }
    WATCHED_REFERENTS.with(|s| s.borrow_mut().add(key, addrs));
}

/// gcj j3/rf3 (`gcj-j2rf2-proposal-string-table-young-entries-ride-a-seed-only-watched-row`,
/// `CRATONVM_GC_STRING_TABLE_SEED_ONLY_YOUNG`, opt-in, Generational only: the
/// VM decides) — add `addrs` to the SEED-ONLY row of the most recent
/// publication's heap (as [`add_watched_referents`] adds to its watched row).
/// Only a major's young seed and its watched-dead verdict read that row
/// ([`watched_major_snapshot_for`]); the young sweeps do not, so a young
/// object in it gets no identity `pointer_map` entry from them. Cleared by
/// the heap's next publication.
pub fn add_seed_only_referents(addrs: &[usize]) {
    if addrs.is_empty() {
        return;
    }
    WATCHED_REFERENTS.with(|s| {
        let mut s = s.borrow_mut();
        let key = s.current;
        s.add_seed_only(key, addrs);
    });
}

/// gcj j3/rf3 — the watched set a MAJOR's young seed and watched-dead verdict
/// read for the heap whose key is `key`: [`watched_referents_snapshot_for`]
/// plus that heap's seed-only row ([`add_seed_only_referents`]). With no
/// seed-only row (every run without the opt-in) it is exactly
/// [`watched_referents_snapshot_for`], sharing its storage; otherwise a fresh
/// union, built once per major. `None` when both are empty.
pub fn watched_major_snapshot_for(key: usize) -> Option<WatchedSnapshot> {
    WATCHED_REFERENTS.with(|s| {
        let s = s.borrow();
        let watched = s.row_for_reader(key).filter(|set| !set.is_empty());
        match s.seed_only_for_reader(key).filter(|set| !set.is_empty()) {
            None => watched.map(|set| WatchedSnapshot(std::sync::Arc::clone(set))),
            Some(extra) => {
                let mut union: WatchedSet = match watched {
                    Some(set) => (**set).clone(),
                    None => WatchedSet::default(),
                };
                union.extend(extra.iter().copied());
                Some(WatchedSnapshot(std::sync::Arc::new(union)))
            }
        }
    })
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

// THREE doc blocks used to be concatenated here and attached, all of them, to
// `PEER_REG_CAPTURE` alone (fixed 2026-09-20). The first belongs to
// `XT_PASSES_LAST_CYCLE` (declared ~200 lines down, and undocumented as a
// result); the second to `PEER_DEPTH_ZERO_TOTAL` (likewise); only the third is
// this static's. Each has been moved to its own item.
//
// Worth stating because the content is not decorative: both orphaned blocks say
// that a ZERO in their counter means "nobody looked", not "looked and found
// nothing", and that is the whole reason the counter exists. Rendered against
// the wrong item, that warning reached no one.
// Register words captured from a FROZEN peer, for the stale-register pairing,
// live in the pause's [`PauseLedger`] (`reg_capture`; the `PEER_REG_CAPTURE`
// process static until gc-common w18-c).
//
// `(os_tid, register index, value)`. The whole GPR block is recorded, not just
// the words a root filter accepted: the question is whether a peer resumes
// holding an address the collector MOVED, and pre-filtering with the same
// predicate the collector already trusts would beg it.
//
// Empty and untouched unless `CRATONVM_DBG_PEER_REG_PAIRING` is set.

/// Peer register words that turned out to name a RELOCATED object.
///
/// The counter behind the pairing §10.11 asked for: a frozen peer's registers
/// are not heap, not frame-band memory, and are never rewritten by a Cheney
/// copy, so a non-zero reading is a thread that will resume with a pointer to
/// an address the collection vacated.
pub static PEER_REG_STALE: AtomicU64 = AtomicU64::new(0);

/// gen r4w6/pinstale6: the subset of the pairing's words that no channel
/// rewrites and no pin keeps (`gen_heap`'s `peer_word_stale_live`): a register
/// or take-over stack word naming a relocated base, any word INTO a relocated
/// object, and a helper-window stack base when the blocked-wake remap is off.
/// A helper-window stack base the remap rewrites at wake is in
/// [`PEER_REG_STALE`] and not here. Only counted under
/// `CRATONVM_DBG_PEER_REG_PAIRING`.
pub static PEER_REG_STALE_LIVE: AtomicU64 = AtomicU64::new(0);

/// Whether the blocked-peer native-stack remap is armed.
///
/// Default-ON. `CRATONVM_GC_NO_BLOCKED_PEER_STACK_REMAP=1` restores the
/// pre-2026-09-07 behaviour, where a blocked peer resumed with its
/// conservatively-scanned stack words still at their pre-move addresses.
///
/// (Until 2026-09-20 this carried [`record_peer_reg`]'s first line — "Record
/// one frozen peer's register word. No-op unless the pairing is armed." — which
/// named the wrong function AND the wrong flag: this gate is
/// `CRATONVM_GC_NO_BLOCKED_PEER_STACK_REMAP`, `record_peer_reg`'s is
/// `CRATONVM_DBG_PEER_REG_PAIRING`. A reader following the doc would have
/// concluded the stack remap was a diagnostic.)
///
/// gci i2/fr: `!runtime_flag_on`, so `=0` keeps the remap ON (a presence read
/// turned this soundness repair OFF for `=0`).
pub fn blocked_peer_stack_remap_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !cratonvm_types::flags::runtime_flag_on("CRATONVM_GC_NO_BLOCKED_PEER_STACK_REMAP")
    })
}

// `(os_tid, addr, value)` for every native-stack word this cycle's
// cross-thread scan resolved to a heap object. Drained by
// `ThreadRegistry::fold_pointer_map_into_blocked_audited`, which moves each
// entry onto its owning blocked thread.
//
// PER VM since gc-common w18-c: the buffer is the pause's [`PauseLedger`]'s
// `stack_slots` (it was the `PEER_STACK_SLOTS` process static). See
// [`record_peer_stack_slot`] for the thread audit that makes the move exact.

/// Engagement census for the blocked-peer native-stack remap. `CAPTURED` is the
/// denominator; `WRITTEN` is the repair actually storing a new address; and
/// `SKIPPED` is the wake guard declining because the word no longer reads its
/// captured value (the native call reused it). A run with `written=0` did not
/// exercise the repair at all, and no conclusion may be drawn from its result.
pub static PEER_STACK_SLOTS_CAPTURED: AtomicU64 = AtomicU64::new(0);
pub static PEER_STACK_SLOTS_ADOPTED: AtomicU64 = AtomicU64::new(0);
pub static PEER_STACK_SLOTS_WRITTEN: AtomicU64 = AtomicU64::new(0);
pub static PEER_STACK_SLOTS_SKIPPED: AtomicU64 = AtomicU64::new(0);
/// gcj-j1pn-raw-root-write-owner-findings item 2: write-backs the waking
/// thread refused because the word lies BELOW the wake path's entry -- in a
/// frame the wake itself pushed, which did not exist when the word was
/// captured (`vm_exec::apply_native_slot_fixups`, the wake floor). Each was a
/// store the `orig` value check alone used to admit.
pub static PEER_STACK_SLOTS_REFUSED_BELOW_WAKE: AtomicU64 = AtomicU64::new(0);

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

/// The capture buffer's hard cap (see [`record_peer_stack_slot`]).
const PEER_STACK_SLOTS_CAP: usize = 65536;

/// The share of [`PEER_STACK_SLOTS_CAP`] a TAKE-OVER capture may occupy.
///
/// gc-common w2-c (2026-09-23). The take-over pass runs BEFORE the helper-window
/// pass in every cycle, and its captures are never routable -- a peer frozen in
/// compiled code is not in a blocked region, so the fold only COUNTS them
/// (`PEER_STACK_SLOTS_UNROUTED`). The helper-window captures are the
/// load-bearing ones: they are what the blocked-wake remap rewrites. With one
/// shared cap, a take-over over a deep frozen stack (the Linux arm now reads up
/// to 512 MiB, and w2-c gave the Windows arm the same capture) could fill the
/// buffer first, and every helper-window word after it was DROPPED -- a repair
/// outage manufactured by a census. Take-over captures stop at half, so the
/// other half is always there for the repair.
const TAKEOVER_STACK_SLOTS_CAP: usize = PEER_STACK_SLOTS_CAP / 2;

/// Take-over captures declined because they reached [`TAKEOVER_STACK_SLOTS_CAP`].
/// A census loss only (these words were never routable); kept apart from
/// [`PEER_STACK_SLOTS_DROPPED`] so it is never read as a repair outage.
pub static PEER_STACK_SLOTS_TAKEOVER_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Push `entry` unless `buf` already holds `cap` entries. `true` when pushed.
#[inline]
fn push_capped(buf: &mut Vec<(u32, usize, usize)>, entry: (u32, usize, usize), cap: usize) -> bool {
    if buf.len() < cap {
        buf.push(entry);
        true
    } else {
        false
    }
}

/// Record one scanned native-stack word and the address it lives at.
///
/// # Per VM (gc-common w18-c, 2026-09-25)
///
/// The capture buffer is the calling thread's [`PauseLedger`] (`stack_slots`),
/// not a process static. Exact, because every access is made by the thread
/// that requested the pause, which is bound to its own VM's ledger from its
/// winning request (`GcBarrier::request_stw_counted_locked`) to `complete_gc`:
///
/// | access | where | thread |
/// |---|---|---|
/// | clear | `PauseLedger::open` (the winning request) and `begin_moving_young_coverage_cycle` (the collection's `open_cycle`, same lock, same thread) | requester |
/// | add | this function and [`record_takeover_stack_slot`], from `vm/src/jit/xt_root_scan.rs` only: the take-over pass and the helper-window pass, both driven by `stw_take_over_and_wait` (`gc_and_alloc.rs`); the Linux arm copies a parked peer's band first and records AFTER releasing it (`classify_parked_snapshot`); `xt_root_scan` spawns no thread | requester |
/// | read | `gen_heap`'s pinned-young decision and plan (`peer_stack_slot_values_into`), `pause_young_pin_read` (`peer_stack_slots_saturated`), inside `collect_garbage_inner_with_pins`, not in a worker closure | collecting thread = requester (`run_collection_pause`) |
/// | drain | [`take_peer_stack_slots`] from `ThreadRegistry::fold_pointer_map_into_blocked_audited`, reached only through `update_all_roots` step 20 (`memory/gc.rs`), which `run_collection_pause` calls BEFORE `complete_gc` unbinds | requester |
///
/// The other two `fold_pointer_map_into_blocked` calls in `vm/src/vm/vm_exec.rs`
/// (`mod tests`) and the one in `thread_registry.rs`'s tests run on an unbound
/// test thread: they drain that thread's own empty fallback, where they used
/// to drain whatever capture a CONCURRENT test's pause had in flight.
///
/// What this closes: with two VMs' pauses overlapping (the coverage slot's
/// 250 ms give-up, an abandoned hold), VM B's opener cleared VM A's captures
/// between A's helper-window pass and A's fold (a blocked peer of A then woke
/// with a stale stack word, the repair silently skipped), and A's fold could
/// adopt B's captures (`os_tid`s of B's threads: never routable in A, so
/// counted `UNROUTED`, a false repair-outage reading) or fold them against A's
/// pointer map. B's take-over could also fill the shared cap and DROP A's
/// helper-window captures.
pub fn record_peer_stack_slot(os_tid: u32, addr: usize, value: usize) {
    if !blocked_peer_stack_remap_enabled() {
        return;
    }
    // Bounded. A runaway capture would cost the pause it is trying to make
    // correct; the observed population is 19-91 words per cycle.
    let pushed = with_ledger(|l| {
        push_capped(
            &mut l.stack_slots.lock(),
            (os_tid, addr, value),
            PEER_STACK_SLOTS_CAP,
        )
    });
    match pushed {
        Some(true) => {
            PEER_STACK_SLOTS_CAPTURED.fetch_add(1, Ordering::Relaxed);
        }
        Some(false) => {
            PEER_STACK_SLOTS_DROPPED.fetch_add(1, Ordering::Relaxed);
        }
        // Thread-local teardown: no ledger to record into.
        None => {}
    }
}

/// [`record_peer_stack_slot`] for a word of a peer FROZEN by the take-over:
/// capped at [`TAKEOVER_STACK_SLOTS_CAP`] so it can never crowd the
/// helper-window captures out of the buffer.
pub fn record_takeover_stack_slot(os_tid: u32, addr: usize, value: usize) {
    if !blocked_peer_stack_remap_enabled() {
        return;
    }
    let pushed = with_ledger(|l| {
        push_capped(
            &mut l.stack_slots.lock(),
            (os_tid, addr, value),
            TAKEOVER_STACK_SLOTS_CAP,
        )
    });
    match pushed {
        Some(true) => {
            PEER_STACK_SLOTS_CAPTURED.fetch_add(1, Ordering::Relaxed);
        }
        Some(false) => {
            PEER_STACK_SLOTS_TAKEOVER_DROPPED.fetch_add(1, Ordering::Relaxed);
        }
        None => {}
    }
}

/// Drain the cycle's captures, from the calling thread's [`PauseLedger`].
/// Called once per collection by the fold, on the requester.
pub fn take_peer_stack_slots() -> Vec<(u32, usize, usize)> {
    with_ledger(|l| std::mem::take(&mut *l.stack_slots.lock())).unwrap_or_default()
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
///
/// Clears the calling thread's [`PauseLedger`] only (gc-common w18-c): another
/// VM's opener can no longer discard this VM's captures mid-pause.
pub fn clear_peer_stack_slots() {
    let _ = with_ledger(PauseLedger::clear_stack_slots);
}

/// The values of this pause's peer native-stack captures (the buffer
/// [`record_peer_stack_slot`] fills and the blocked-wake fold drains), appended
/// to `out` where `keep` holds. The buffer is NOT drained: the fold still needs
/// every entry.
///
/// gen r4w6/pinstale6 (2026-09-24): the pinned in-place young copy reads it to
/// pin the one blocked-peer stack population no channel repairs — a word that
/// is NOT an object base (the fold's exact `pointer_map` lookup rewrites bases
/// only). See `gen_heap`'s `InPlaceEvac` block and
/// `docs/feature-designs/generative-gc-architecture.md` §1.
pub fn peer_stack_slot_values_into(out: &mut Vec<usize>, keep: impl Fn(usize) -> bool) {
    let _ = with_ledger(|l| {
        let g = l.stack_slots.lock();
        out.extend(g.iter().map(|&(_, _, v)| v).filter(|&v| keep(v)));
    });
}

/// Remove this pause's peer native-stack captures for which `stale(os_tid,
/// value)` holds, so that neither the pin loop nor the blocked-wake fold sees
/// them; returns how many went. G1's post-block screen
/// (`g1_peer_pins_skip_post_block_regions`) is the one caller: a word naming a
/// region handed out after its peer blocked is not a reference, and the fold
/// must not store a new address over it any more than the pin should hold it.
pub fn drop_peer_stack_slots_where(mut stale: impl FnMut(u32, usize) -> bool) -> usize {
    with_ledger(|l| {
        let mut g = l.stack_slots.lock();
        let before = g.len();
        g.retain(|&(tid, _, v)| !stale(tid, v));
        before - g.len()
    })
    .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// ngr: the region hand-out clock and blocked peers' block stamps
// ---------------------------------------------------------------------------
//
// A blocked peer's native-stack word can only be a reference to an object the
// peer could reach BEFORE it blocked: a thread inside a blocked region reads
// no heap (a poll loop leaves the region to read, and its next entry stamps
// again), and the blocked-wake fold writes moved addresses back only at wake.
// An object placed after the peer blocked lives in a region handed out after
// it blocked. So a captured word naming such a region is a stale stack word
// (the NativeGrowthReclaimProbe rows: the Reference Handler's old TLAB bounds,
// equal to the base of a 34 MB humongous array allocated much later), and the
// pin it would take holds a live span still for nothing.
//
// The clock is process-wide: it only orders events, so two VMs sharing it lose
// nothing. A region leaving the Free pool ticks it ([`region_handout_tick`]);
// a thread entering its outermost blocked window reads it into its slot
// ([`register_self_block_stamp_slot`]) and stores 0 at the matching leave.
// 0 means "unknown": its words keep every pin.
//
// Ordering: the tick is followed by a SeqCst fence before the region can be
// allocated into, and the block-side read is preceded by one, so a peer that
// read a reference to an object in a region before blocking reads a clock at
// least that region's stamp (the message-passing pattern with a full fence on
// both sides).

static REGION_HANDOUT_CLOCK: AtomicU64 = AtomicU64::new(1);

/// Advance the hand-out clock for a region leaving the Free pool and return its
/// stamp (strictly greater than every stamp a block entry has read so far).
pub fn region_handout_tick() -> u64 {
    let stamp = REGION_HANDOUT_CLOCK.fetch_add(1, Ordering::SeqCst) + 1;
    std::sync::atomic::fence(Ordering::SeqCst);
    stamp
}

/// The hand-out clock as a thread entering a blocked window sees it: every
/// region handed out before any reference this thread holds has a stamp at or
/// below the returned value.
pub fn region_handout_clock_at_block() -> u64 {
    std::sync::atomic::fence(Ordering::SeqCst);
    REGION_HANDOUT_CLOCK.load(Ordering::SeqCst)
}

static PER_TID_BLOCK_STAMP: std::sync::OnceLock<
    std::sync::RwLock<std::collections::HashMap<u32, std::sync::Arc<AtomicU64>>>,
> = std::sync::OnceLock::new();

fn per_tid_block_stamp(
) -> &'static std::sync::RwLock<std::collections::HashMap<u32, std::sync::Arc<AtomicU64>>> {
    PER_TID_BLOCK_STAMP.get_or_init(|| std::sync::RwLock::new(std::collections::HashMap::new()))
}

/// Hand the calling thread the slot it publishes its block stamp into, reset
/// to 0 (unknown). Once per thread; the caller caches the `Arc`. As
/// [`register_self_jit_depth_slot`], re-registering a recycled tid returns the
/// existing slot.
pub fn register_self_block_stamp_slot(os_tid: u32) -> std::sync::Arc<AtomicU64> {
    let mut map = per_tid_block_stamp()
        .write()
        .unwrap_or_else(|p| p.into_inner());
    let slot = std::sync::Arc::clone(
        map.entry(os_tid)
            .or_insert_with(|| std::sync::Arc::new(AtomicU64::new(0))),
    );
    slot.store(0, Ordering::Release);
    slot
}

/// Remove a thread's block-stamp slot at its exit, so a recycled tid never
/// inherits a stamp.
pub fn unregister_block_stamp_slot(os_tid: u32) {
    per_tid_block_stamp()
        .write()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&os_tid);
}

/// The clock value `os_tid` read when it entered its current blocked window,
/// or `None` when it is not in one (or never published).
pub fn block_stamp_of_tid(os_tid: u32) -> Option<u64> {
    let map = per_tid_block_stamp()
        .read()
        .unwrap_or_else(|p| p.into_inner());
    map.get(&os_tid)
        .map(|slot| slot.load(Ordering::Acquire))
        .filter(|&s| s != 0)
}

/// gcg g6/jn (`CRATONVM_GEN_UNRECORDED_ROOT_GUARD_DEPOSITS`): replace this
/// pause's set of conservatively deposited snapshot words. Called by the VM's
/// snapshot gather (`ThreadRegistry::collect_all_root_snapshots`), on the
/// initiator, before the collection the guard runs in.
pub fn publish_conservative_deposit_words(words: Vec<usize>) {
    let _ = with_ledger(move |l| *l.conservative_deposit_words.lock() = words);
}

/// gcg g6/jn: this pause's conservatively deposited snapshot words that
/// satisfy `keep`, appended to `out` (none during thread-local teardown).
pub fn conservative_deposit_words_into(out: &mut Vec<usize>, keep: impl Fn(usize) -> bool) {
    let _ = with_ledger(|l| {
        out.extend(
            l.conservative_deposit_words
                .lock()
                .iter()
                .copied()
                .filter(|&v| keep(v)),
        );
    });
}

/// Whether this pause's peer native-stack capture buffer refused a word
/// (it is at `PEER_STACK_SLOTS_CAP`): the capture — and every consumer that
/// reads it as the whole blocked-peer stack population — is then incomplete.
/// `true` (fail-closed: "incomplete") during thread-local teardown.
pub fn peer_stack_slots_saturated() -> bool {
    with_ledger(|l| l.stack_slots.lock().len() >= PEER_STACK_SLOTS_CAP).unwrap_or(true)
}

/// `record_peer_reg`'s register index for a word read from a TAKE-OVER
/// (frozen) peer's stack rather than from a register.
pub const PEER_REG_STACK_TAKEOVER: u8 = 0xfe;

/// `record_peer_reg`'s register index for a word read from a HELPER-WINDOW
/// (blocked) peer's stack rather than from a register.
pub const PEER_REG_STACK_HELPER: u8 = 0xff;

/// Does `record_peer_reg`'s `reg` name a general-purpose REGISTER (as opposed
/// to one of the two stack populations)? Windows numbers the CONTEXT GPRs
/// 0..=15, the Linux arm its 17 `gregs` 0..=16.
#[inline]
pub fn peer_reg_is_register(reg: u8) -> bool {
    reg < PEER_REG_STACK_TAKEOVER
}

/// Record one peer register or stack word the cross-thread scan read.
///
/// Two consumers:
///
/// * **Always** (gen r4w6/pinstale6, 2026-09-24): a REGISTER word inside a
///   published young region joins this pause's young pin-word ledger
///   ([`YoungPinLedger::note_peer_reg_word`]), so a pinned in-place young
///   cycle pins the page it names. Nothing rewrites a peer's register file —
///   not the frame remap, not the blocked-wake stack fold — so pinning is the
///   only channel that keeps such a word naming its object. On a cycle that
///   can relocate, the only peers whose registers are captured are blocked
///   peers with no compiled frame (a frozen peer or a helper window makes the
///   take-over verdict non-`NONE`, which fails the ledger and the coverage
///   proof); their registers belong to VM Rust frames, whose liveness no
///   calling-convention argument settles — a suspended thread is not at a call
///   boundary. See `docs/feature-designs/generative-gc-architecture.md`.
/// * Only with `CRATONVM_DBG_PEER_REG_PAIRING`: every word, into the calling
///   thread's [`PauseLedger`] (`reg_capture`; the `PEER_REG_CAPTURE` process
///   static until gc-common w18-c), for the `[peer-reg-stale]` pairing. The
///   writers and the one reader (`gen_heap`'s pairing, in
///   `collect_garbage_inner_with_pins`) are the requester, exactly as for
///   [`record_peer_stack_slot`]'s buffer.
pub fn record_peer_reg(os_tid: u32, reg: u8, value: usize) {
    if peer_reg_is_register(reg) {
        note_peer_reg_pin_word(value);
    }
    if !peer_reg_pairing_enabled() {
        return;
    }
    let _ = with_ledger(|l| {
        let mut g = l.reg_capture.lock();
        // Bounded: a runaway capture would change the timing it is measuring.
        if g.len() < 65536 {
            g.push((os_tid, reg, value));
        }
    });
}

/// Drop the previous cycle's capture. Called where the collection begins, so a
/// hit is always attributable to the cycle that relocated. The calling
/// thread's [`PauseLedger`] only.
pub fn clear_peer_reg_capture() {
    let _ = with_ledger(PauseLedger::clear_reg_capture);
}

/// Take the capture for comparison against this cycle's pointer map, from the
/// calling thread's [`PauseLedger`].
pub fn take_peer_reg_capture() -> Vec<(u32, u8, usize)> {
    with_ledger(|l| std::mem::take(&mut *l.reg_capture.lock())).unwrap_or_default()
}

/// `CRATONVM_DBG_PEER_REG_PAIRING` — arm the frozen-peer register capture and
/// the post-evacuation comparison against the pointer map.
pub fn peer_reg_pairing_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_PEER_REG_PAIRING")
    })
}

/// Collections whose peer-JIT accounting was SKIPPED because `peer_depth` read
/// as zero.
///
/// `refresh_moving_young_coverage_for_collection` only runs the peer handshake
/// inside `if peer_depth > 0`. A zero therefore does not mean "the peers were
/// accounted for" -- it means nothing was asked. On a workload where peers are
/// continuously in compiled code that is the one door to the moving arm that
/// no ledger guards, so it has to be countable separately from an accepted
/// handshake.
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

// The six take-over counts of this cycle (`XT_PASSES_LAST_CYCLE` and its five
// siblings until gc-common w9-f) are fields of the calling thread's
// [`PauseLedger`] now: see [`XtCounts`] for the "zero passes means nobody
// looked" rule and for why the move is exact.

/// Record this cycle's post-quota take-over time: the nanoseconds
/// `stw_take_over_and_wait` spent AFTER its quota was met (the frozen-peer
/// interpreter-frame walk, the helper-window pass and the TLAB skip-span
/// publish). gc-common w4-a (`handoff-w3f-takeover-phase-timing`): the
/// `--verbose:gc` pause line's `ttsp_us` ends when the quota is met and its
/// `collect_us` starts after the take-over returns, so this work was in
/// neither. Called once, at the end of the take-over.
///
/// Stored in the calling thread's [`PauseLedger`] -- the initiator's, i.e.
/// its own VM's (gc-common w8-d; `XT_POST_QUOTA_NS_LAST_CYCLE` until then).
pub fn publish_xt_post_quota_ns(ns: u64) {
    let _ = with_ledger(|l| l.post_quota_ns.store(ns, Ordering::Relaxed));
}

/// This cycle's post-quota take-over time in nanoseconds; 0 when no take-over
/// ran (the take-over disabled, or a backend without TLAB skip support) --
/// every pause opens its ledger at zero ([`reset_peer_proven_jit_depth`]).
pub fn xt_post_quota_ns() -> u64 {
    with_ledger(|l| l.post_quota_ns.load(Ordering::Relaxed)).unwrap_or(0)
}

/// Add one take-over pass's duration: the passes (`xt::take_over_pass` and the
/// root-gathering variant) run INSIDE the barrier wait, so their sum
/// ([`xt_pass_ns`]) is a SHARE of the pause line's `ttsp_us`, never an addition
/// to it. gc-common w6-f (`common-c-proposal-takeover-cost-in-the-pause-line`).
/// In the calling thread's [`PauseLedger`] since gc-common w8-d
/// (`XT_PASS_NS_LAST_CYCLE` until then).
pub fn add_xt_pass_ns(ns: u64) {
    let _ = with_ledger(|l| l.pass_ns.fetch_add(ns, Ordering::Relaxed));
}

/// This cycle's summed take-over pass time in nanoseconds; 0 when no pass ran.
pub fn xt_pass_ns() -> u64 {
    with_ledger(|l| l.pass_ns.load(Ordering::Relaxed)).unwrap_or(0)
}

/// Zero the per-cycle cross-thread coverage. Called once at the top of the
/// stop-the-world take-over, so what the sweep reads describes THIS cycle.
pub fn reset_xt_cycle() {
    // The counts and the times of the calling thread's ledger -- the
    // initiator's, i.e. its own VM's (gc-common w9-f for the counts).
    // The verdict too, in the same ledger (gc-common w10-g).
    let _ = with_ledger(|l| {
        l.xt.clear();
        l.clear_takeover_times();
        l.clear_verdict();
        l.proven_monitor_peers.lock().clear();
    });
}

/// What the cross-thread take-over left behind for THIS pause, stated once
/// (gc-common w2-c, first step of `common-c-proposal-one-takeover-contract`).
///
/// Written at the end of `stw_take_over_and_wait`, reset with the rest of the
/// per-cycle coverage by [`reset_xt_cycle`]. Today each backend reads a
/// different subset of the facts below (`XT_TAKEOVER` in a coverage mask,
/// `unrewritable_peer_state`, the pin sets) and they disagree about whether a
/// frozen peer is safe; [`TakeoverVerdict::licence`] is the single answer they
/// are meant to converge on. Consumers: Generational
/// ([`takeover_forbids_unpinnable_move`], always; gc-common w3-g), G1
/// ([`g1_takeover_licence_refusal`], opt-in; w3-g), ZGC
/// (`ZgcRealHeap::coverage_incompleteness_is_page_pinnable`, whenever a
/// page-pinned relocation is considered, as an extra refusal next to its
/// `XT_TAKEOVER` coverage mask; w4-g).
///
/// Reset by [`reset_xt_cycle`] (a take-over runs) AND by
/// [`reset_peer_proven_jit_depth`] (every pause opens), so a pause that never
/// reaches the take-over reads [`TakeoverVerdict::NONE`], not a stale verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TakeoverVerdict {
    /// Peers frozen in compiled code (their registers cannot be rewritten).
    pub frozen: u32,
    /// Blocked peers whose stacks held JIT frames (helper windows).
    pub helper_windows: u32,
    /// Peers the take-over could not classify at all -- still RUNNING, roots
    /// unseen. Nothing can be pinned for them.
    pub unreadable: u32,
    /// Every frozen/helper-window band was read whole and pinned: no
    /// incomplete stack, every helper window pinned.
    pub pins_complete: bool,
    /// The probes resolved DERIVED pointers to their objects
    /// (`CRATONVM_XT_TAKEOVER_INTERIOR` for frozen peers,
    /// `CRATONVM_XT_HELPER_WINDOW_PIN_RESOLVE` for helper windows). An
    /// exact-base pin set leaves a cursor's array unpinned.
    pub derived_pointers_resolved: bool,
}

/// What a collector may do to the heap under a [`TakeoverVerdict`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveLicence {
    /// No take-over constraint this pause.
    Move,
    /// Move everything EXCEPT what the take-over pinned (a backend that can
    /// pin: G1 regions, ZGC pages).
    MovePinned,
    /// Nothing a frozen or unread peer could name may move.
    NonMoving,
}

impl TakeoverVerdict {
    /// The verdict of a pause that froze nothing and read every peer.
    pub const NONE: TakeoverVerdict = TakeoverVerdict {
        frozen: 0,
        helper_windows: 0,
        unreadable: 0,
        pins_complete: true,
        derived_pointers_resolved: true,
    };

    /// The one predicate: may a collector move objects this pause, given
    /// whether it can honour a pin set (`backend_can_pin`: G1 and ZGC yes,
    /// Generational's Cheney copy no)?
    ///
    /// * nothing frozen, no helper window, nobody unread -> [`MoveLicence::Move`];
    /// * an UNREAD peer -> [`MoveLicence::NonMoving`] whatever else holds: its
    ///   roots were never seen, so no pin set can be complete;
    /// * a backend that cannot pin -> `NonMoving`;
    /// * a pinnable backend with a complete, derived-pointer-resolving pin set
    ///   -> [`MoveLicence::MovePinned`]; otherwise `NonMoving`.
    pub fn licence(&self, backend_can_pin: bool) -> MoveLicence {
        if self.frozen == 0 && self.helper_windows == 0 && self.unreadable == 0 {
            return MoveLicence::Move;
        }
        if self.unreadable > 0 || !backend_can_pin {
            return MoveLicence::NonMoving;
        }
        if self.pins_complete && self.derived_pointers_resolved {
            MoveLicence::MovePinned
        } else {
            MoveLicence::NonMoving
        }
    }
}

impl TakeoverVerdict {
    /// The fail-closed reading: a peer nobody read, so every backend's licence
    /// is [`MoveLicence::NonMoving`]. What [`takeover_verdict`] answers when
    /// the calling thread's ledger is unreachable (thread-local teardown), and
    /// never a published verdict.
    pub const UNKNOWN: TakeoverVerdict = TakeoverVerdict {
        frozen: 0,
        helper_windows: 0,
        unreadable: 1,
        pins_complete: false,
        derived_pointers_resolved: false,
    };
}

// The take-over verdict is PER VM since gc-common w10-g: it lives in the
// calling thread's [`PauseLedger`] (it was the process static
// `TAKEOVER_VERDICT`). Unlike the take-over counts ([`XtCounts`]), its
// unbound-thread fallback (`NONE`, licence `Move`) is the UNSAFE answer, so the
// move rests on every access sharing the thread of the pause's requester,
// which is bound to its own VM's ledger from its winning request
// (`GcBarrier::request_stw_counted_locked`) to its `complete_gc`:
//
// | access | where | thread |
// |---|---|---|
// | clear | `reset_peer_proven_jit_depth` (`PauseLedger::open`) from the winning request; `reset_xt_cycle` at the top of `stw_take_over_and_wait` | the requester |
// | write | `publish_takeover_verdict`, end of `stw_take_over_and_wait` (`gc_and_alloc.rs`), called by `run_collection_pause` and by the non-collection pauses after their own request | the requester |
// | read, Generational | `collect_garbage_inner_with_pins` (`takeover_forbids_unpinnable_move`); `OldPinnedCompact::for_this_pause` via `sweep_old_gen_non_moving` from `run_non_moving_young_cycle`; `pause_young_pin_read` (below) | the collecting thread = the requester (`run_collection_pause` collects on the thread that requested) |
// | read, G1 | `G1Collector::collect_garbage` (`g1_takeover_licence_refusal`), once per pause before evacuation | the collecting thread |
// | read, ZGC | `relocate_refusal` -> `coverage_incompleteness_is_page_pinnable`, from `relocate_stw` and the STW collect | the collecting thread (the mark driver `zgc-concurrent-mark` and the `zgc_par_chunks` workers never read it) |
//
// No reader runs on a GC worker, the `cratonvm-gc-young-wipe` helper or the
// finalizer thread. A reader on the writer's thread reads what the writer
// wrote whether or not the thread is bound, so no audited read can land on
// the empty fallback. What this closes: with two VMs' pauses overlapping (the
// coverage slot's 250 ms give-up), VM B's pause opening reset VM A's verdict to
// `NONE` between A's take-over and A's collector, i.e. licence `Move` for a
// peer A froze; and VM B's take-over could hand A's collector B's verdict.

/// Record this pause's [`TakeoverVerdict`] (the take-over driver, once, after
/// the helper-window pass), in the calling thread's [`PauseLedger`].
pub fn publish_takeover_verdict(v: TakeoverVerdict) {
    let _ = with_ledger(|l| *l.verdict.lock() = v);
}

/// This pause's [`TakeoverVerdict`], from the calling thread's
/// [`PauseLedger`]; [`TakeoverVerdict::NONE`] when no take-over ran, and
/// [`TakeoverVerdict::UNKNOWN`] (fail-closed) during thread-local teardown.
pub fn takeover_verdict() -> TakeoverVerdict {
    with_ledger(|l| *l.verdict.lock()).unwrap_or(TakeoverVerdict::UNKNOWN)
}

/// [`TakeoverVerdict::licence`] of this pause's verdict.
pub fn may_move_under_takeover(backend_can_pin: bool) -> MoveLicence {
    takeover_verdict().licence(backend_can_pin)
}

/// Whether the take-over licence FORBIDS a backend that cannot honour a pin
/// set (Generational's Cheney copy) from moving anything this pause.
///
/// The Generational young collector folds this into its coverage term, so its
/// take-over divert comes from the same predicate as the other two backends'
/// (gc-common w3-g, `handoff-w2c-one-takeover-licence` edit 3). It is the SAME
/// decision the collector already reached through `XT_TAKEOVER` /
/// `unrewritable_peer_state` on every take-over cycle -- a frozen peer or a
/// helper window always marks the cycle -- except that it cannot be discharged:
/// a pin set is no protection to a collector that copies everything.
pub fn takeover_forbids_unpinnable_move() -> bool {
    may_move_under_takeover(false) == MoveLicence::NonMoving
}

/// `CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER=1` -- let the Generational pinned
/// in-place young copy (`CRATONVM_GEN_PINNED_YOUNG_COPY`, which must also be
/// set) take a young cycle whose only divert is the take-over licence: a pause
/// whose unrewritable peers are all HELPER WINDOWS the pass pinned whole
/// (gcd d3/m, 2026-09-27; proposal
/// `gcd-d2h-proposal-pinned-copy-takes-helper-window-cycles-DONE-20260928`).
///
/// **Default ON since gcj j1yg-close (2026-10-27), `=0` kills.** With it off
/// the helper-window pass reads no band into the young pin ledger,
/// [`pause_young_pin_words`] fails every pause with a take-over verdict as
/// before, and `gen_heap` never offers such a cycle to the pinned copy:
/// byte-for-byte the old behaviour. It was opt-in until the multi-thread
/// coverage of `GciPn1JavacTreeProbe` (4 threads) was measured: without it
/// every helper-window pause of that row swept in place.
///
/// With it on, a Cheney copy still cannot honour a pin set
/// ([`takeover_forbids_unpinnable_move`] is unchanged), but the pinned copy
/// can: the licence a pinning backend would get
/// ([`takeover_verdict_admits_pinned_copy`]: nothing frozen, nothing unread,
/// every window pinned from a whole band with derived pointers resolved) is
/// honoured by pinning EVERY young word of each window's register file and
/// whole stack (bases included: the band holds compiled frames, whose non-oop
/// words no channel may rewrite), read raw into this pause's ledger by the
/// pass ([`YoungPinLedger::note_peer_band`]).
///
/// gci i12/sx: read from the flag snapshot on every call, no process
/// `OnceLock` (AGENTS.md "no process globals"; i11/tk review item 1). The
/// readers are per pause (the ledger read, the plan's take-over arm, the root
/// gatherer's honoured-pins term), so the uncached read costs nothing that
/// matters, and a test's `with_thread_overrides` now reaches it: before, the
/// first reader in the test process latched the answer for every later test.
/// In production the snapshot is immutable, so the answer cannot change
/// within a run.
pub fn pinned_young_copy_takeover_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER")
}

/// gcd d3/m: does a GENERATIONAL heap honour this pause's helper-window pins,
/// for the cross-thread coverage account
/// (`conservative_roots::refresh_moving_young_coverage_for_collection`'s
/// `pins_honoured`)? `VmHeap::honours_conservative_pins` says no (a Cheney
/// copy cannot), which is right for the Cheney copy and keeps the pinned
/// depth credit out of the account -- so a helper-window pause always reads
/// `cross-thread-jit-peer`, and the take-over arm below could never see a
/// complete coverage record. With
/// [`pinned_young_copy_takeover_enabled`] and the pinned copy on, the answer
/// is yes: the only way such a pause relocates is the pinned copy's take-over
/// arm, which pins every young word of every window, and every other
/// collector decision still refuses it through the take-over licence
/// ([`takeover_forbids_unpinnable_move`] is unchanged). The root gatherer
/// (`vm/src/memory/roots.rs`) ORs this in for a Generational heap.
pub fn generational_takeover_pins_honoured() -> bool {
    gc_flags().gen_pinned_young_copy && pinned_young_copy_takeover_enabled()
}

/// gcd d3/m: the take-over licence a pinned in-place young copy may act on
/// -- the licence a pinning backend gets ([`MoveLicence::MovePinned`]), from a
/// pause that froze nobody (a frozen peer's TLAB, registers and interrupted
/// frame are a different contract, left non-moving) and found at least one
/// helper window. Pure.
pub fn takeover_verdict_admits_pinned_copy(v: TakeoverVerdict) -> bool {
    v.frozen == 0 && v.helper_windows > 0 && v.licence(true) == MoveLicence::MovePinned
}

/// gcd d3/m: may the young pin ledger stand for a pause with take-over
/// verdict `v`, given `band_windows` helper windows the pass read into it
/// whole ([`YoungPinLedger::peer_band_windows`])? Only when the verdict admits
/// the pinned copy and every one of its windows is in the ledger. Pure.
pub fn takeover_admits_band_ledger(v: TakeoverVerdict, band_windows: u32) -> bool {
    takeover_verdict_admits_pinned_copy(v) && band_windows >= v.helper_windows
}

/// `CRATONVM_G1_TAKEOVER_LICENCE=1` -- let G1 act on the take-over licence.
///
/// **Opt-in.** G1 evacuates regardless of the coverage record (its
/// `refuse_evacuation` is itself an opt-in lever), so a take-over pause whose
/// pin set is incomplete -- an unread (still running) peer, a frozen stack
/// that could not be read whole, an unpinned helper window, or a probe that
/// cannot resolve derived pointers -- evacuates whatever the pins missed. The
/// licence says `NonMoving` for exactly those pauses. Refusing them is the
/// sound answer, but a refused G1 pause reclaims nothing, so until the rate is
/// measured (`SpinPoll`, `SpinPollMark`, `G1PollStormProbe`,
/// `CRATONVM_GC_STATS=1`) it stays opt-in. See
/// `docs/feature-designs/generative-gc-architecture.md`.
fn g1_takeover_licence_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_on("CRATONVM_G1_TAKEOVER_LICENCE")
    })
}

/// G1's reading of the take-over licence: `Some(XT_TAKEOVER)` when the pause
/// must not evacuate, `None` otherwise. A pure function of its inputs so both
/// arms are testable (the lever latches for the process).
fn g1_licence_refusal(verdict: TakeoverVerdict, lever_on: bool) -> Option<usize> {
    if lever_on && verdict.licence(true) == MoveLicence::NonMoving {
        Some(incomplete_reason::XT_TAKEOVER)
    } else {
        None
    }
}

/// The refusal G1's young/mixed pause ORs into its coverage refusal
/// (`G1Collector::refuse_evacuation`'s caller): `Some(XT_TAKEOVER)` when
/// `CRATONVM_G1_TAKEOVER_LICENCE` is set and this pause's licence is
/// [`MoveLicence::NonMoving`]. `MovePinned` needs nothing new -- the region
/// pins are how G1 honours it today.
pub fn g1_takeover_licence_refusal() -> Option<usize> {
    if !g1_takeover_licence_enabled() {
        return None;
    }
    g1_licence_refusal(takeover_verdict(), true)
}

/// Accumulate one take-over pass's outcome into this cycle's totals, in the
/// calling thread's [`PauseLedger`] (see [`XtCounts`]).
pub fn publish_xt_pass(taken_over: u64, unclassified: u64, roots: u64) {
    let _ = with_ledger(|l| {
        l.xt.passes.fetch_add(1, Ordering::Relaxed);
        l.xt.taken_over.fetch_add(taken_over, Ordering::Relaxed);
        l.xt.unclassified.fetch_add(unclassified, Ordering::Relaxed);
        l.xt.roots.fetch_add(roots, Ordering::Relaxed);
    });
}

/// Accumulate the post-barrier helper-window pass's outcome, in the calling
/// thread's [`PauseLedger`].
pub fn publish_xt_helper_window(windows: u64, roots: u64) {
    let _ = with_ledger(|l| {
        l.xt.hw_windows.fetch_add(windows, Ordering::Relaxed);
        l.xt.hw_roots.fetch_add(roots, Ordering::Relaxed);
    });
}

/// `(passes, taken_over, unclassified, roots, hw_windows, hw_roots)` for the
/// cycle currently in progress, from the calling thread's [`PauseLedger`] --
/// for the thread running a pause, its own VM's. All zero (passes 0: "nobody
/// looked") during thread-local teardown.
pub fn xt_cycle_coverage() -> (u64, u64, u64, u64, u64, u64) {
    with_ledger(|l| l.xt.snapshot()).unwrap_or((0, 0, 0, 0, 0, 0))
}

// ---- The helper-window discharge verdict (gcd d2/i, 2026-09-27) ------------
//
// `XT_HELPER_WINDOWS_UNPINNED_CYCLE` in `vm/src/jit/xt_root_scan.rs` was a
// process `AtomicU64` until this change (page
// `gcd-d1b-helper-window-cycle-verdict-is-process-global-FIXED-20260928`). Each VM's
// take-over wrote it and read it back LATER -- `stw_take_over_and_wait`'s
// `TakeoverVerdict` after the pass, the coverage accounting's pinned-depth
// credit during the root scan -- and `PASS_LOCK` (Linux arm) serialised only
// the passes. VM B's take-over (no blocked peer: it stores 0) landing between
// VM A's pass (1: an unpinned window) and A's reads handed A "every window
// pinned": a pinned-depth credit and a `pins_complete` licence for a window
// nobody pinned.
//
// Thread audit, the same one the six sibling rows passed (see [`XtCounts`]):
//
// | access | where | thread |
// |---|---|---|
// | clear | [`reset_xt_cycle`] (top of `stw_take_over_and_wait`), `PauseLedger::open`, and the pass's own reset (`reset_helper_window_cycle`) | the requester |
// | write | `helper_window_pass`, both OS arms (spawns no thread) | the requester |
// | read, decision | `stw_take_over_and_wait` (`takeover_verdict_unreadable`, `helper_only` discharge, `pins_complete`) | the requester |
// | read, coverage | `conservative_roots::refresh_moving_young_coverage_for_collection` (`pinned_credit_admissible`) | the collecting thread = the requester |
//
// Every access is on the thread running the pause, so it reads what it wrote
// whether or not it is bound, and no other VM can write it. The one reading
// that cannot be made is during thread-local teardown ([`with_ledger`] answers
// `None`); both readers then fail CLOSED (a refusing window).

/// Record how many helper windows refuse the discharge this pause (unpinned
/// windows plus blocked peers whose stack could not be read), in the calling
/// thread's [`PauseLedger`]. `0` opens the verdict ("nothing refuses yet").
pub fn set_xt_helper_windows_unpinned(n: u64) {
    let _ = with_ledger(|l| l.xt.hw_unpinned.store(n, Ordering::Release));
}

/// This pause's refusing helper-window count, from the calling thread's
/// [`PauseLedger`]; `None` only during thread-local teardown, which every
/// caller must read as "refusing" (the fail-closed answer).
pub fn xt_helper_windows_unpinned() -> Option<u64> {
    with_ledger(|l| l.xt.hw_unpinned.load(Ordering::Acquire))
}

/// gcd d10/t: count one take-over peer whose machine stack this pause could
/// not read whole, into the calling thread's [`PauseLedger`] -- the
/// initiator's, as for every other take-over row (see [`XtCounts`]). Called by
/// `xt_root_scan::note_takeover_stack_incomplete` beside the process total.
/// Lock-free (a TLS read and one atomic): it runs while peers are frozen.
pub fn note_xt_takeover_stack_incomplete() {
    let _ = with_ledger(|l| l.xt.stack_incomplete.fetch_add(1, Ordering::AcqRel));
}

/// This pause's count of take-over peers whose machine stack could not be
/// read whole, from the calling thread's [`PauseLedger`]; `None` only during
/// thread-local teardown, which a caller must read as "incomplete".
///
/// The per-VM answer to "did this take-over read every frozen stack whole?".
/// `stw_take_over_and_wait` still answers it with a DELTA of the process
/// total, which another VM's overlapping take-over also moves; see
/// `gcd-d10t-takeover-stack-incomplete-is-read-as-a-process-delta-20260928`.
pub fn xt_takeover_stack_incomplete_this_pause() -> Option<u64> {
    with_ledger(|l| l.xt.stack_incomplete.load(Ordering::Acquire))
}

/// `CRATONVM_XT_BLOCKED_MONITOR_PROOF=1` -- let a peer blocked in the COMPILED
/// `monitorenter` helper answer for its compiled frames with the per-thread
/// coverage proof it ran at its blocking deposit, instead of as a helper window
/// (gcd d2/i, 2026-09-27; page
/// `gcd-d1b-thread-exit-shape-livelocks-on-forced-young-cycles-FIXED-20260928`).
///
/// **Opt-in, default OFF.** With it off nothing is armed, no deposit writes
/// the row and the take-over reads the blocked roster exactly as before.
///
/// What it changes, on Generational only (the deposit's proof is taken only
/// there): a thread blocked in `jit_monitor_enter` whose flag-raising deposit
/// PROVED its JIT entry chain rewritable (`moving_young_precise_only`: every
/// compiled-frame oop in a shadow-stack home or an oop-map slot) is treated
/// like a cooperatively-parked peer that proved the same chain at its park:
/// its depth is credited to the pause's proven ledger
/// ([`add_peer_proven_jit_depth`]) and the helper-window pass skips it, so it
/// no longer turns the cycle non-moving by itself. Its wake applies the same
/// three remaps a parked peer's resume does (`vm_exec::apply_blocked_wake_jit_remap`,
/// default ON since 2026-09-08). The Rust frames between the compiled frame and
/// the park are the part the proof does not cover; for this one helper they
/// were read end to end (`jit_monitor_enter_body` -> `monitor_enter_blocking`:
/// the receiver is pinned, the helper answers the remapped one, nothing else
/// is held), which is why the arm is set by the compiled helper's entry only,
/// never by a blocking native.
pub fn blocked_monitor_proof_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_XT_BLOCKED_MONITOR_PROOF"))
}

// JIT round 21 lane refs, REFS21-6 (`CRATONVM_XT_HELPER_WINDOW_PARK_CREDIT`,
// opt-in): the blocked peers whose helper window this pass reads for its
// stack-slot captures, depth and shadow stack only -- NOT for marking roots
// and pins from their register file and machine-stack words. Published by the
// VM's take-over driver on the collecting thread immediately before
// `helper_window_pass` and cleared right after, so it is per pass and per
// collecting thread (a second VM's collector on another thread never sees
// it). See `docs/known-issues/gc/jr21-blocked-peer-helper-window-roots-stale-rust-stack-words-20261014.md`.
thread_local! {
    static XT_PARK_CREDITED_TIDS: std::cell::RefCell<Vec<u32>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// `CRATONVM_XT_HELPER_WINDOW_PARK_CREDIT=1` (default ON since the w3o stress + fact A/B, 2026-10-16; was opt-in until the
/// GC battery gates it): see the section above. Read once.
pub fn xt_park_credit_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_XT_HELPER_WINDOW_PARK_CREDIT")
    })
}

/// Publish this pass's park-credited peers (see the section above).
pub fn set_xt_park_credited_tids(tids: Vec<u32>) {
    let _ = XT_PARK_CREDITED_TIDS.try_with(|c| *c.borrow_mut() = tids);
}

/// Withdraw them (the pass is over).
pub fn clear_xt_park_credited_tids() {
    let _ = XT_PARK_CREDITED_TIDS.try_with(|c| c.borrow_mut().clear());
}

/// Is `tid` park-credited for the helper-window pass running on this thread?
pub fn xt_park_credited(tid: u32) -> bool {
    XT_PARK_CREDITED_TIDS
        .try_with(|c| c.borrow().contains(&tid))
        .unwrap_or(false)
}

/// Blocked monitor peers credited by proof instead of scanned as helper
/// windows ([`blocked_monitor_proof_enabled`]), and the ones whose recorded
/// depth no longer matched their published slot (scanned as before).
/// Cumulative engagement census: zero in both with the flag set means no
/// pause met a proven blocked monitor peer.
pub static XT_BLOCKED_MONITOR_PEERS_PROVEN: AtomicU64 = AtomicU64::new(0);
/// See [`XT_BLOCKED_MONITOR_PEERS_PROVEN`].
pub static XT_BLOCKED_MONITOR_PEERS_DEPTH_MISMATCH: AtomicU64 = AtomicU64::new(0);

/// gcd d3/m: remember a blocked-monitor peer credited by proof this pause, so
/// the helper-window pass reads its band into the young pin ledger
/// ([`take_proven_monitor_peers_for_ledger`]).
///
/// Why the ledger needs it: the proof covers the peer's COMPILED frames (every
/// oop in a shadow home or an oop-map slot, remapped at wake), but a pause's
/// young pin ledger must also account for the peer's JIT depth and hold the
/// young words of its register file and its Rust frames, which nothing
/// rewrites. The peer deposited nothing for this pause (it blocked before the
/// pause opened), so without a band the ledger reads a depth shortfall, every
/// such cycle is `nonmoving-pin-ledger-incomplete` under the pinned copy, and
/// term 4 diverts it under option B: the proof alone could never let the
/// cycle move (`gcd-d1b-thread-exit-shape-livelocks-on-forced-young-cycles-FIXED-20260928`).
/// In the calling thread's [`PauseLedger`]; the writer and the reader are the
/// requester (`stw_take_over_and_wait`).
pub fn stash_proven_monitor_peer_for_ledger(os_tid: u32, depth: usize) {
    let _ = with_ledger(|l| l.proven_monitor_peers.lock().push((os_tid, depth)));
}

/// Drain this pause's [`stash_proven_monitor_peer_for_ledger`] rows.
pub fn take_proven_monitor_peers_for_ledger() -> Vec<(u32, usize)> {
    with_ledger(|l| std::mem::take(&mut *l.proven_monitor_peers.lock())).unwrap_or_default()
}

// ---- Raw JNI local references (gcd d4/m, 2026-09-28) ------------------------
//
// With `CRATONVM_JNI_INDIRECT_LOCALS` off (the default) a JNI local reference
// is the object's raw address, and a native's own copies of it -- in C locals,
// registers, C structs -- are rewritten by nothing
// (`common-w2c-jni-local-refs-are-raw-addresses`). Every collection that MOVES
// a young object while such a native holds a local to it leaves that copy
// naming vacated bytes.
//
// Today (pinned copy off) a young cycle under a live compiled frame is a sweep
// (term 4), so the only moving cycles a raw local meets are JIT-cold Cheney
// copies. The pinned copy (`CRATONVM_GEN_PINNED_YOUNG_COPY`), its take-over
// arm and option B (`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4`) turn JIT-warm cycles
// into moving ones, and the young pin ledger they read holds a raw local only
// when it happens to sit inside a depositing thread's swept band: a thread with
// no JIT entry (an interpreter-called native parked in a JNIEnv up-call, a
// foreign-attached thread between calls) deposits nothing, and a local stored
// off-stack is in no band at all. So those three arms must not relocate while
// any thread of the VM holds raw locals: the ledger reads INCOMPLETE then
// (`pause_young_pin_read`), and the cycle is the sweep it was before -- exactly
// today's behaviour for those pauses, never a wider exposure. Cheney cycles are
// unchanged (the pre-existing `common-w2c` exposure).
//
// The count is per VM (this ledger) and exact at a pause: a thread enters
// native code only as a counted mutator, so its increment happens before it
// could park or block for the pause, and its decrement only after it has left
// the native. The JNI dispatch owns the scopes (`vm/src/native/**`, the JNI
// region of `vm_exec.rs`).

/// One JNI native call, or one foreign attachment, holding raw local
/// references in THIS VM (see the section comment). Counted while alive;
/// `enter(.., false)` (locals indirect) counts nothing.
#[must_use = "the raw-local count drops when the scope drops"]
pub struct RawJniLocalsScope {
    ledger: Option<std::sync::Arc<PauseLedger>>,
}

impl RawJniLocalsScope {
    /// Count the calling thread as holding raw JNI locals in `ledger`'s VM
    /// until the scope drops, when `raw`.
    pub fn enter(ledger: &std::sync::Arc<PauseLedger>, raw: bool) -> Self {
        if !raw {
            return Self { ledger: None };
        }
        ledger.raw_jni_locals.fetch_add(1, Ordering::AcqRel);
        Self {
            ledger: Some(std::sync::Arc::clone(ledger)),
        }
    }
}

impl Drop for RawJniLocalsScope {
    fn drop(&mut self) {
        // gci i4/pn: the same saturating, counted close as the unscoped pair.
        // A bare `fetch_sub` wrapped to `usize::MAX` when a stray
        // `note_raw_jni_locals_closed` had already taken this scope's count,
        // and `raw_jni_locals_open` then read `true` for the rest of the VM's
        // life (the pinned copy and option B silently off, nothing counted).
        if let Some(l) = self.ledger.take() {
            note_raw_jni_locals_closed(&l);
        }
    }
}

/// The unscoped pair for a holder whose lifetime is not one Rust scope (a
/// foreign attachment, counted from attach to detach). Every `open` must be
/// matched by exactly one `closed` on the same ledger.
pub fn note_raw_jni_locals_open(ledger: &PauseLedger) {
    ledger.raw_jni_locals.fetch_add(1, Ordering::AcqRel);
}

/// See [`note_raw_jni_locals_open`]. Saturates at zero; gch h2/yg: a close
/// that finds zero is counted ([`raw_jni_locals_census`]).
pub fn note_raw_jni_locals_closed(ledger: &PauseLedger) {
    if ledger
        .raw_jni_locals
        .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
        .is_err()
    {
        ledger
            .raw_jni_locals_underflows
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// gch h2/yg (`gch-h1yp-raw-jni-locals-count-turns-the-pinned-copy-off-for-the-vms-life`):
/// `(count_now, close_underflows)` of `ledger` (a VM's, e.g. through
/// `GcBarrier::pause_ledger`) -- the standing raw-JNI-locals count and the
/// unbalanced closes [`note_raw_jni_locals_closed`] absorbed. A run that ends
/// with `count_now > 0` and no thread in a native leaked an open (the pinned
/// copy and option B were then off for the rest of the run);
/// `close_underflows > 0` is an imbalance the other way. Takes the ledger
/// explicitly because the shutdown summary runs on a thread no pause binds.
pub fn raw_jni_locals_census(ledger: &PauseLedger) -> (u64, u64) {
    (
        // Widening: a thread count.
        ledger.raw_jni_locals.load(Ordering::Acquire) as u64,
        ledger.raw_jni_locals_underflows.load(Ordering::Relaxed),
    )
}

/// Does anything read the raw-JNI-locals count? Only the young pin ledger's
/// relocating consumers do -- the pinned copy (`CRATONVM_GEN_PINNED_YOUNG_COPY`,
/// its take-over arm included) and option B (`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4`).
/// The JNI dispatch passes `raw && raw_jni_locals_matter()` to
/// [`RawJniLocalsScope::enter`], so the default path pays one cached flag load
/// per native call and never touches the shared counter. Latched for the
/// process, so an `open` and its `closed` always agree.
///
/// zt t1/rl (`zgc-zr1rl-raw-jni-locals-on-a-native-peer`, z2-r2 hunk X2 (a)):
/// also true under `CRATONVM_ZGC_RELOCATE_REFUSES_RAW_JNI_LOCALS` (`=1`, or
/// unset with `CRATONVM_JNI_INDIRECT_LOCALS=0`),
/// whose reader is ZGC's `relocate_refusal`. The count is still kept only for
/// a dispatch whose locals are raw (`CRATONVM_JNI_INDIRECT_LOCALS=0`, or a
/// thread's raw escape): with the switch on, an indirect-locals dispatch pays
/// a ledger `Arc` clone and one TLS bump and no shared RMW; off (the default),
/// nothing changes but one latched load ([`zgc_relocate_refuses_raw_jni_locals`]).
/// A generational or G1 heap reads the count only through its
/// own consumers above, which stay gated by their own flags: keeping it under
/// this switch changes nothing they do.
pub fn raw_jni_locals_matter() -> bool {
    young_pin_ledger_licenses_moves() || zgc_relocate_refuses_raw_jni_locals()
}

/// `CRATONVM_ZGC_RELOCATE_REFUSES_RAW_JNI_LOCALS` (zt t1/rl; unset follows
/// `CRATONVM_JNI_INDIRECT_LOCALS` since zt t14, `=1` forces it on, `=0` kills
/// it): ZGC
/// refuses a slide while a thread of its VM holds raw JNI local references
/// ([`raw_jni_locals_open_or_closed`]), and the JNI dispatch keeps that count
/// ([`raw_jni_locals_matter`]).
///
/// zt t2/x1 (finding X1-1): latched for the process outside this crate's unit
/// tests. [`raw_jni_locals_matter`] runs on EVERY JNI native dispatch of every
/// collector (`vm/src/native/jni.rs` `raw_locals_counted`), and the t1 cut
/// read the flag snapshot there per call (`runtime_var_os`: a name-set probe,
/// a map lookup and an `OsString` clone), where the doc above promises "one
/// cached flag load". The latch also makes the count's keeper (the dispatch)
/// and its reader (`relocate_refusal`) agree for the whole run. This crate's
/// tests read per call so a thread override reaches the slide test.
pub fn zgc_relocate_refuses_raw_jni_locals() -> bool {
    // zt t14 (rl3 item 5, rows `rl6s_*`): unset is ON exactly when JNI
    // locals are raw for the whole VM (`CRATONVM_JNI_INDIRECT_LOCALS=0`):
    // there `rl6s_ctl` lost every upcall (`upcall ok 0/40`, HotSpot 40/40)
    // and the switch made it SAME; with indirect locals (the default) the
    // slide already rewrites the handles, so the count would only add a
    // ledger clone per JNI dispatch for nothing.
    fn read() -> bool {
        use cratonvm_types::flags::{runtime_var, value_is_off};
        match runtime_var("CRATONVM_ZGC_RELOCATE_REFUSES_RAW_JNI_LOCALS") {
            Ok(value) if !value.trim().is_empty() => !value_is_off(&value),
            _ => runtime_var("CRATONVM_JNI_INDIRECT_LOCALS")
                .ok()
                .is_some_and(|value| !value.trim().is_empty() && value_is_off(&value)),
        }
    }
    if cfg!(test) {
        read()
    } else {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(read)
    }
}

/// Does a consumer of this pause's young pin ledger MOVE objects on its word
/// -- the pinned in-place copy (`CRATONVM_GEN_PINNED_YOUNG_COPY`), term 4's
/// option B (`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4`), or (JIT r20 wave 18, lane
/// gcI) precise-root promotion (`CRATONVM_GEN_PRECISE_ROOT_PROMOTE`), whose
/// non-moving sweep promotes a root value only when the ledger proves no
/// compiled-frame word names it ([`ledger_names_young_object`])? Then the
/// deposits spend no deadness claim, the raw-JNI-locals count is kept, and
/// peer bands are read into the ledger. Latched for the process (a flags
/// snapshot read).
pub fn young_pin_ledger_licenses_moves() -> bool {
    let f = gc_flags();
    f.gen_pinned_young_copy || f.gen_young_pin_ledger_term4 || f.gen_precise_root_promote
}

/// JIT r20 wave 18, lane gcI: this pause's young pin ledger as a predicate
/// "does some compiled-frame / native-band / register word name an address
/// inside `[start, end)`?", or `None` when the ledger is INCOMPLETE (then
/// nothing may be moved on its word). One ledger read; the words are sorted
/// once. For the precise-root promotion veto in `sweep_young_non_moving`.
pub fn pause_young_pin_word_ranges() -> Option<impl Fn(usize, usize) -> bool> {
    let mut words: Vec<usize> = Vec::new();
    if !pause_young_pin_words(&mut words) {
        return None;
    }
    words.sort_unstable();
    words.dedup();
    Some(move |start: usize, end: usize| {
        let i = words.partition_point(|&w| w < start);
        words.get(i).is_some_and(|&w| w < end)
    })
}

/// [`pause_young_pin_word_ranges`] answered for one young object at `base`
/// (sized from its header). `true` -- name it, keep it pinned -- when its size
/// cannot be decided.
pub fn ledger_names_young_object(names: &impl Fn(usize, usize) -> bool, base: usize) -> bool {
    // SAFETY: `base` is a root value the sweep's own mark resolved to a young
    // object start; its header is readable for the whole pause.
    let header = unsafe { &*(base as *const crate::heap::ObjectHeader) };
    let size = crate::gen_heap::gen_object_total_size(header);
    size == 0 || names(base, base.saturating_add(size))
}

#[cfg(test)]
mod jr20_w18_gci_ledger_veto_tests {
    use super::ledger_names_young_object;
    use crate::heap::{ArrayElementType, ObjectHeader, ObjectKind};

    /// A ledger word at the object's base or anywhere inside it vetoes the
    /// precise-root exemption; a word just past its end, or none, does not.
    #[test]
    fn a_ledger_word_inside_the_object_names_it_and_one_past_the_end_does_not() {
        let mut buf = [0u64; 16];
        let header = ObjectHeader::new(
            cratonvm_types::ClassId::new(7),
            ObjectKind::Object,
            ArrayElementType::Reference,
            0,
            2,
        );
        let size = crate::gen_heap::gen_object_total_size(&header);
        // SAFETY: `buf` is 128 bytes, 8-aligned, larger than any header.
        unsafe { std::ptr::write(buf.as_mut_ptr() as *mut ObjectHeader, header) };
        let base = buf.as_ptr() as usize;
        assert!(size > 0 && size <= 128, "size {size}");
        let in_words =
            |words: Vec<usize>| move |s: usize, e: usize| words.iter().any(|&w| w >= s && w < e);
        assert!(ledger_names_young_object(&in_words(vec![base]), base));
        assert!(ledger_names_young_object(
            &in_words(vec![base + size - 8]),
            base
        ));
        assert!(!ledger_names_young_object(
            &in_words(vec![base + size]),
            base
        ));
        assert!(!ledger_names_young_object(&in_words(Vec::new()), base));
    }
}

/// Does any thread of the calling thread's VM hold raw JNI local references
/// now? Read by the collecting thread (bound to its own VM's ledger). `true`
/// -- fail closed -- when the ledger cannot be reached (thread-local teardown).
pub fn raw_jni_locals_open() -> bool {
    with_ledger(|l| l.raw_jni_locals.load(Ordering::Acquire) > 0).unwrap_or(true)
}

/// [`raw_jni_locals_open`] that answers `false` -- not the fail-closed `true`
/// -- when the calling thread reaches no ledger (zt t1/rl, z2-r2 hunk X2 (b)):
/// for ZGC's `relocate_refusal`, where an unreachable ledger refusing would
/// stop every slide of the run rather than one cycle. The collecting thread
/// is bound to its VM's ledger during a pause, so on the production path the
/// two readers agree.
pub fn raw_jni_locals_open_or_closed() -> bool {
    with_ledger(|l| l.raw_jni_locals.load(Ordering::Acquire) > 0).unwrap_or(false)
}

#[cfg(test)]
mod helper_window_verdict_ledger_tests {
    use super::*;
    use std::sync::Arc;

    /// gcd d2/i: VM A's pass recorded an unpinned window; VM B's take-over,
    /// which found none, records 0 in ITS ledger. A's bound read still says
    /// "one refuses" -- it used to read B's 0 through the process static.
    #[test]
    fn another_vms_zero_does_not_overwrite_this_pauses_refusal() {
        let a = Arc::new(PauseLedger::new());
        let b = Arc::new(PauseLedger::new());
        with_pause_ledger(&a, || set_xt_helper_windows_unpinned(1));
        let b2 = Arc::clone(&b);
        std::thread::spawn(move || {
            with_pause_ledger(&b2, || {
                reset_xt_cycle();
                set_xt_helper_windows_unpinned(0);
            })
        })
        .join()
        .unwrap();
        assert_eq!(with_pause_ledger(&a, xt_helper_windows_unpinned), Some(1));
        assert_eq!(with_pause_ledger(&b, xt_helper_windows_unpinned), Some(0));
    }

    /// The row is per pause: the take-over's `reset_xt_cycle` and the pause
    /// opener both clear it, so a pause that runs no pass reads 0, not the
    /// previous pause's refusal.
    #[test]
    fn the_refusal_is_cleared_by_the_take_over_reset_and_by_the_pause_open() {
        let l = Arc::new(PauseLedger::new());
        with_pause_ledger(&l, || {
            set_xt_helper_windows_unpinned(3);
            reset_xt_cycle();
            assert_eq!(xt_helper_windows_unpinned(), Some(0));
            set_xt_helper_windows_unpinned(2);
            reset_peer_proven_jit_depth();
            assert_eq!(xt_helper_windows_unpinned(), Some(0));
        });
    }
}

#[cfg(test)]
mod xt_post_quota_tests {
    use super::*;

    /// gc-common w4-a: the post-quota take-over time is per cycle — published
    /// once by the take-over, read by the pause line, and zeroed by the next
    /// cycle's `reset_xt_cycle` so a pause with no take-over reads 0, not the
    /// previous pause's figure.
    #[test]
    fn the_post_quota_time_is_reset_with_the_cycle() {
        publish_xt_post_quota_ns(12_345);
        assert_eq!(xt_post_quota_ns(), 12_345);
        // gc-common w6-f: the passes' share of `ttsp_us` accumulates per pass
        // and is zeroed with the cycle.
        add_xt_pass_ns(1_000);
        add_xt_pass_ns(500);
        assert_eq!(xt_pass_ns(), 1_500);
        reset_xt_cycle();
        assert_eq!(xt_post_quota_ns(), 0);
        assert_eq!(xt_pass_ns(), 0);
    }
}

/// Is `addr` a currently-registered Weak/Soft/Phantom referent this cycle?
/// Consulted by the non-moving young sweep for each kept-in-place survivor.
///
/// gcj j1/rf: answers for the most recent publication on this thread;
/// [`is_watched_referent_for`] answers for one heap.
pub fn is_watched_referent(addr: usize) -> bool {
    WATCHED_REFERENTS.with(|s| {
        let s = s.borrow();
        s.row(s.current).is_some_and(|set| set.contains(&addr))
    })
}

/// gcj j1/rf — [`is_watched_referent`] for the heap whose key is `key` (its
/// own row, else the key-0 row).
pub fn is_watched_referent_for(key: usize, addr: usize) -> bool {
    WATCHED_REFERENTS.with(|s| {
        s.borrow()
            .row_for_reader(key)
            .is_some_and(|set| set.contains(&addr))
    })
}

/// Snapshot of the watched-referent set, or `None` when it is empty.
///
/// The set lives in a `thread_local!` owned by the collecting thread, so a
/// parallel sweep worker cannot consult it directly (it would see its own,
/// always-empty, copy). The collector snapshots it once before spawning
/// workers; the set is sized by the VM's reference processor, not by the
/// young generation, so the clone is cheap and usually skipped entirely.
///
/// gci i21/ws: the snapshot shares the published set (an `Arc` clone), so
/// taking one costs no copy and no rehash however many reference rows the
/// processor holds.
///
/// gcj j1/rf: the most recent publication on this thread; a heap's own
/// collection asks [`watched_referents_snapshot_for`].
pub fn watched_referents_snapshot() -> Option<WatchedSnapshot> {
    WATCHED_REFERENTS.with(|s| {
        let s = s.borrow();
        s.row(s.current)
            .filter(|set| !set.is_empty())
            .map(|set| WatchedSnapshot(std::sync::Arc::clone(set)))
    })
}

/// gcj j1/rf — [`watched_referents_snapshot`] for the heap whose key is
/// `key`: its own row, else the key-0 row (an unkeyed publication: the kill
/// switch, a test); `None` when that row is empty or absent.
pub fn watched_referents_snapshot_for(key: usize) -> Option<WatchedSnapshot> {
    WATCHED_REFERENTS.with(|s| {
        s.borrow()
            .row_for_reader(key)
            .filter(|set| !set.is_empty())
            .map(|set| WatchedSnapshot(std::sync::Arc::clone(set)))
    })
}

#[cfg(test)]
mod gcj_j1rf_watched_table_tests {
    use super::*;

    /// gcj j1/rf (section 2 of
    /// `common-watched-referent-set-is-per-thread-not-per-heap-20261012`): a
    /// second heap's publication nested inside the first heap's window (after
    /// its publish, before its collection reads) neither replaces the first
    /// heap's row nor is read by it; the unkeyed reads answer for the most
    /// recent publication, as the one-slot set did.
    #[test]
    fn gcj_j1rf_a_nested_publication_for_another_heap_keeps_each_heaps_row() {
        set_watched_referents(&[]);
        let (a, b) = (0x7f00_0000_0000usize, 0x7e00_0000_0000usize);
        set_watched_referents_for(a, &[0x10, 0x20]);
        add_watched_referents(&[0x30]); // the string pool's add, right after
        set_watched_referents_for(b, &[0x40]);
        // B's collection reads B only.
        let snap_b = watched_referents_snapshot_for(b).expect("B published");
        assert_eq!(snap_b.len(), 1);
        assert!(snap_b.contains(&0x40) && !snap_b.contains(&0x10));
        assert!(is_watched_referent_for(b, 0x40) && !is_watched_referent_for(b, 0x30));
        // The unkeyed reads answer for the most recent publication (B).
        assert!(is_watched_referent(0x40) && !is_watched_referent(0x10));
        // A's collection still reads A's set, its add included.
        let snap_a = watched_referents_snapshot_for(a).expect("A kept");
        assert_eq!(snap_a.len(), 3);
        for addr in [0x10usize, 0x20, 0x30] {
            assert!(snap_a.contains(&addr) && is_watched_referent_for(a, addr));
        }
        assert!(!snap_a.contains(&0x40));
        // A heap with no row of its own and no unkeyed row reads nothing.
        assert!(watched_referents_snapshot_for(0x7d00_0000_0000).is_none());
        drop((snap_a, snap_b));
        set_watched_referents(&[]);
        assert!(watched_referents_snapshot_for(a).is_none(), "an unkeyed publish resets");
        assert!(watched_referents_snapshot_for(b).is_none());
    }

    /// The kill-switch shape (`CRATONVM_GC_WATCHED_SET_PER_HEAP=0`): every
    /// publication is unkeyed, so a keyed reader falls back to the one key-0
    /// row -- the one-slot set exactly, including the positive control: a
    /// nested unkeyed publication REPLACES the first one (the hazard).
    #[test]
    fn gcj_j1rf_unkeyed_publications_are_the_one_slot_set() {
        let a = 0x7f00_0000_0000usize;
        set_watched_referents(&[0x10]);
        assert!(is_watched_referent_for(a, 0x10), "the fallback row");
        set_watched_referents(&[0x40]); // a nested VM's unkeyed publish
        let snap = watched_referents_snapshot_for(a).expect("the last publish");
        assert!(snap.contains(&0x40) && !snap.contains(&0x10), "replaced");
        drop(snap);
        // A keyed row wins over the fallback for its own heap only.
        set_watched_referents_for(a, &[0x50]);
        assert!(is_watched_referent_for(a, 0x50) && !is_watched_referent_for(a, 0x40));
        assert!(is_watched_referent_for(0x1234_0000, 0x40), "others still fall back");
        set_watched_referents(&[]);
        assert!(watched_referents_snapshot().is_none());
    }

    /// The table holds at most `WATCHED_TABLE_ROWS` heaps; the least recently
    /// published row is the one evicted, and republishing refreshes a row.
    #[test]
    fn gcj_j1rf_the_table_evicts_the_least_recently_published_row() {
        set_watched_referents(&[]);
        let key = |i: usize| 0x1000_0000 * (i + 1);
        for i in 0..WATCHED_TABLE_ROWS {
            set_watched_referents_for(key(i), &[i + 8]);
        }
        // Refresh row 0, then one more heap evicts row 1 (the oldest now).
        set_watched_referents_for(key(0), &[8]);
        set_watched_referents_for(key(WATCHED_TABLE_ROWS), &[99]);
        assert!(is_watched_referent_for(key(0), 8), "refreshed row kept");
        assert!(!is_watched_referent_for(key(1), 9), "oldest row evicted");
        assert!(is_watched_referent_for(key(2), 10));
        assert!(is_watched_referent_for(key(WATCHED_TABLE_ROWS), 99));
        set_watched_referents(&[]);
    }
}

#[cfg(test)]
mod gcj_j3rf3_watched_reuse_tests {
    use super::*;

    fn contents(key: usize) -> Vec<usize> {
        let mut v: Vec<usize> = watched_referents_snapshot_for(key)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();
        v.sort_unstable();
        v
    }

    /// gcj j3/rf3 (`CRATONVM_GEN_WATCHED_SET_REUSE_UNCHANGED`): an unchanged
    /// list reuses the row and publishes exactly the list again, with the
    /// adds made after the previous publication (the String table's young
    /// entries) removed; an address the list itself holds survives that
    /// removal. A changed list, a held snapshot, a plain publish or an
    /// unkeyed reset in between forces a rebuild, and the contents are the
    /// list on every arm.
    #[test]
    fn gcj_j3rf3_an_unchanged_list_reuses_the_row_and_drops_the_adds() {
        set_watched_referents(&[]);
        let k = 0x7c00_0000_0000usize;
        assert!(!set_watched_referents_reusing_for(k, vec![0x10, 0x20, 0x20]), "new row");
        add_watched_referents(&[0x30, 0x10]); // 0x10 is in the list already
        assert_eq!(contents(k), vec![0x10, 0x20, 0x30]);
        assert!(set_watched_referents_reusing_for(k, vec![0x10, 0x20, 0x20]), "reused");
        assert_eq!(contents(k), vec![0x10, 0x20], "the add is gone, 0x10 stays");
        // A changed list rebuilds.
        assert!(!set_watched_referents_reusing_for(k, vec![0x10, 0x28]));
        assert_eq!(contents(k), vec![0x10, 0x28]);
        // A held snapshot forbids the in-place reuse; contents still exact.
        add_watched_referents_for(k, &[0x40]);
        let held = watched_referents_snapshot_for(k).expect("published");
        assert!(!set_watched_referents_reusing_for(k, vec![0x10, 0x28]), "snapshot held");
        assert_eq!(contents(k), vec![0x10, 0x28]);
        assert!(held.contains(&0x40), "the snapshot keeps what it was taken with");
        drop(held);
        assert!(set_watched_referents_reusing_for(k, vec![0x10, 0x28]), "reusable again");
        // A plain publish drops the memo.
        set_watched_referents_for(k, &[0x10, 0x28]);
        assert!(!set_watched_referents_reusing_for(k, vec![0x10, 0x28]), "memo dropped");
        // An unkeyed reset drops the row and its memo; an add re-creates the
        // row without one, so the next reusing publish rebuilds.
        set_watched_referents(&[]);
        add_watched_referents_for(k, &[0x50]);
        assert!(!set_watched_referents_reusing_for(k, vec![0x10, 0x28]), "no stale memo");
        assert_eq!(contents(k), vec![0x10, 0x28]);
        set_watched_referents(&[]);
    }

    fn major_contents(key: usize) -> Vec<usize> {
        let mut v: Vec<usize> = watched_major_snapshot_for(key)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();
        v.sort_unstable();
        v
    }

    /// gcj j3/rf3 (`CRATONVM_GC_STRING_TABLE_SEED_ONLY_YOUNG`): the seed-only
    /// row is read by the major's view only (the young sweeps' snapshot does
    /// not see it), joins the watched row there, and is cleared by the heap's
    /// next publication on both publish paths; another heap's row is apart.
    /// Without a seed-only row the major's view shares the watched storage.
    #[test]
    fn gcj_j3rf3_the_seed_only_row_is_read_by_the_major_view_only() {
        set_watched_referents(&[]);
        let (a, b) = (0x7b00_0000_0000usize, 0x7a00_0000_0000usize);
        set_watched_referents_for(a, &[0x10]);
        let plain = watched_major_snapshot_for(a).expect("published");
        let sweep = watched_referents_snapshot_for(a).expect("published");
        assert!(plain.shares_storage_with(&sweep), "no seed-only row: the same set");
        drop((plain, sweep));
        add_seed_only_referents(&[0x20, 0x30]);
        assert_eq!(contents(a), vec![0x10], "the sweeps do not see it");
        assert_eq!(major_contents(a), vec![0x10, 0x20, 0x30], "the major does");
        set_watched_referents_for(b, &[0x40]);
        assert_eq!(major_contents(b), vec![0x40], "another heap's row is apart");
        assert_eq!(major_contents(a), vec![0x10, 0x20, 0x30], "and keeps its own");
        set_watched_referents_for(a, &[0x10]);
        assert_eq!(major_contents(a), vec![0x10], "the next publish clears it");
        add_seed_only_referents(&[0x50]);
        assert!(!set_watched_referents_reusing_for(a, vec![0x10]), "plain publish: no memo");
        add_seed_only_referents(&[0x50]);
        assert!(set_watched_referents_reusing_for(a, vec![0x10]), "reused");
        assert_eq!(major_contents(a), vec![0x10], "the reuse path clears it too");
        // Seed-only with an empty watched row still reaches the major.
        set_watched_referents_for(a, &[]);
        add_seed_only_referents(&[0x60]);
        assert!(watched_referents_snapshot_for(a).is_none());
        assert_eq!(major_contents(a), vec![0x60]);
        set_watched_referents(&[]);
        assert!(watched_major_snapshot_for(a).is_none(), "the reset drops every row");
    }
}

#[cfg(test)]
mod gci_i21ws_watched_tests {
    use super::*;

    /// Deterministic pseudo-random heap-like addresses (8-aligned, some
    /// duplicated, some sharing low bits), so the equality test below covers
    /// collisions in both hashers' buckets.
    fn addrs(n: usize) -> Vec<usize> {
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let a = 0x7f00_0000_0000usize + ((x as usize) & 0x00ff_fff8);
            out.push(a);
            if i % 5 == 0 {
                out.push(a); // a reference row naming the same word twice
            }
            if i % 7 == 0 {
                out.push(0x7f00_0000_0000usize + (i << 12)); // page-aligned run
            }
        }
        out
    }

    /// The Fx-hashed published set answers EXACTLY as the std `HashSet` it
    /// replaced: the same members (sorted iteration equal), the same `len`,
    /// and the same `contains` verdict for every published address, every
    /// near miss (+/-8, +4) and a run of unrelated words -- through the
    /// snapshot and through `is_watched_referent`.
    #[test]
    fn gci_i21ws_the_fx_watched_set_answers_as_the_std_set() {
        let published = addrs(20_000);
        let reference: std::collections::HashSet<usize> = published.iter().copied().collect();
        set_watched_referents(&published);
        let snap = watched_referents_snapshot().expect("non-empty");
        assert_eq!(snap.len(), reference.len());
        let mut ours: Vec<usize> = snap.iter().copied().collect();
        let mut theirs: Vec<usize> = reference.iter().copied().collect();
        ours.sort_unstable();
        theirs.sort_unstable();
        assert_eq!(ours, theirs, "same members");
        let mut via_into_iter: Vec<usize> = (&snap).into_iter().copied().collect();
        via_into_iter.sort_unstable();
        assert_eq!(via_into_iter, theirs);
        for &a in &published {
            for q in [a, a.wrapping_add(8), a.wrapping_sub(8), a.wrapping_add(4)] {
                assert_eq!(snap.contains(&q), reference.contains(&q), "{q:#x}");
                assert_eq!(is_watched_referent(q), reference.contains(&q), "{q:#x}");
            }
        }
        for q in (0..4096usize).map(|i| 0x1000 + i * 8) {
            assert_eq!(snap.contains(&q), reference.contains(&q), "{q:#x}");
        }
        drop(snap);
        // An add after the publish answers as the std set's `extend`.
        let extra = [0x10usize, 0x18, published[3]];
        add_watched_referents(&extra);
        let mut reference = reference;
        reference.extend(extra.iter().copied());
        let snap = watched_referents_snapshot().expect("non-empty");
        assert_eq!(snap.len(), reference.len());
        for q in extra.iter().chain(published.iter()) {
            assert_eq!(snap.contains(q), reference.contains(q), "{q:#x}");
        }
        drop(snap);
        set_watched_referents(&[]);
        assert!(watched_referents_snapshot().is_none());
    }

    /// Two snapshots of one publication share it (no copy); both answer as
    /// the published addresses; an empty publication answers `None`.
    #[test]
    fn gci_i21ws_watched_snapshot_shares_the_published_set() {
        set_watched_referents(&[0x1000, 0x2000, 0x3000, 0x2000]);
        let a = watched_referents_snapshot().expect("non-empty");
        let b = watched_referents_snapshot().expect("non-empty");
        assert!(a.shares_storage_with(&b), "a snapshot is an Arc clone, not a copy");
        assert_eq!(a.len(), 3, "duplicates collapse");
        for addr in [0x1000usize, 0x2000, 0x3000] {
            assert!(a.contains(&addr) && is_watched_referent(addr));
        }
        assert!(!a.contains(&0x1008) && !is_watched_referent(0x1008));
        drop(a);
        drop(b);
        set_watched_referents(&[]);
        assert!(watched_referents_snapshot().is_none());
        assert!(!is_watched_referent(0x1000));
    }

    /// A snapshot held across a republish or an add keeps the contents it was
    /// taken with (copy-on-write), and the thread's set has the new ones.
    #[test]
    fn gci_i21ws_a_held_snapshot_never_changes_under_its_holder() {
        set_watched_referents(&[0x10, 0x20]);
        let held = watched_referents_snapshot().expect("non-empty");
        add_watched_referents(&[0x30]);
        assert!(!held.contains(&0x30), "the add copied, the holder kept its set");
        assert!(is_watched_referent(0x30));
        set_watched_referents(&[0x40]);
        assert!(held.contains(&0x10) && held.contains(&0x20) && !held.contains(&0x40));
        assert!(is_watched_referent(0x40) && !is_watched_referent(0x10));
        drop(held);
        // Unshared again: republishing reuses the set in place.
        set_watched_referents(&[0x50]);
        assert!(is_watched_referent(0x50) && !is_watched_referent(0x40));
        add_watched_referents(&[]);
        assert_eq!(watched_referents_snapshot().map(|s| s.len()), Some(1));
        set_watched_referents(&[]);
    }

    /// `from_addrs` / `collect` build the same set the publisher does.
    #[test]
    fn gci_i21ws_watched_snapshot_constructors_agree() {
        let a = WatchedSnapshot::from_addrs(&[8, 16, 16]);
        let b: WatchedSnapshot = [16usize, 8].into_iter().collect();
        assert_eq!(a.len(), 2);
        assert!(b.contains(&8) && b.contains(&16) && !b.is_empty());
        assert!(!a.shares_storage_with(&b));
        assert!(WatchedSnapshot::default().is_empty());
    }
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

    /// gcf f1/j: republishing the same pin set keeps the entry; a different
    /// set replaces it; an empty one removes it.
    #[test]
    fn republishing_the_same_pin_set_keeps_the_entry() {
        let tid = std::thread::current().id();
        let entry = || recover(pinned_jit_map()).get(&tid).cloned();
        publish_pinned_jit_roots(&[0x1000, 0x2000, 0x1000]);
        let first = entry().expect("published");
        publish_pinned_jit_roots(&[0x2000, 0x1000]);
        assert_eq!(entry(), Some(first));
        publish_pinned_jit_roots(&[0x2000]);
        assert_eq!(entry(), Some([0x2000usize].into_iter().collect()));
        publish_pinned_jit_roots(&[0x2000, 0x3000]);
        assert_eq!(entry().map(|s| s.len()), Some(2));
        publish_pinned_jit_roots(&[]);
        assert_eq!(entry(), None);
    }

    /// gci i17/dk (i16/dp D3): an unchanged republication returns without the
    /// process-global map lock and leaves the same entry; an add, a clear, an
    /// empty publication or one past the compare bound drops the thread's
    /// record, and the next publication takes the locked path again. Runs on
    /// its own thread so its record starts empty.
    #[test]
    fn gci_i17dk_unchanged_republication_skips_the_map_lock() {
        const A: usize = 0x17d0_1000;
        const B: usize = 0x17d0_2000;
        const C: usize = 0x17d0_3000;
        std::thread::spawn(|| {
            let tid = std::thread::current().id();
            let entry = || recover(pinned_jit_map()).get(&tid).cloned();
            assert!(!published_pins_answer(&[A]), "no record before any publication");
            publish_pinned_jit_roots(&[A, B, A]);
            let first = entry().expect("published");
            assert!(published_pins_answer(&[B, A]));
            assert!(published_pins_answer(&[A, B, B]));
            assert!(!published_pins_answer(&[A]));
            assert!(!published_pins_answer(&[A, B, C]));

            // Another thread holds the map lock: the unchanged republication
            // must return while it is held.
            let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
            let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
            let holder = std::thread::spawn(move || {
                let map = recover(pinned_jit_map());
                held_tx.send(()).expect("held");
                let returned = done_rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .is_ok();
                drop(map);
                returned
            });
            held_rx.recv().expect("the holder locked the map");
            publish_pinned_jit_roots(&[B, A]);
            let _ = done_tx.send(());
            assert!(
                holder.join().expect("holder"),
                "an unchanged republication waited for the map lock"
            );
            assert_eq!(entry(), Some(first.clone()), "same entry");

            // An add changes the entry: the record no longer answers.
            add_pinned_jit_root(C);
            assert!(!published_pins_answer(&[A, B]));
            assert_eq!(entry().map(|s| s.len()), Some(3));
            publish_pinned_jit_roots(&[A, B]);
            assert_eq!(entry(), Some(first.clone()), "the locked path replaced it");
            assert!(published_pins_answer(&[A, B]));

            // A clear removes the entry and the record.
            clear_pinned_jit_roots();
            assert!(!published_pins_answer(&[A, B]));
            assert_eq!(entry(), None);
            publish_pinned_jit_roots(&[A, B]);
            assert_eq!(entry(), Some(first.clone()), "republished after the clear");

            // Past the compare bound: published, never recorded.
            let wide: Vec<usize> = (0..=PIN_REPUBLISH_COMPARE_MAX)
                .map(|i| 0x17d1_0000 + 8 * i)
                .collect();
            publish_pinned_jit_roots(&wide);
            assert!(!published_pins_answer(&wide));
            assert_eq!(entry().map(|s| s.len()), Some(wide.len()));

            // An empty publication removes the entry and the record.
            publish_pinned_jit_roots(&[A]);
            assert!(published_pins_answer(&[A]));
            publish_pinned_jit_roots(&[]);
            assert!(!published_pins_answer(&[A]));
            assert_eq!(entry(), None);
            publish_pinned_jit_roots(&[A]);
            assert_eq!(entry(), Some([A].into_iter().collect()));
            publish_pinned_jit_roots(&[]);
        })
        .join()
        .expect("worker");
    }

    /// gcg g3/e H1 (gch h1/yp): the split puts the caller's own entry in
    /// `own` and another thread's in `others`. Other tests may publish
    /// concurrently, so `others` is checked by containment only.
    #[test]
    fn gcg_g3e_the_pin_snapshot_split_separates_the_callers_own_words() {
        const MINE: usize = 0x3e3e_1000;
        const PEER: usize = 0x3e3e_2000;
        publish_pinned_jit_roots(&[MINE]);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let peer = std::thread::spawn(move || {
            publish_pinned_jit_roots(&[PEER]);
            ready_tx.send(()).expect("ready");
            let _ = done_rx.recv();
            publish_pinned_jit_roots(&[]);
        });
        ready_rx.recv().expect("the peer published");
        let (own, others) = pinned_jit_roots_snapshot_split();
        assert_eq!(own, vec![MINE], "only the caller's entry is its own");
        assert!(
            others.contains(&PEER),
            "a peer's word is never the caller's"
        );
        assert!(!others.contains(&MINE));
        let all = pinned_jit_roots_snapshot();
        assert!(
            all.contains(&MINE) && all.contains(&PEER),
            "the union is the snapshot"
        );
        done_tx.send(()).expect("done");
        peer.join().expect("peer");
        publish_pinned_jit_roots(&[]);
    }

    /// gc-common w2-c: the one take-over licence, as a truth table.
    #[test]
    fn takeover_licence_truth_table() {
        let none = TakeoverVerdict::NONE;
        assert_eq!(none.licence(false), MoveLicence::Move);
        assert_eq!(none.licence(true), MoveLicence::Move);

        let frozen_ok = TakeoverVerdict {
            frozen: 1,
            ..TakeoverVerdict::NONE
        };
        assert_eq!(frozen_ok.licence(true), MoveLicence::MovePinned);
        assert_eq!(
            frozen_ok.licence(false),
            MoveLicence::NonMoving,
            "a backend that cannot pin must not move under a frozen peer"
        );

        let exact_base_only = TakeoverVerdict {
            derived_pointers_resolved: false,
            ..frozen_ok
        };
        assert_eq!(
            exact_base_only.licence(true),
            MoveLicence::NonMoving,
            "an exact-base pin set leaves a cursor's array unpinned"
        );

        let partial = TakeoverVerdict {
            helper_windows: 2,
            frozen: 0,
            pins_complete: false,
            ..TakeoverVerdict::NONE
        };
        assert_eq!(partial.licence(true), MoveLicence::NonMoving);

        let unread = TakeoverVerdict {
            unreadable: 1,
            ..TakeoverVerdict::NONE
        };
        assert_eq!(
            unread.licence(true),
            MoveLicence::NonMoving,
            "a peer whose roots were never seen voids any pin set"
        );
    }

    /// gc-common w3-g: G1's consumer of the licence refuses exactly the
    /// `NonMoving` pauses, and only with the lever on.
    #[test]
    fn g1_licence_refusal_follows_the_licence_and_the_lever() {
        let unread = TakeoverVerdict {
            unreadable: 1,
            ..TakeoverVerdict::NONE
        };
        assert_eq!(
            g1_licence_refusal(unread, true),
            Some(incomplete_reason::XT_TAKEOVER)
        );
        assert_eq!(g1_licence_refusal(unread, false), None, "opt-in");
        let pinned = TakeoverVerdict {
            frozen: 1,
            ..TakeoverVerdict::NONE
        };
        assert_eq!(
            g1_licence_refusal(pinned, true),
            None,
            "a complete, derived-resolving pin set is honoured by the region pins"
        );
        assert_eq!(g1_licence_refusal(TakeoverVerdict::NONE, true), None);
    }

    /// gc-common w3-g: a pause that opens without running a take-over must not
    /// inherit the previous pause's verdict -- the collectors now read it.
    ///
    /// The marker verdict is deliberately one whose licence is still `Move`
    /// (nothing frozen, nobody unread): the global is read by the Generational
    /// collector, and a concurrently running collector test must not see a
    /// take-over it never had.
    #[test]
    fn opening_a_pause_clears_the_takeover_verdict() {
        let marker = TakeoverVerdict {
            pins_complete: false,
            ..TakeoverVerdict::NONE
        };
        assert_eq!(marker.licence(false), MoveLicence::Move);
        publish_takeover_verdict(marker);
        reset_peer_proven_jit_depth();
        assert_eq!(takeover_verdict(), TakeoverVerdict::NONE);
        assert!(!takeover_forbids_unpinnable_move());
    }

    /// gc-common w2-c: take-over captures stop at half the buffer, so a deep
    /// frozen stack (captured first in every cycle) can never crowd out the
    /// helper-window captures the blocked-wake remap depends on. Exercised on a
    /// local buffer: the real one is process-global and other tests use it.
    #[test]
    fn takeover_captures_leave_half_the_buffer_to_the_repair() {
        let mut buf = Vec::new();
        let mut admitted = 0usize;
        for i in 0..PEER_STACK_SLOTS_CAP {
            if push_capped(&mut buf, (1, i * 8, i), TAKEOVER_STACK_SLOTS_CAP) {
                admitted += 1;
            }
        }
        assert_eq!(admitted, TAKEOVER_STACK_SLOTS_CAP);
        assert!(TAKEOVER_STACK_SLOTS_CAP < PEER_STACK_SLOTS_CAP);
        // The helper-window capture after a saturating take-over still lands.
        assert!(push_capped(
            &mut buf,
            (2, 0x1000, 0x2000),
            PEER_STACK_SLOTS_CAP
        ));
        assert_eq!(buf.last(), Some(&(2, 0x1000, 0x2000)));
        // And the hard cap still holds for everyone.
        while push_capped(&mut buf, (2, 0, 0), PEER_STACK_SLOTS_CAP) {}
        assert_eq!(buf.len(), PEER_STACK_SLOTS_CAP);
    }

    /// gc-common 2026-09-23 (lane A) — the empty-publication fast path must
    /// never leave a stale entry behind: every transition a thread can make
    /// through the three writers (`publish` non-empty / empty, `add`,
    /// `clear`) has to be visible in the union the collectors read.
    ///
    /// Runs on its own thread so the thread-local "may own" hint starts from
    /// its initial state, and uses addresses no other test publishes (the map
    /// is process-global and other tests write to it concurrently).
    #[test]
    fn empty_publication_fast_path_never_strands_an_entry() {
        std::thread::spawn(|| {
            const A: usize = 0x7EAD_0000_1000;
            const B: usize = 0x7EAD_0000_2000;
            let has = |a: usize| pinned_jit_roots_snapshot().contains(&a);

            // Nothing owned: the empty publication takes the fast path.
            publish_pinned_jit_roots(&[]);
            assert!(!has(A) && !has(B));

            publish_pinned_jit_roots(&[A]);
            assert!(has(A), "a non-empty publication must be visible");
            publish_pinned_jit_roots(&[]);
            assert!(!has(A), "an empty publication must withdraw the entry");

            add_pinned_jit_root(B);
            assert!(has(B), "add_pinned_jit_root must be visible");
            publish_pinned_jit_roots(&[]);
            assert!(!has(B), "an empty publication after `add` must withdraw it");

            publish_pinned_jit_roots(&[A, B]);
            clear_pinned_jit_roots();
            assert!(!has(A) && !has(B), "clear must withdraw the entry");
            publish_pinned_jit_roots(&[]);
            assert!(!has(A) && !has(B));

            add_pinned_jit_root(A);
            clear_pinned_jit_roots();
            add_pinned_jit_root(B);
            assert!(has(B) && !has(A));
            publish_pinned_jit_roots(&[]);
            assert!(!has(B));
        })
        .join()
        .expect("fast-path test thread panicked");
    }

    /// Provenance is wanted only between a collection's `clear_pinned_jit_roots`
    /// and the same thread's next publication. Per thread, so a fresh thread
    /// isolates the test from every other one.
    #[test]
    fn provenance_is_wanted_only_during_the_collections_own_scan() {
        std::thread::spawn(|| {
            assert!(!jit_root_provenance_wanted(), "a thread starts unarmed");
            // A deposit or a native call's snapshot: never armed.
            publish_pinned_jit_roots(&[]);
            assert!(!jit_root_provenance_wanted());
            // The collection's root gatherer.
            clear_pinned_jit_roots();
            assert!(
                jit_root_provenance_wanted(),
                "the collection's scan records"
            );
            publish_pinned_jit_roots(&[]);
            assert!(
                !jit_root_provenance_wanted(),
                "its publication ends the scan"
            );
        })
        .join()
        .expect("provenance arming test thread panicked");
    }

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
        assert_eq!(
            was_vacated(0x2100),
            None,
            "chunk interior must be forgotten"
        );
        assert_eq!(
            was_vacated_on(0x3000),
            Some((0xb000, 11)),
            "an address OUTSIDE the chunk must survive -- the purge is a range, not a clear"
        );

        reset_vacated_ledger_for_test();
        set_vacated_frames_enabled_for_test(false);
    }

    /// gen r5w6/pin10: the range purge on the ordered ledger is half-open,
    /// `[lo, hi)`, exactly as the `retain` it replaced: `lo` goes, `hi` and
    /// `lo - 8` stay.
    #[test]
    fn vacated_ledger_range_purge_is_half_open() {
        let _g = VACATED_TEST_LOCK.lock();
        set_vacated_frames_enabled_for_test(true);
        reset_vacated_ledger_for_test();

        record_vacated(
            &pointer_map_of(&[
                (0x6ff8, 0xe000),
                (0x7000, 0xe100),
                (0x7ff8, 0xe200),
                (0x8000, 0xe300),
            ]),
            5,
        );
        note_allocated_range(0x7000, 0x8000);

        assert_eq!(was_vacated_on(0x6ff8), Some((0xe000, 5)), "below the range");
        assert_eq!(was_vacated(0x7000), None, "lo is inside");
        assert_eq!(was_vacated(0x7ff8), None, "the last word inside");
        assert_eq!(was_vacated_on(0x8000), Some((0xe300, 5)), "hi is outside");

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

    /// gci i3/tk (X2 of `gci-i1jn-takeover-...`): the pinned door records the
    /// reason and the incomplete verdict but never the un-rewritable bit; the
    /// peer-state reason stays visible to `peer_state_reason_recorded`, and
    /// the `_because` door is unchanged.
    #[test]
    fn gci_i3tk_the_pinned_door_keeps_the_reason_and_not_the_unrewritable_bit() {
        for reason in [
            incomplete_reason::XT_TAKEOVER,
            incomplete_reason::XT_HELPER_WINDOW,
        ] {
            begin_moving_young_coverage_cycle();
            assert!(!peer_state_reason_recorded());
            mark_moving_young_coverage_incomplete_pinned_because(reason);
            assert!(moving_young_coverage_incomplete(), "no Cheney copy");
            assert_eq!(moving_young_incomplete_reason(), reason);
            assert_eq!(moving_young_incomplete_reason_mask(), 1usize << reason);
            assert!(!unrewritable_peer_state(), "selective promotion may run");
            assert!(peer_state_reason_recorded());
            begin_moving_young_coverage_cycle();
            mark_moving_young_coverage_incomplete_because(reason);
            assert!(unrewritable_peer_state(), "the old door still gates promotion");
            assert!(peer_state_reason_recorded());
        }
        begin_moving_young_coverage_cycle();
        mark_moving_young_coverage_incomplete_because(incomplete_reason::UNPUBLISHED_FRAME_OOP);
        assert!(!peer_state_reason_recorded(), "a frame reason is not peer state");
        begin_moving_young_coverage_cycle();
        assert!(!peer_state_reason_recorded());
    }

    /// The discriminator `gen_heap` and `zgc` branch on, and the mixed unit its
    /// name used to deny.
    ///
    /// Two separate properties are pinned here because two separate defects
    /// live in them:
    ///
    /// 1. **An EMPTY publication still counts.** "This thread looked and found
    ///    nothing" and "this thread never looked" are opposite conclusions for
    ///    a pin-by-value consumer, and the pin set itself cannot hold the
    ///    difference. If this regressed, a collector would read an empty set as
    ///    a licence to relocate everything.
    /// 2. **The two publication paths carry different units.**
    ///    `publish_pinned_jit_roots` bumps once per CALL, `add_pinned_jit_root`
    ///    once per ADDRESS. The accessor was documented as "how many THREADS
    ///    have published", which is true of neither path taken together. Only
    ///    non-zero-ness is well defined, and that is what both consumers use.
    #[test]
    fn conservative_jit_scan_count_is_per_publication_not_per_thread() {
        begin_moving_young_coverage_cycle();
        assert_eq!(
            conservative_jit_scans(),
            0,
            "a freshly opened coverage cycle has no publications",
        );

        publish_pinned_jit_roots(&[]);
        assert_eq!(
            conservative_jit_scans(),
            1,
            "a publication of an EMPTY vector is still a look, and the pin set \
             cannot record that it happened",
        );

        publish_pinned_jit_roots(&[0x1000, 0x2000, 0x3000]);
        assert_eq!(
            conservative_jit_scans(),
            2,
            "publish_pinned_jit_roots counts CALLS, whatever the slice length",
        );

        add_pinned_jit_root(0x4000);
        add_pinned_jit_root(0x5000);
        assert_eq!(
            conservative_jit_scans(),
            4,
            "add_pinned_jit_root counts ADDRESSES — the magnitude mixes two \
             units and may not be read as a thread or scan count",
        );

        // Per-CYCLE: the count describes the pause being opened, not the run.
        begin_moving_young_coverage_cycle();
        assert_eq!(conservative_jit_scans(), 0);

        clear_pinned_jit_roots();
    }

    /// CHARACTERIZATION TEST of the FUNCTIONS: `CONSERVATIVE_JIT_SCANS` is
    /// cleared only by [`begin_moving_young_coverage_cycle`], not by
    /// [`reset_peer_proven_jit_depth`], so an open that runs AFTER a peer's
    /// deposit discards the peer's "I looked".
    ///
    /// That used to be the production order -- the initiator opened its cycle
    /// during root gathering, after its peers had parked -- and it was a hole:
    /// with `g1_only_jit_pins()` on a non-G1 heap the initiator publishes
    /// nothing, the count read zero while peers held conservative roots, and
    /// `unrewritable_conservative_jit_roots` went false
    /// (`docs/feature-designs/generative-gc-architecture.md`).
    ///
    /// It is no longer the production order (re-audited gc-common w8-d): since
    /// w2-a the open IS the winning request's `open_cycle`
    /// (`GcBarrier::request_stw_opening_cycle`), under the barrier lock and
    /// before `stw_requested` is visible, so no peer of the pause can have
    /// deposited yet. The test stays because the function-level fact is what
    /// makes that ordering load-bearing: a door that opened its cycle late
    /// again would reopen the hole, and ZGC refuses on `== 0` (`zgc.rs`).
    #[test]
    fn conservative_jit_scans_are_erased_by_the_coverage_cycle_open() {
        begin_moving_young_coverage_cycle();

        // Stand in for a peer that parked on `stw_requested` and deposited.
        publish_pinned_jit_roots(&[0x1000]);
        assert_eq!(conservative_jit_scans(), 1, "the peer's look was recorded");

        // Clearing the sibling ledgers — what `request_stw` does — leaves this
        // one alone. Stated because adding the clear THERE looks like the fix.
        reset_peer_proven_jit_depth();
        assert_eq!(
            conservative_jit_scans(),
            1,
            "reset_peer_proven_jit_depth does not touch this counter; adding it \
             there would not be sufficient either, because the initiator's \
             begin_moving_young_coverage_cycle still runs later",
        );

        // An open AFTER the deposit discards it -- which is why every door
        // opens its cycle at the request, before any peer can deposit.
        begin_moving_young_coverage_cycle();
        assert_eq!(
            conservative_jit_scans(),
            0,
            "an open after a deposit discards it; see this test's doc comment \
             before changing this expectation",
        );

        clear_pinned_jit_roots();
    }

    /// gen r4w3/young2: `note_conservative_jit_scan` is the counter half of a
    /// publication and nothing else — it bumps exactly as an empty
    /// `publish_pinned_jit_roots` does and leaves every published pin alone.
    #[test]
    fn noting_a_scan_bumps_the_count_and_leaves_the_pins_alone() {
        begin_moving_young_coverage_cycle();
        clear_pinned_jit_roots();
        publish_pinned_jit_roots(&[0x2000]);
        assert_eq!(conservative_jit_scans(), 1);

        note_conservative_jit_scan();
        assert_eq!(conservative_jit_scans(), 2, "one bump per noted scan");
        assert!(
            pinned_jit_roots_snapshot().contains(&0x2000),
            "noting a scan must not drop a pin this thread already published",
        );

        begin_moving_young_coverage_cycle();
        assert_eq!(conservative_jit_scans(), 0, "still per-cycle");
        clear_pinned_jit_roots();
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
///
/// gen r5w6/pin10: the vacated map is ORDERED (`BTreeMap`, was an
/// `FxHashMap`). [`note_allocated_range`] runs on every TLAB refill while the
/// ledger is armed, and on a hash map its only form was a `retain` over EVERY
/// entry -- O(ledger) per refill, with a ledger of hundreds of thousands of
/// addresses after a few relocating cycles. An armed run slowed enough to lose
/// the timing-sensitive failures it was armed to catch. A range removal is
/// O(log n + removed) here; the per-key operations stay O(log n).
type VacatedLedger = (
    std::collections::BTreeMap<usize, (usize, u64)>,
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
    let on = cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_VACATED_FRAMES");
    state.store(if on { 2 } else { 1 }, std::sync::atomic::Ordering::Relaxed);
    on
}

// TWO doc blocks were concatenated here and attached to `RELOCATING_CYCLES`
// alone (fixed 2026-09-20). The first is [`record_vacated`]'s, which sits ~30
// lines down and was undocumented as a result — and it was ALSO stale: it
// opened "Replace the ledger with THIS collection's vacated addresses ... One
// collection at a time, deliberately", while the function's own inline comment
// says "ACCUMULATE across collections rather than replace" and explains why the
// replace semantics were abandoned. Two contradictory accounts of the same ten
// lines, one of them rendered as the documentation of a different item.
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

/// Fold THIS collection's vacated addresses (the pointer map's keys) into the
/// ledger, dated by `collection`.
///
/// **Accumulating, not replacing** — see the inline comment below for why the
/// original one-collection-at-a-time semantics were wrong, and
/// [`was_vacated_on`] for how a consumer dates an entry. The ledger stays
/// bounded because [`note_allocated`] / [`note_allocated_range`] drop an entry
/// the moment the allocator re-issues the address.
///
/// **It is only exact because of those two.** A compacting cycle zeroes what it
/// vacated and hands it straight back, so a reference to a vacated address is
/// ambiguous the moment a NEW object is allocated there — and the first version
/// of this instrument, which did not track that, reported eight perfectly valid
/// frame slots per run (`MVTable.updateRows` local[4] holding exactly the
/// `SessionLocal$Savepoint` its `astore 4` had put there). With re-issued
/// addresses removed, a hit is unambiguous: nothing has been allocated at that
/// address since the collector moved its occupant away.
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
    // gen r5w6/pin10: the bound held only while one slide moved fewer than
    // `MOVED_HISTORY_MAX` objects; a larger one refilled the cleared map past
    // it. Keep the slide's LAST pairs, the newest history.
    let skip = pairs.len().saturating_sub(MOVED_HISTORY_MAX);
    for (from, to, class_at_to) in &pairs[skip..] {
        map.insert(*from, (*to, *class_at_to));
    }
}

/// Did the collector ever move an object AWAY from `addr`, and where to?
///
/// The raw ledger read behind [`stale_use_verdict`], without that function's
/// "and the space has since been re-issued under a different class" screen.
/// Two very different defects produce a dangling reference and this is what
/// separates them: `Some` means the referent MOVED and something failed to
/// rewrite the reference; `None` means it was never relocated, so it was
/// reclaimed while still referenced — or the reference was never right.
///
/// Requires `CRATONVM_DBG_VACATED_FRAMES`; `None` when the ledger is off, which
/// a caller must not read as "was never moved".
pub fn moved_away_to(addr: usize) -> Option<(usize, u32)> {
    if !vacated_frames_enabled() || addr == 0 || addr % 8 != 0 {
        return None;
    }
    let g = MOVED_HISTORY.read();
    g.as_ref()?.get(&addr).copied()
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
        "a STALE reference is being USED: the collector moved this object to `moved_to`, the \
         allocator has since re-issued the address, and the object now there is of a different \
         class. The backtrace names the VM code still holding it."
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
/// `docs/feature-designs/default-moving-young-gen.md`.
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
        "a heap access RECEIVER is an address the collector moved an object away from — \
         every field read through it returns whatever now occupies that memory. The \
         backtrace names the VM code holding it."
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
/// `docs/feature-designs/default-moving-young-gen.md`:
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
    // gen r5w6/pin10: a range removal on the ordered map (see
    // `VacatedLedger`), not a `retain` over every entry.
    let doomed: Vec<usize> = from.range(lo..hi).map(|(&k, _)| k).collect();
    for k in doomed {
        from.remove(&k);
    }
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

fn per_tid_jit_depth() -> &'static std::sync::RwLock<
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
    // Poison-recovering (gci i5/tk). A detached slot would read as "never
    // published", which the default-ON never-published rule
    // (`CRATONVM_XT_HELPER_WINDOW_UNPUBLISHED_NATIVE`) takes as "no compiled
    // frame" -- no longer the safe direction. The map's data is consistent
    // (insert / remove do not panic short of an allocation abort).
    let mut map = per_tid_jit_depth().write().unwrap_or_else(|p| p.into_inner());
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

fn per_tid_shadow_addr(
) -> &'static std::sync::RwLock<std::collections::HashMap<u32, (usize, usize, usize)>> {
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
    // Poison-recovering (gci i5/tk): a dropped publication reads as "never
    // published" (see `register_self_jit_depth_slot`).
    per_tid_shadow_addr()
        .write()
        .unwrap_or_else(|p| p.into_inner())
        .insert(os_tid, (addr, base, end));
}

/// The `(addr, base, end)` triple `os_tid` published, if any.
pub fn shadow_window_of_tid(os_tid: u32) -> Option<(usize, usize, usize)> {
    let map = per_tid_shadow_addr().read().unwrap_or_else(|p| p.into_inner());
    map.get(&os_tid).copied()
}

/// Withdraw every published triple whose backing buffer is `[base, end)`.
/// Called by `ShadowStack`'s `Drop`, just before the buffer and the struct
/// that holds it are freed.
///
/// gci i47w lane oe48
/// (`gci-i47w-oe48-g1-helper-window-reads-a-freed-shadow-stack-FIXED-20261017`): a
/// triple is keyed by the OS tid, but it describes the `JvmThread` that last
/// entered compiled code on that OS thread. It was removed only when the OS
/// thread exited (`unregister_jit_depth_slot`), so an OS thread that outlives
/// the `JvmThread` it published (a continuation runner whose virtual thread
/// ended, a carrier) kept a triple naming freed memory. The helper-window pass
/// reads the struct BEFORE the identity check can refuse it
/// (`xt_root_scan::read_peer_shadow_window_ex`), so once the allocator gave
/// those pages back to the OS the read faulted (i48e `jp24_s_churn_def_r2`:
/// `EXCEPTION_ACCESS_VIOLATION` in `scan_peer_shadow_window` under
/// `helper_window_pass`).
///
/// Matching on `(base, end)` rather than on the struct address also covers a
/// struct MOVED after it published: the buffer moves with it, so its final
/// drop withdraws the stale address too. The owning thread's once-per-pair
/// cache (`conservative_roots::publish_self_shadow_addr_once`) is not touched:
/// a later `JvmThread` that reuses both the struct address and the buffer
/// stays unpublished until it enters compiled code with another pair, which a
/// reader treats as UNKNOWN (untrusted) -- the safe direction.
pub fn retract_shadow_window_of_buffer(base: usize, end: usize) {
    if base == 0 {
        return;
    }
    // Read first: almost every drop finds nothing (most `JvmThread`s never
    // entered compiled code, or their OS thread has republished since).
    let any = per_tid_shadow_addr()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .values()
        .any(|&(_, b, e)| b == base && e == end);
    if !any {
        return;
    }
    per_tid_shadow_addr()
        .write()
        .unwrap_or_else(|p| p.into_inner())
        .retain(|_, triple| !(triple.1 == base && triple.2 == end));
}

/// Drop `os_tid`'s slot when its owning thread exits.
///
/// Without this a dead thread's last depth stays readable under a tid the OS
/// will hand to someone else, and a recycled thread that never enters JIT never
/// overwrites it -- so the initiator would credit phantom depth for a peer with
/// no compiled frames at all.
pub fn unregister_jit_depth_slot(os_tid: u32) {
    // Poison-recovering, like the readers and writers around it (gci i5/tk:
    // a reader degrading to `None` is no longer the safe direction once the
    // never-published rule reads `None` + `None` as "no compiled frame").
    // A failed REMOVAL is the opposite: it leaves this thread's
    // last depth readable under a tid the OS will hand to someone else, and
    // `add_xt_cycle_pinned_jit_depth` then credits frames nobody holds, which
    // lets the collector move an object an unscanned frame still names. Same
    // argument as `recover` on the pin registry.
    per_tid_jit_depth()
        .write()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&os_tid);
    // The shadow address dies with the thread too: its `JvmThread` (and the
    // `Box` the window points into) goes with it, so a leftover entry is a
    // dangling pointer under a tid the OS will recycle.
    per_tid_shadow_addr()
        .write()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&os_tid);
}

/// The JIT depth `os_tid` last published, or `None` if it never registered.
///
/// `None` and `Some(0)` are NOT interchangeable for the caller: a thread that
/// never registered has an UNKNOWN depth, and crediting zero for it would claim
/// its frames are accounted for. See [`add_xt_cycle_pinned_jit_depth`].
pub fn jit_depth_of_tid(os_tid: u32) -> Option<usize> {
    let map = per_tid_jit_depth().read().unwrap_or_else(|p| p.into_inner());
    map.get(&os_tid)
        .map(|slot| slot.load(std::sync::atomic::Ordering::Acquire))
}

// Peer JIT depth this cycle discharged by PINNING rather than by proof, and
// whether every frozen peer could be attributed a published depth.
//
// One peer whose depth is unknown poisons the whole ledger: the accounting is
// a single process-wide subtraction, so it cannot say "credit these threads
// and keep refusing for that one".
//
// In the calling thread's `PauseLedger` since gc-common w8-d (the
// `XT_CYCLE_PINNED_JIT_DEPTH` / `XT_CYCLE_PINNED_DEPTH_EXACT` statics until
// then): the writer (`helper_window_pass`) and the reader (the initiator's
// `refresh_moving_young_coverage_for_collection`) both run on the
// initiator, which is bound to its own VM's ledger for the whole pause. An
// unbound thread (a unit test) gets its own thread-local ledger -- the
// isolation the `cfg(test)` thread-locals used to give, now in every build.

/// Credit `depth` JIT entries belonging to a peer whose ENTIRE stack this cycle
/// pinned.
///
/// `depth == None` means the peer froze without ever having registered a slot,
/// so its depth is unknown; that marks the ledger inexact and
/// [`xt_cycle_pinned_jit_depth`] then refuses to credit anything at all.
pub fn add_xt_cycle_pinned_jit_depth(depth: Option<usize>) {
    let _ = with_ledger(|l| match depth {
        Some(d) => {
            l.pinned_depth.fetch_add(d, Ordering::AcqRel);
        }
        None => l.pinned_exact.store(false, Ordering::Release),
    });
}

/// Depth discharged by pinning this cycle, or 0 when the ledger is inexact.
pub fn xt_cycle_pinned_jit_depth() -> usize {
    with_ledger(|l| {
        if l.pinned_exact.load(Ordering::Acquire) {
            l.pinned_depth.load(Ordering::Acquire)
        } else {
            0
        }
    })
    .unwrap_or(0)
}

/// Reset the pinned-depth ledger. Shares the lifecycle of
/// [`clear_xt_cycle_pinned_jit_roots`] -- the pins and the depth they discharge
/// must appear and disappear together.
pub fn clear_xt_cycle_pinned_jit_depth() {
    let _ = with_ledger(PauseLedger::clear_pinned);
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

    /// gci i3tk item 5 / i5tk X1: a POISONED per-tid map must not read as
    /// "never published". Since the never-published rule
    /// (`CRATONVM_XT_HELPER_WINDOW_UNPUBLISHED_NATIVE`) is default ON, a
    /// `None` + `None` answer is taken as "no compiled frame", so every writer
    /// and reader recovers the poison instead of degrading.
    ///
    /// Poisoning the process-wide maps is safe for the rest of the suite only
    /// because EVERY site recovers now (this test pins that): a poisoned lock
    /// stays poisoned for the life of the test binary.
    #[test]
    fn gci_i13tq_a_poisoned_per_tid_map_still_publishes_and_reads() {
        // Poison both maps: a thread panics while holding each write guard.
        let r = std::thread::spawn(|| {
            let _g = per_tid_jit_depth().write().unwrap_or_else(|p| p.into_inner());
            panic!("gci i13/tq: deliberate poison of the per-tid depth map");
        })
        .join();
        assert!(r.is_err(), "the poisoning thread must have panicked");
        let r = std::thread::spawn(|| {
            let _g = per_tid_shadow_addr().write().unwrap_or_else(|p| p.into_inner());
            panic!("gci i13/tq: deliberate poison of the per-tid shadow map");
        })
        .join();
        assert!(r.is_err(), "the poisoning thread must have panicked");
        assert!(per_tid_jit_depth().is_poisoned());
        assert!(per_tid_shadow_addr().is_poisoned());

        let tid = 0xFEED_1301;
        // A writer under poison still lands in the shared map (not a detached slot).
        let slot = register_self_jit_depth_slot(tid);
        slot.store(5, Ordering::Release);
        assert_eq!(
            jit_depth_of_tid(tid),
            Some(5),
            "a poisoned depth map must not read as never published"
        );
        assert!(
            std::sync::Arc::ptr_eq(&slot, &register_self_jit_depth_slot(tid)),
            "a poisoned map must still hand back the registered cell"
        );
        publish_self_shadow_addr(tid, 0x1000, 0x2000, 0x3000);
        assert_eq!(
            shadow_window_of_tid(tid),
            Some((0x1000, 0x2000, 0x3000)),
            "a poisoned shadow map must not read as never published"
        );
        // And removal still removes.
        unregister_jit_depth_slot(tid);
        assert_eq!(jit_depth_of_tid(tid), None);
        assert_eq!(shadow_window_of_tid(tid), None);
    }

    /// gci i47w lane oe48: retracting a buffer withdraws exactly the triples
    /// published for that `(base, end)`, under whichever tids published it,
    /// and leaves every other window alone; `base == 0` is a no-op.
    #[test]
    fn gci_i47w_oe48_retracting_a_buffer_withdraws_only_its_windows() {
        let (a, b, c) = (0xFEED_4E11u32, 0xFEED_4E12u32, 0xFEED_4E13u32);
        publish_self_shadow_addr(a, 0x7_1000, 0x7_2000, 0x7_3000);
        // The same buffer published under a second tid (a runner that ran it
        // before the first one did): both go.
        publish_self_shadow_addr(b, 0x7_1000, 0x7_2000, 0x7_3000);
        publish_self_shadow_addr(c, 0x8_1000, 0x8_2000, 0x8_3000);
        retract_shadow_window_of_buffer(0, 0);
        let untouched = shadow_window_of_tid(a);
        // Same base, another end: not this buffer.
        retract_shadow_window_of_buffer(0x7_2000, 0x7_4000);
        let other_end = shadow_window_of_tid(a);
        retract_shadow_window_of_buffer(0x7_2000, 0x7_3000);
        let (ra, rb, rc) = (
            shadow_window_of_tid(a),
            shadow_window_of_tid(b),
            shadow_window_of_tid(c),
        );
        for t in [a, b, c] {
            unregister_jit_depth_slot(t);
        }
        assert_eq!(untouched, Some((0x7_1000, 0x7_2000, 0x7_3000)));
        assert_eq!(other_end, Some((0x7_1000, 0x7_2000, 0x7_3000)));
        assert_eq!(ra, None);
        assert_eq!(rb, None);
        assert_eq!(rc, Some((0x8_1000, 0x8_2000, 0x8_3000)));
    }
}

/// gc-common w8-d: the per-VM [`PauseLedger`] and its thread-local route.
#[cfg(test)]
mod pause_ledger_tests {
    use super::*;
    use std::sync::Arc;

    /// A peer's deposit lands in the ledger of ITS VM's pause, and an
    /// initiator reads only its own VM's ledger -- the property the
    /// process-static ledger could not have: VM B's peers can no longer
    /// inflate VM A's proven depth, whatever the coverage slot did.
    #[test]
    fn a_deposit_is_credited_to_its_own_vms_pause_only() {
        let a = Arc::new(PauseLedger::new());
        let b = Arc::new(PauseLedger::new());
        // A's initiator: bound for its pause, which opens its ledger.
        bind_pause_ledger(&a);
        reset_peer_proven_jit_depth();
        // A peer of VM B parks and deposits (another thread, B's binding).
        let b2 = Arc::clone(&b);
        std::thread::spawn(move || with_pause_ledger(&b2, || add_peer_proven_jit_depth(5)))
            .join()
            .expect("B's peer");
        assert_eq!(b.proven_jit_depth(), 5, "B's deposit is B's");
        assert_eq!(
            peer_proven_jit_depth(),
            0,
            "A's initiator must not be credited with B's peer"
        );
        // A peer of VM A deposits on its own thread.
        let a2 = Arc::clone(&a);
        std::thread::spawn(move || with_pause_ledger(&a2, || add_peer_proven_jit_depth(3)))
            .join()
            .expect("A's peer");
        assert_eq!(peer_proven_jit_depth(), 3);
        // B's next request opens B's ledger only.
        let b3 = Arc::clone(&b);
        std::thread::spawn(move || {
            bind_pause_ledger(&b3);
            reset_peer_proven_jit_depth();
            unbind_pause_ledger(&b3);
        })
        .join()
        .expect("B's initiator");
        assert_eq!(b.proven_jit_depth(), 0);
        assert_eq!(
            peer_proven_jit_depth(),
            3,
            "B's reset must not erase A's ledger"
        );
        unbind_pause_ledger(&a);
        assert!(!pause_ledger_bound(&a));
        assert_eq!(
            peer_proven_jit_depth(),
            0,
            "an unbound thread reads its own fallback"
        );
    }

    /// `with_pause_ledger` restores the previous binding, also on unwind, and
    /// `unbind_pause_ledger` removes only the ledger it names.
    #[test]
    fn a_scoped_binding_restores_the_previous_one() {
        let a = Arc::new(PauseLedger::new());
        let b = Arc::new(PauseLedger::new());
        bind_pause_ledger(&a);
        with_pause_ledger(&b, || {
            assert!(pause_ledger_bound(&b));
            add_peer_proven_jit_depth(2);
        });
        assert!(pause_ledger_bound(&a), "the initiator's binding is back");
        assert_eq!(b.proven_jit_depth(), 2);
        assert_eq!(a.proven_jit_depth(), 0);
        fn unwinding_deposit() {
            panic!("a deposit that unwinds");
        }
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_pause_ledger(&b, unwinding_deposit)
        }));
        assert!(unwound.is_err());
        assert!(pause_ledger_bound(&a), "restored on unwind too");
        unbind_pause_ledger(&b);
        assert!(
            pause_ledger_bound(&a),
            "unbinding another VM's ledger is a no-op"
        );
        unbind_pause_ledger(&a);
        assert!(!pause_ledger_bound(&a));
    }

    /// gc-common w18-c: the guard form restores the previous binding when it
    /// drops, and nests like `with_pause_ledger`.
    #[test]
    fn a_scoped_ledger_guard_restores_the_previous_binding() {
        let a = Arc::new(PauseLedger::new());
        let b = Arc::new(PauseLedger::new());
        bind_pause_ledger(&a);
        {
            let _scope = scoped_pause_ledger(&b);
            assert!(pause_ledger_bound(&b));
            add_peer_proven_jit_depth(3);
            {
                let _inner = scoped_pause_ledger(&a);
                assert!(pause_ledger_bound(&a));
            }
            assert!(
                pause_ledger_bound(&b),
                "the inner guard restores the outer one"
            );
        }
        assert!(pause_ledger_bound(&a), "the initiator's binding is back");
        assert_eq!((a.proven_jit_depth(), b.proven_jit_depth()), (0, 3));
        unbind_pause_ledger(&a);
        {
            let _scope = scoped_pause_ledger(&b);
            assert!(pause_ledger_bound(&b));
        }
        assert!(
            !pause_ledger_bound(&b),
            "an unbound thread goes back to unbound"
        );
    }

    /// gc-common w18-c: the peer native-stack captures and the register
    /// pairing capture (`PEER_STACK_SLOTS` / `PEER_REG_CAPTURE` process statics
    /// until then) are the bound ledger's. VM B's pause opening and B's
    /// captures leave VM A's alone; A's drain returns A's words only; an
    /// unbound thread drains its own empty fallback; opening A's next pause
    /// discards what A never folded.
    #[test]
    fn the_peer_captures_are_the_bound_vms_only() {
        let a = Arc::new(PauseLedger::new());
        let b = Arc::new(PauseLedger::new());
        let remap_on = blocked_peer_stack_remap_enabled();
        with_pause_ledger(&a, || {
            record_peer_stack_slot(0xA, 0x5a00_0000, 0x5a00_1000);
            record_takeover_stack_slot(0xA, 0x5a00_0008, 0x5a00_2000);
            // The pairing capture fills only under its debug flag; seed it
            // directly so the routing is tested either way.
            a.reg_capture
                .lock()
                .push((0xA, PEER_REG_STACK_HELPER, 0x5a00_3000));
        });
        let b2 = Arc::clone(&b);
        std::thread::spawn(move || {
            with_pause_ledger(&b2, || {
                // B's request opens B's pause; B's opener clears B's captures.
                b2.open();
                clear_peer_stack_slots();
                clear_peer_reg_capture();
                record_peer_stack_slot(0xB, 0x5b00_0000, 0x5b00_1000);
                assert!(
                    take_peer_reg_capture().is_empty(),
                    "A's pairing words are not B's"
                );
            });
        })
        .join()
        .expect("B's initiator");
        let expected_a: Vec<(u32, usize, usize)> = if remap_on {
            vec![
                (0xA, 0x5a00_0000, 0x5a00_1000),
                (0xA, 0x5a00_0008, 0x5a00_2000),
            ]
        } else {
            Vec::new()
        };
        with_pause_ledger(&a, || {
            let mut values = Vec::new();
            peer_stack_slot_values_into(&mut values, |v| v != 0x5a00_2000);
            let want: Vec<usize> = if remap_on {
                vec![0x5a00_1000]
            } else {
                Vec::new()
            };
            assert_eq!(values, want, "the filter applies, over A's words only");
            assert!(!peer_stack_slots_saturated());
            assert_eq!(
                take_peer_stack_slots(),
                expected_a,
                "B's pause neither cleared nor joined A's"
            );
            assert!(
                take_peer_stack_slots().is_empty(),
                "a drain empties the buffer"
            );
            assert_eq!(
                take_peer_reg_capture(),
                vec![(0xA, PEER_REG_STACK_HELPER, 0x5a00_3000)]
            );
        });
        assert_eq!(
            b.stack_slots.lock().len(),
            usize::from(remap_on),
            "B's capture is still B's to fold"
        );
        assert!(
            std::thread::spawn(take_peer_stack_slots)
                .join()
                .expect("unbound drain")
                .is_empty(),
            "an unbound thread drains its own empty fallback"
        );
        // A capture a pause never folded is discarded when the next one opens.
        with_pause_ledger(&a, || record_peer_stack_slot(0xA, 0x5a00_0010, 0x5a00_4000));
        a.open();
        assert!(
            a.stack_slots.lock().is_empty(),
            "a pause opens with no captures"
        );
    }

    /// Opening a pause zeroes every row of the bound ledger, including the
    /// take-over times -- which only a take-over (`reset_xt_cycle`) used to
    /// zero, so a pause without one reported the previous take-over's times.
    #[test]
    fn opening_a_pause_zeroes_the_whole_ledger() {
        let l = Arc::new(PauseLedger::new());
        with_pause_ledger(&l, || {
            add_peer_proven_jit_depth(4);
            add_xt_cycle_pinned_jit_depth(Some(6));
            publish_xt_post_quota_ns(700);
            add_xt_pass_ns(90);
            assert_eq!(peer_proven_jit_depth(), 4);
            assert_eq!(xt_cycle_pinned_jit_depth(), 6);
            assert_eq!((xt_post_quota_ns(), xt_pass_ns()), (700, 90));
            add_xt_cycle_pinned_jit_depth(None);
            assert_eq!(xt_cycle_pinned_jit_depth(), 0, "inexact voids the credit");
            reset_peer_proven_jit_depth();
            assert_eq!(peer_proven_jit_depth(), 0);
            assert_eq!(xt_cycle_pinned_jit_depth(), 0);
            add_xt_cycle_pinned_jit_depth(Some(1));
            assert_eq!(xt_cycle_pinned_jit_depth(), 1, "the poison is lifted");
            assert_eq!((xt_post_quota_ns(), xt_pass_ns()), (0, 0));
        });
    }

    /// gch h1/yp: the conservatively deposited snapshot words are one pause's
    /// gather; the next pause opens without them.
    #[test]
    fn gch_h1yp_a_pause_opens_without_the_previous_conservative_deposit_words() {
        let l = Arc::new(PauseLedger::new());
        with_pause_ledger(&l, || {
            publish_conservative_deposit_words(vec![0x7000_0010, 0x7000_0200]);
            let mut words = Vec::new();
            conservative_deposit_words_into(&mut words, |v| v != 0x7000_0200);
            assert_eq!(words, vec![0x7000_0010], "the filter applies");
            reset_peer_proven_jit_depth();
            words.clear();
            conservative_deposit_words_into(&mut words, |_| true);
            assert!(words.is_empty(), "a pause opens with no deposit words");
        });
    }

    /// gc-common w9-f: the six take-over counts (`XT_*_LAST_CYCLE` process
    /// statics until then) are the bound ledger's. VM B's take-over, on B's
    /// initiator, neither adds to VM A's counts nor zeroes them -- the zeroing
    /// is what could erase A's `unclassified` between A's passes and A's
    /// verdict. Opening a pause zeroes them, and an unbound thread reads its
    /// own (empty) fallback.
    ///
    /// Clears through the ledger itself rather than `reset_xt_cycle` (which
    /// was a process-wide verdict reset until gc-common w10-g).
    #[test]
    fn the_takeover_counts_are_the_bound_vms_only() {
        let a = Arc::new(PauseLedger::new());
        let b = Arc::new(PauseLedger::new());
        with_pause_ledger(&a, || {
            publish_xt_pass(2, 1, 7);
            publish_xt_helper_window(3, 4);
            assert_eq!(xt_cycle_coverage(), (1, 2, 1, 7, 3, 4));
        });
        let b2 = Arc::clone(&b);
        std::thread::spawn(move || {
            with_pause_ledger(&b2, || {
                publish_xt_pass(5, 0, 9);
                assert_eq!(xt_cycle_coverage(), (1, 5, 0, 9, 0, 0), "B's pass is B's");
                // B's next take-over opens its cycle.
                b2.xt.clear();
                assert_eq!(xt_cycle_coverage(), (0, 0, 0, 0, 0, 0));
            });
        })
        .join()
        .expect("B's initiator");
        with_pause_ledger(&a, || {
            assert_eq!(
                xt_cycle_coverage(),
                (1, 2, 1, 7, 3, 4),
                "B's take-over must neither add to nor zero A's counts"
            );
        });
        assert_eq!(
            std::thread::spawn(xt_cycle_coverage)
                .join()
                .expect("unbound reader"),
            (0, 0, 0, 0, 0, 0),
            "an unbound thread reads its own empty fallback"
        );
        a.open();
        assert_eq!(a.xt.snapshot(), (0, 0, 0, 0, 0, 0), "a pause opens at zero");
    }

    /// gc-common w10-g: the take-over verdict (`TAKEOVER_VERDICT`, a process
    /// static until then) is the bound ledger's. VM B's pause opening, and VM
    /// B's take-over, on B's initiator, leave VM A's verdict alone -- the
    /// opening used to reset it to `NONE`, i.e. licence `Move` for the peer A
    /// froze. Opening A's own pause clears it, and an unbound thread reads its
    /// own fallback.
    #[test]
    fn a_frozen_native_peer_has_an_explicit_vm_guard_without_a_jit_entry() {
        std::thread::spawn(|| {
            let a = Arc::new(PauseLedger::new());
            let b = Arc::new(PauseLedger::new());
            assert!(!compiled_frames_live());
            with_pause_ledger(&a, || {
                publish_takeover_verdict(TakeoverVerdict {
                    frozen: 1,
                    ..TakeoverVerdict::NONE
                });
                assert!(
                    frozen_peers_live(),
                    "a frozen native peer needs an explicit collector guard"
                );
                assert!(
                    !compiled_frames_live(),
                    "claim protection must not manufacture a JIT entry"
                );
                with_pause_ledger(&b, || {
                    assert!(
                        !frozen_peers_live(),
                        "another VM must not inherit A's refusal"
                    );
                });
                assert!(frozen_peers_live(), "A's guard must survive B's scope");
                publish_takeover_verdict(TakeoverVerdict {
                    helper_windows: 1,
                    ..TakeoverVerdict::NONE
                });
                assert!(!frozen_peers_live());
                assert!(
                    conservative_peers_live(),
                    "a pinned A5 band has no JIT entry but still needs its pins honoured"
                );
                a.open();
                assert!(
                    !frozen_peers_live(),
                    "the next pause must not inherit a stale guard"
                );
            });
        })
        .join()
        .unwrap();
    }

    #[test]
    fn the_takeover_verdict_is_the_bound_vms_only() {
        let a = Arc::new(PauseLedger::new());
        let b = Arc::new(PauseLedger::new());
        let frozen = TakeoverVerdict {
            frozen: 1,
            pins_complete: false,
            ..TakeoverVerdict::NONE
        };
        assert_eq!(frozen.licence(true), MoveLicence::NonMoving);
        // Opened through the ledger, not `reset_peer_proven_jit_depth`, which
        // also clears the helper-window pins (per VM since gc-common w21-e,
        // and this thread's own under `cfg(test)`).
        a.open();
        with_pause_ledger(&a, || {
            publish_takeover_verdict(frozen);
            assert_eq!(takeover_verdict(), frozen);
            assert!(takeover_forbids_unpinnable_move());
        });
        let b2 = Arc::clone(&b);
        std::thread::spawn(move || {
            with_pause_ledger(&b2, || {
                // B's request opens B's pause, B's take-over runs and reads
                // nothing frozen.
                b2.open();
                reset_xt_cycle();
                publish_takeover_verdict(TakeoverVerdict::NONE);
                assert_eq!(takeover_verdict(), TakeoverVerdict::NONE);
            });
        })
        .join()
        .expect("B's initiator");
        with_pause_ledger(&a, || {
            assert_eq!(
                takeover_verdict(),
                frozen,
                "B's pause must neither reset nor overwrite A's verdict"
            );
            assert_eq!(
                may_move_under_takeover(true),
                MoveLicence::NonMoving,
                "A's collector still reads A's frozen peer"
            );
        });
        assert_eq!(
            std::thread::spawn(takeover_verdict)
                .join()
                .expect("unbound reader"),
            TakeoverVerdict::NONE,
            "an unbound thread reads its own fallback"
        );
        a.open();
        with_pause_ledger(&a, || {
            assert_eq!(
                takeover_verdict(),
                TakeoverVerdict::NONE,
                "A's next pause opens clear"
            );
        });
        assert_eq!(
            TakeoverVerdict::UNKNOWN.licence(true),
            MoveLicence::NonMoving
        );
        assert_eq!(
            TakeoverVerdict::UNKNOWN.licence(false),
            MoveLicence::NonMoving
        );
    }

    /// gc-common w21-e: the moving-young coverage rows (the verdict, its
    /// reasons, the un-rewritable flag, the scan count and the helper-window
    /// pins; six process statics until then) are the bound ledger's. VM B's
    /// coverage open, on B's initiator, leaves VM A's rows alone -- it used to
    /// erase A's "incomplete" and A's pins, i.e. licence a move for a peer A
    /// froze -- and B's marks, scans and pins never reach A's collector.
    #[test]
    fn the_coverage_rows_are_the_bound_vms_only() {
        const PIN_A: usize = 0x7a21_0000;
        const PIN_B: usize = 0x7b21_0000;
        let a = Arc::new(PauseLedger::new());
        let b = Arc::new(PauseLedger::new());
        with_pause_ledger(&a, || {
            begin_moving_young_coverage_cycle();
            mark_moving_young_coverage_incomplete_because(incomplete_reason::XT_TAKEOVER);
            note_conservative_jit_scan();
            add_xt_cycle_pinned_jit_roots(&[PIN_A, PIN_A]);
        });
        let b2 = Arc::clone(&b);
        std::thread::spawn(move || {
            with_pause_ledger(&b2, || {
                // B's request opens B's coverage cycle...
                begin_moving_young_coverage_cycle();
                assert!(
                    !moving_young_coverage_incomplete(),
                    "A's verdict is not B's"
                );
                assert!(!unrewritable_peer_state());
                assert_eq!(conservative_jit_scans(), 0, "A's scan is not B's");
                assert!(!pinned_jit_roots_snapshot().contains(&PIN_A));
                // ...and B's pause records its own.
                mark_moving_young_coverage_incomplete_because(
                    incomplete_reason::UNPUBLISHED_FRAME_OOP,
                );
                add_xt_cycle_pinned_jit_roots(&[PIN_B]);
            });
        })
        .join()
        .expect("B's initiator");
        with_pause_ledger(&a, || {
            assert!(
                moving_young_coverage_incomplete(),
                "B's coverage open must not erase A's verdict"
            );
            assert_eq!(
                moving_young_incomplete_reason(),
                incomplete_reason::XT_TAKEOVER
            );
            assert_eq!(
                moving_young_incomplete_reason_mask(),
                1usize << incomplete_reason::XT_TAKEOVER,
                "B's reason is not A's"
            );
            assert!(unrewritable_peer_state());
            assert_eq!(conservative_jit_scans(), 1);
            let pins = pinned_jit_roots_snapshot();
            assert!(
                pins.contains(&PIN_A) && !pins.contains(&PIN_B),
                "A's collector pins A's helper windows, all of them, and none of B's"
            );
            assert_eq!(xt_cycle_pinned_jit_root_count(), 1, "a pin is kept once");
        });
        assert_eq!(
            b.coverage_snapshot(),
            CoverageSnapshot {
                incomplete: true,
                reason: incomplete_reason::UNPUBLISHED_FRAME_OOP,
                reason_mask: 1usize << incomplete_reason::UNPUBLISHED_FRAME_OOP,
                unrewritable: false,
                scans: 0,
                xt_pins: vec![PIN_B],
            }
        );
        assert!(
            !std::thread::spawn(moving_young_coverage_incomplete)
                .join()
                .expect("unbound reader"),
            "an unbound thread reads neither VM's rows"
        );
        // A's next collection opens A's rows afresh; clearing B's pins (what
        // every pause's open does) leaves B's verdict to B's next collection.
        with_pause_ledger(&a, begin_moving_young_coverage_cycle);
        assert_eq!(a.coverage_snapshot(), CoverageSnapshot::default());
        with_pause_ledger(&b, clear_xt_cycle_pinned_jit_roots);
        let b_now = b.coverage_snapshot();
        assert!(b_now.xt_pins.is_empty() && b_now.incomplete);
    }

    /// gc-common w21-e: a coverage write from a thread no binding covers is not
    /// lost. It lands in the orphan rows (here, under `cfg(test)`, the thread's
    /// own fallback's), which a bound read ORs in -- the bound ledger's reason
    /// first -- and which the next coverage open clears with the ledger's.
    #[test]
    fn an_unbound_coverage_write_is_seen_by_a_bound_read_and_cleared_by_the_open() {
        const PIN: usize = 0x7c21_0000;
        let a = Arc::new(PauseLedger::new());
        with_pause_ledger(&a, begin_moving_young_coverage_cycle);
        let before = coverage_orphan_writes();
        mark_moving_young_coverage_incomplete_because(incomplete_reason::UNREGISTERED_JIT_FRAME);
        note_conservative_jit_scan();
        add_xt_cycle_pinned_jit_roots(&[PIN]);
        assert!(
            coverage_orphan_writes() >= before + 3,
            "each unbound write is counted"
        );
        assert_eq!(
            a.coverage_snapshot(),
            CoverageSnapshot::default(),
            "an unbound write is not the ledger's own"
        );
        with_pause_ledger(&a, || {
            assert!(
                moving_young_coverage_incomplete(),
                "a bound read sees the orphan rows"
            );
            assert_eq!(
                moving_young_incomplete_reason(),
                incomplete_reason::UNREGISTERED_JIT_FRAME
            );
            assert_eq!(conservative_jit_scans(), 1);
            assert!(pinned_jit_roots_snapshot().contains(&PIN));
            // A bound write goes to the ledger, and its reason comes first.
            mark_moving_young_coverage_incomplete_because(incomplete_reason::XT_HELPER_WINDOW);
            assert_eq!(
                moving_young_incomplete_reason(),
                incomplete_reason::XT_HELPER_WINDOW
            );
            assert_eq!(
                moving_young_incomplete_reason_mask(),
                (1usize << incomplete_reason::XT_HELPER_WINDOW)
                    | (1usize << incomplete_reason::UNREGISTERED_JIT_FRAME)
            );
        });
        assert!(a.coverage_snapshot().unrewritable);
        assert!(
            !unrewritable_peer_state(),
            "an unbound read sees the orphan rows only"
        );
        // The next open clears the ledger's rows and the orphan rows.
        with_pause_ledger(&a, begin_moving_young_coverage_cycle);
        assert_eq!(a.coverage_snapshot(), CoverageSnapshot::default());
        assert!(!moving_young_coverage_incomplete());
        assert_eq!(conservative_jit_scans(), 0);
        assert_eq!(xt_cycle_pinned_jit_root_count(), 0);
    }
}

// ===========================================================================
// gen r4w5/pinwords5 (2026-09-24) — THE PER-PAUSE YOUNG PIN-WORD LEDGER
// ===========================================================================
//
// One self-contained block (the gc-common session edits the rest of this
// file). Its hooks outside the block are the `open_young_pin_pause()` line in
// `reset_peer_proven_jit_depth` and, since gc-common w36-c (per-VM ledger; see
// "Per VM" below), the `young_pins` field of `PauseLedger`.
//
// # What it is for
//
// `gen_heap::collect_garbage_inner`'s term 4
// (`unrewritable_conservative_jit_roots`) diverts every young cycle on which a
// compiled frame is live and a conservative JIT scan ran, because the pin
// registry (`pinned_jit_roots_snapshot`) holds OBJECT BASES and cannot see the
// interior / derived words that crashed the QDox repro. This ledger records the
// RAW WORDS instead: every conservative word, published for THIS pause, that
// lands in a young region and that no precise channel rewrites. An interior or
// derived word counts exactly like a base, because it is never resolved.
// See `docs/feature-designs/generative-gc-architecture.md`
// (option B) and `docs/feature-designs/generative-gc-architecture.md`.
//
// # Why a pause STAMP and not a reset (the reset-ordering prerequisite)
//
// Every other per-pause ledger here is CLEARED when a pause opens, and a clear
// that lands after a peer's deposit erases it — the defect of
// `docs/feature-designs/generative-gc-architecture.md`.
// This one is never cleared. `open_young_pin_pause` advances a stamp (from
// `reset_peer_proven_jit_depth`, i.e. the barrier's `request_stw`, under the
// barrier lock, before `stw_requested` is visible); each deposit carries the
// stamp current when it was made; a reader keeps only deposits whose stamp is
// the current one. So a reader can tell "published for THIS pause" (stamp ==
// current) from "stale from an earlier pause" (stamp < current) from "never
// published" (stamp 0), and no reset — `begin_moving_young_coverage_cycle`,
// the initiator's `collect_roots` clears — can erase a peer's deposit.
//
// # Completeness is the safety property
//
// [`pause_young_pin_words`] returns `false` unless the deposits made for this
// pause account for EVERY JIT entry in the process: the sum of the chain
// depths the depositing threads reported must reach [`depth`] (the
// process-wide JIT depth `JitEntryGuard` maintains). A thread inside compiled
// code that did not deposit — a blocked peer, a peer frozen by the take-over,
// a peer whose park path does not deposit — leaves a shortfall, and the
// function says "incomplete". A per-thread overflow, an unread band or a torn
// read says the same. The caller must then divert.

/// Words each thread may deposit per pause. A deposit that would exceed it
/// marks the thread's ledger incomplete (the reader then returns `false`).
///
/// 2048 words: 16 KiB of words, 2 KiB of tags and a 32 KiB dedup index per
/// thread, allocated once per thread at its first deposit. The wave-4 census
/// measured at most 9 young OBJECTS pinned per cycle
/// (`cjdiv_young_pins_max=9`), but raw words (duplicates removed) run far
/// higher than objects: gcj j1/yg measured a 4-thread in-process javac
/// (`GciPn1JavacTreeProbe`) depositing 266 to 1381 distinct young words per
/// thread (most from layout-free Rust frames between JIT entries), so at the
/// old cap of 256 every such deposit overflowed and declined the pinned copy
/// on 14 of 24 cycles. The pinned copy's own 1/8 page bound still declines a
/// pause whose words pin too much.
pub const YOUNG_PIN_WORDS_PER_THREAD: usize = 2048;

/// Bytes BELOW a young region's base that still count as "in young".
///
/// A base-minus-offset derived pointer (an array cursor biased by the header
/// size, `p = base + HEADER - k*scale`) can sit just below the first object of
/// a region. The slack only ever ADDS words to the ledger, which is the safe
/// direction: a false positive costs one diverted cycle.
pub const YOUNG_PIN_LOW_SLACK: usize = 256;

/// The two young regions as the ledger screens words against them:
/// `[base - YOUNG_PIN_LOW_SLACK, end]`, INCLUSIVE of one-past-end (a loop
/// cursor that has just run off the last object of a region still names it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct YoungPinRange {
    /// `(lo, hi)` inclusive; `hi == 0` is an empty slot.
    spans: [(usize, usize); 2],
}

impl YoungPinRange {
    /// Build from two `[base, end)` regions; a region with `base == 0` or
    /// `end <= base` is absent.
    pub fn from_regions(regions: [(usize, usize); 2]) -> Self {
        let mut spans = [(0usize, 0usize); 2];
        for (span, &(base, end)) in spans.iter_mut().zip(regions.iter()) {
            if base != 0 && end > base {
                *span = (base.saturating_sub(YOUNG_PIN_LOW_SLACK), end);
            }
        }
        Self { spans }
    }

    /// The two young semispaces `gen_heap` publishes in `JIT_REGION_BOUNDS`
    /// (the same table `gen_heap::addr_in_published_young_regions` reads).
    /// Empty on every collector but the generational one.
    pub fn published() -> Self {
        let w = &crate::gen_heap::JIT_REGION_BOUNDS.words;
        Self::from_regions([
            (w[0].load(Ordering::Acquire), w[1].load(Ordering::Acquire)),
            (w[2].load(Ordering::Acquire), w[3].load(Ordering::Acquire)),
        ])
    }

    /// No young region is published: every screen against this range is
    /// vacuous, so a caller must not read "no word matched" as good news.
    pub fn is_empty(&self) -> bool {
        self.spans.iter().all(|&(_, hi)| hi == 0)
    }

    /// Does the raw word `w` fall in either young region (with the low slack,
    /// inclusive of one-past-end)?
    #[inline]
    pub fn contains(&self, w: usize) -> bool {
        self.spans
            .iter()
            .any(|&(lo, hi)| hi != 0 && w >= lo && w <= hi)
    }

    /// Does a `len`-byte read at `w` stay inside a young region proper
    /// (`[base, end)`)? [`contains`](Self::contains) also admits the low slack
    /// and one-past-end, and neither is inside the arena: the page past a
    /// region's end can be unmapped, and the young commit screen answers
    /// `true` for every address outside its tables, so a header screen must
    /// ask this before it dereferences (netty buffer suites, 2026-09-27:
    /// SIGSEGV reading `[end + 6]` for a stack word equal to a region's end).
    #[inline]
    pub fn contains_span(&self, w: usize, len: usize) -> bool {
        self.spans.iter().any(|&(lo, hi)| {
            let base = if lo == 0 { 0 } else { lo + YOUNG_PIN_LOW_SLACK };
            hi != 0 && w >= base && w.checked_add(len).is_some_and(|e| e <= hi)
        })
    }
}

/// One thread's deposit slot. Written only by its owning thread, through
/// [`YoungPinDeposit`]; read by the collector with a seqlock on `stamp`.
pub struct YoungPinSlot {
    /// Pause stamp of the last COMMITTED deposit; `0` = never published, or a
    /// deposit is being written.
    stamp: AtomicU64,
    /// The owning thread's `JIT_ENTRY_CHAIN` length when it deposited.
    depth: AtomicUsize,
    /// Words stored in `words[..len]`.
    len: AtomicUsize,
    /// How many of them came from a layout-free band (census only).
    native: AtomicUsize,
    /// gcj-j1pn-pinned-cycle-review-findings item 1: how many LEADING words
    /// (`words[..regs]`) are the depositing thread's register image
    /// ([`YoungPinDeposit::note_register`]); read by
    /// [`YoungPinLedger::read_register_words`].
    regs: AtomicUsize,
    /// The deposit overflowed, or its scan could not read everything it had
    /// to: the ledger is incomplete for this pause.
    incomplete: AtomicBool,
    words: Box<[AtomicUsize]>,
    /// gcj-j1pn-pinned-cycle-review-findings item 1: `layout_free[i]` says
    /// `words[i]` was deposited at least once from a layout-free band or a
    /// register ([`YoungPinDeposit::note_native`]), not only from a compiled
    /// frame's band. Read by [`YoungPinLedger::read_tagged`].
    layout_free: Box<[AtomicBool]>,
    /// gcj-j1pn-young-pin-ledger-review-findings item 7: the open deposit's
    /// dedup index, an open-addressing table of `position + 1` into `words`
    /// (`0` = empty), [`YOUNG_PIN_DEDUP_CELLS`] cells. Written and read only
    /// by the owning thread's deposit; cleared at `begin_deposit`.
    seen: Box<[AtomicUsize]>,
}

/// Cells of a [`YoungPinSlot`]'s dedup index: a power of two, twice the word
/// cap, so a full deposit's table is at most half occupied and every probe
/// ends.
const YOUNG_PIN_DEDUP_CELLS: usize = 2 * YOUNG_PIN_WORDS_PER_THREAD;

const _: () = assert!(YOUNG_PIN_DEDUP_CELLS.is_power_of_two());

/// The dedup index's home cell for `word` (Fibonacci hashing of the word
/// without its alignment bits).
#[inline]
fn young_pin_dedup_home(word: usize) -> usize {
    let h = ((word >> 3) as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (h >> (64 - YOUNG_PIN_DEDUP_CELLS.trailing_zeros())) as usize
}

impl YoungPinSlot {
    fn new() -> Self {
        Self {
            stamp: AtomicU64::new(0),
            depth: AtomicUsize::new(0),
            len: AtomicUsize::new(0),
            native: AtomicUsize::new(0),
            regs: AtomicUsize::new(0),
            incomplete: AtomicBool::new(false),
            words: (0..YOUNG_PIN_WORDS_PER_THREAD)
                .map(|_| AtomicUsize::new(0))
                .collect(),
            layout_free: (0..YOUNG_PIN_WORDS_PER_THREAD)
                .map(|_| AtomicBool::new(false))
                .collect(),
            seen: (0..YOUNG_PIN_DEDUP_CELLS)
                .map(|_| AtomicUsize::new(0))
                .collect(),
        }
    }
}

/// An open deposit into the calling thread's slot. Nothing is visible to a
/// reader until [`YoungPinDeposit::commit`]; a deposit dropped without a
/// commit leaves the slot at stamp 0 (never published), which the reader
/// treats as a missing deposit — the fail-closed direction.
///
/// Never allocates: the words go straight into the slot's preallocated cells.
pub struct YoungPinDeposit {
    slot: std::sync::Arc<YoungPinSlot>,
    stamp: u64,
    len: usize,
    native: usize,
    /// Leading register-image words (see [`YoungPinSlot`]'s `regs`).
    regs: usize,
    incomplete: bool,
    /// gcj j1/yg: the incompleteness was the word cap, not a scan's verdict.
    overflowed: bool,
}

impl YoungPinDeposit {
    /// Record a word read from a compiled frame's band (after the caller's
    /// rewritten-slot screen).
    #[inline]
    pub fn note(&mut self, word: usize) {
        self.push(word, false);
    }

    /// Record a word read from a layout-free band: a native / Rust frame in the
    /// JIT band, a frame with no usable layout, or a register.
    #[inline]
    pub fn note_native(&mut self, word: usize) {
        self.push(word, true);
    }

    /// Record a word of the depositing thread's REGISTER image (a layout-free
    /// word, counted as [`Self::note_native`] counts it). gcj-j1pn-pinned-cycle-review-findings
    /// item 1: register words are tagged so the pinned plan can tell them from
    /// stack words ([`YoungPinLedger::read_register_words`]). The tag is a
    /// count of LEADING words, so only register words noted before any other
    /// word are tagged; a later one is recorded untagged (a layout-free word,
    /// the rule the plan applied to every word before the tag).
    #[inline]
    pub fn note_register(&mut self, word: usize) {
        let tagging = self.regs == self.len;
        self.push(word, true);
        if tagging {
            self.regs = self.len;
        }
    }

    fn push(&mut self, word: usize, native: bool) {
        if self.incomplete {
            return;
        }
        // Raw words, deduplicated: one entry per distinct value. The same
        // word in two slots pins the same address, and the cap is on what the
        // consumer has to act on, not on how many copies the stack holds.
        //
        // gcj j1pn ledger item 7: through the slot's hash index, not a linear
        // scan of up to 256 stored words per word read (the sweep reaches the
        // stack top whenever the ledger licenses moves, so this runs per word
        // of every Rust frame above the entry).
        let seen = &self.slot.seen;
        let mask = seen.len() - 1;
        let mut at = young_pin_dedup_home(word) & mask;
        loop {
            let pos = seen[at].load(Ordering::Relaxed);
            if pos == 0 {
                break;
            }
            if self.slot.words[pos - 1].load(Ordering::Relaxed) == word {
                // gcj j1pn cycle item 1: a word seen from a compiled band and
                // again from a layout-free band or a register is layout-free
                // (the source the start-word rule must not discount).
                let lf = &self.slot.layout_free[pos - 1];
                if native && !lf.load(Ordering::Relaxed) {
                    lf.store(true, Ordering::Relaxed);
                    self.native += 1;
                }
                return;
            }
            at = (at + 1) & mask;
        }
        match self.slot.words.get(self.len) {
            Some(cell) => {
                cell.store(word, Ordering::Relaxed);
                self.slot.layout_free[self.len].store(native, Ordering::Relaxed);
                self.len += 1;
                seen[at].store(self.len, Ordering::Relaxed);
                if native {
                    self.native += 1;
                }
            }
            None => {
                self.incomplete = true;
                self.overflowed = true;
            }
        }
    }

    /// This thread's scan could not read everything it had to (a truncated
    /// band, a register file it cannot capture, a chain it could not borrow):
    /// the reader must return `false` for this pause.
    pub fn mark_incomplete(&mut self) {
        self.incomplete = true;
    }

    /// gcj j2/x1: has [`Self::mark_incomplete`] (or an overflow) already made
    /// this deposit incomplete? Every later word is dropped, so a scanner may
    /// stop reading.
    pub fn is_incomplete(&self) -> bool {
        self.incomplete
    }

    /// Distinct words recorded so far.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no word has been recorded.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Publish the deposit for the pause current at [`YoungPinLedger::begin_deposit`],
    /// crediting `jit_depth` JIT entries (the depositing thread's chain length,
    /// read over the same chain the scan walked).
    pub fn commit(self, jit_depth: usize) {
        // gcj j1/yg: why a deposit was incomplete, for the unproven-frame
        // arm's census (`young_pin_unproven_read_census`).
        if self.incomplete {
            let slot = if self.overflowed {
                unproven_refusal::DEPOSIT_OVERFLOW
            } else {
                unproven_refusal::DEPOSIT_MARKED
            };
            with_young_pin_census(|c| {
                c.unproven[slot].fetch_add(1, Ordering::Relaxed);
            });
        }
        let s = &self.slot;
        s.len.store(self.len, Ordering::Relaxed);
        s.native.store(self.native, Ordering::Relaxed);
        s.regs.store(self.regs, Ordering::Relaxed);
        s.depth.store(jit_depth, Ordering::Relaxed);
        s.incomplete.store(self.incomplete, Ordering::Relaxed);
        // Release: a reader that sees this stamp sees every store above and
        // every word cell written since `begin_deposit`.
        s.stamp.store(self.stamp, Ordering::Release);
    }
}

/// What one read of the ledger found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct YoungPinRead {
    /// Every JIT entry in the process is accounted for by a deposit made for
    /// this pause, and no deposit overflowed or was torn.
    pub complete: bool,
    /// Words appended to the caller's vector.
    pub words: usize,
    /// The subset from layout-free bands and registers.
    pub native: usize,
    /// Deposits made for this pause.
    pub deposits: usize,
    /// Sum of their reported JIT depths.
    pub depth: usize,
    /// gen r4w6/pinstale6: of `words`, the peer REGISTER words the
    /// cross-thread scan captured this pause
    /// ([`YoungPinLedger::note_peer_reg_word`]); also counted in `native`.
    pub peer_regs: usize,
    /// gcd d3/m: of `words`, the distinct young words of the blocked-peer
    /// BANDS (register file plus whole stack) the helper-window pass read into
    /// this pause's ledger ([`YoungPinLedger::note_peer_band`]); also counted
    /// in `native`. Zero unless `CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER` or
    /// the blocked-monitor proof's ledger capture engaged.
    pub peer_bands: usize,
    /// gcj j1/yg: a deposit was incomplete (overflowed, a short sweep, a
    /// register file it could not capture), torn by a restart, or a peer
    /// capture overflowed or failed.
    pub deposit_incomplete: bool,
    /// gcj j1/yg: the deposits accounted for less JIT depth than the process
    /// holds (a thread that holds compiled frames deposited nothing).
    pub depth_short: bool,
}

/// Slots of [`YoungPinLedger`]'s census; see [`young_pin_ledger_census`].
pub const YOUNG_PIN_CENSUS_LEN: usize = 8;

/// Most distinct peer register words one pause records
/// ([`YoungPinLedger::note_peer_reg_word`]); one more marks the ledger
/// incomplete for the pause. 16 registers for each of 64 captured peers.
pub const PEER_REG_PIN_WORDS_CAP: usize = 1024;

/// Most distinct peer-band words one pause records
/// ([`YoungPinLedger::note_peer_band`]); one more marks the ledger incomplete
/// for the pause. A band is a blocked peer's whole stack, so this is sized for
/// the stale young words deep Rust frames leave behind, not for live oops; the
/// pinned copy's 1/8 page bound usually declines such a pause first.
pub const PEER_BAND_PIN_WORDS_CAP: usize = 16384;

/// The blocked-peer band words of one pause (gcd d3/m; see
/// [`YoungPinLedger::note_peer_band`]). Stamped rather than cleared, like
/// [`PeerRegPinWords`].
#[derive(Debug)]
struct PeerBandPinWords {
    stamp: u64,
    /// Sorted, deduplicated.
    words: Vec<usize>,
    /// JIT depth the captured bands account for (their threads' published
    /// depths).
    depth: usize,
    /// Helper windows among the captured bands (a take-over pause's windows,
    /// as opposed to the blocked-monitor proof's ledger-only captures).
    windows: u32,
    /// A band could not be read whole, a peer's depth was unknown, or the
    /// word cap overflowed: the ledger is incomplete for this pause.
    incomplete: bool,
    /// gcj-j1pn-pinned-cycle-review-findings item 1: the young words of the
    /// captured bands' REGISTER files (a subset of `words`, unsorted, capped
    /// at [`PEER_BAND_PIN_WORDS_CAP`]; see
    /// [`YoungPinLedger::note_peer_band_registers`]).
    regs: Vec<usize>,
}

impl PeerBandPinWords {
    const fn new() -> Self {
        Self {
            stamp: 0,
            words: Vec::new(),
            depth: 0,
            windows: 0,
            incomplete: false,
            regs: Vec::new(),
        }
    }

    /// Re-open for `stamp` if the rows belong to an earlier pause.
    fn at(&mut self, stamp: u64) {
        if self.stamp != stamp {
            self.stamp = stamp;
            self.words.clear();
            self.depth = 0;
            self.windows = 0;
            self.incomplete = false;
            self.regs.clear();
        }
    }
}

/// The peer register words of one pause (see
/// [`YoungPinLedger::note_peer_reg_word`]). Stamped rather than cleared, like
/// the deposit slots: a word noted under an earlier stamp is never read.
#[derive(Debug)]
struct PeerRegPinWords {
    stamp: u64,
    /// Sorted, deduplicated (gci i1/pn; see `note_peer_reg_word`).
    words: Vec<usize>,
    overflow: bool,
}

impl PeerRegPinWords {
    const fn new() -> Self {
        Self {
            stamp: 0,
            words: Vec::new(),
            overflow: false,
        }
    }
}

/// The ledger itself: a pause stamp, the registered slots, and the peer
/// register words. One per VM, in its [`PauseLedger`] (gc-common w36-c).
///
/// A struct rather than loose statics so that the unit tests can build a
/// private one (the gc tests run in parallel threads of one process, and a
/// shared ledger would let one test's deposit satisfy another's shortfall).
/// The census, which spans the process's life, is [`YoungPinCensus`].
pub struct YoungPinLedger {
    stamp: AtomicU64,
    slots: parking_lot::Mutex<Vec<std::sync::Arc<YoungPinSlot>>>,
    /// gen r4w6/pinstale6: peer register words captured by the cross-thread
    /// scan for the current pause — a population no thread deposits for
    /// itself (see [`Self::note_peer_reg_word`]).
    peer_regs: parking_lot::Mutex<PeerRegPinWords>,
    /// gcd d3/m: blocked-peer band words the helper-window pass read for the
    /// current pause, with the JIT depth they account for (see
    /// [`Self::note_peer_band`]).
    peer_bands: parking_lot::Mutex<PeerBandPinWords>,
    /// [`YOUNG_PIN_ORPHAN_WRITES`] when this ledger's current pause opened;
    /// see [`Self::orphaned_since_open`].
    orphans_at_open: AtomicU64,
}

impl std::fmt::Debug for YoungPinLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("YoungPinLedger")
            .field("stamp", &self.stamp())
            .field("slots", &self.slots.lock().len())
            .finish_non_exhaustive()
    }
}

impl Default for YoungPinLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl YoungPinLedger {
    /// A ledger at stamp 1 with no slots.
    pub const fn new() -> Self {
        Self {
            // 0 is reserved for "never published".
            stamp: AtomicU64::new(1),
            slots: parking_lot::Mutex::new(Vec::new()),
            peer_regs: parking_lot::Mutex::new(PeerRegPinWords::new()),
            peer_bands: parking_lot::Mutex::new(PeerBandPinWords::new()),
            orphans_at_open: AtomicU64::new(0),
        }
    }

    /// Whether a young pin-word write that no VM's ledger could take (an
    /// UNBOUND deposit or register word, [`note_young_pin_orphan_write`]) was
    /// made since this ledger's current pause opened -- or before its first
    /// open. Such a write may have been this pause's, and it was dropped, so
    /// the pause's read must fail closed. Process-wide, hence conservative
    /// across VMs: another VM's orphan also fails this VM's read.
    pub fn orphaned_since_open(&self) -> bool {
        YOUNG_PIN_ORPHAN_WRITES.load(Ordering::Acquire)
            != self.orphans_at_open.load(Ordering::Acquire)
    }

    /// Record one peer REGISTER word the cross-thread scan captured for the
    /// CURRENT pause (gen r4w6/pinstale6). The caller screens it to a young
    /// region first ([`note_peer_reg_pin_word`]).
    ///
    /// These words are part of the ledger's population — "a conservative word
    /// in young that no precise channel rewrites" — but no thread deposits
    /// them: they sit in the register file of a peer the collector read from
    /// outside (a blocked peer's helper window, or a frozen peer). Deduplicated;
    /// more than [`PEER_REG_PIN_WORDS_CAP`] distinct words makes the pause's
    /// ledger incomplete. Called by the collecting thread only, never from a
    /// signal handler (the first word of a pause may allocate).
    pub fn note_peer_reg_word(&self, word: usize) {
        let stamp = self.stamp();
        let mut g = self.peer_regs.lock();
        if g.stamp != stamp {
            g.stamp = stamp;
            g.words.clear();
            g.overflow = false;
        }
        if g.overflow {
            return;
        }
        // gci i1/pn: the words are kept sorted, so the dedup is a binary
        // search. It was `Vec::contains`, a linear scan of up to
        // `PEER_REG_PIN_WORDS_CAP` words per call, i.e. ~0.5 M compares on a
        // pause that captures the cap (gch h1/yp review note 6). Same set,
        // same cap rule: a duplicate never consumes the cap.
        let at = match g.words.binary_search(&word) {
            Ok(_) => return,
            Err(at) => at,
        };
        if g.words.len() >= PEER_REG_PIN_WORDS_CAP {
            g.overflow = true;
            return;
        }
        g.words.insert(at, word);
    }

    /// Record one blocked peer's BAND for the CURRENT pause (gcd d3/m): every
    /// word of its register file and of its whole stack `[rsp, top)` that the
    /// caller screened to a young region ([`YoungPinRange::contains`]), raw
    /// (interior and derived words kept as they are), and the JIT depth the
    /// band accounts for.
    ///
    /// The band is read by the collector from OUTSIDE the peer (the
    /// helper-window pass), so no thread deposits it for itself. Unlike the
    /// register words it CREDITS DEPTH: a band read whole covers every JIT
    /// frame on that thread's stack, which is what lets a pause with a blocked
    /// compiled peer read as complete. So the rules fail closed:
    ///
    /// * `depth == None` (the peer never published a depth slot) or
    ///   `whole == false` (the band was cut short, or the peer could not be
    ///   read) marks the pause incomplete, and credits nothing;
    /// * more than [`PEER_BAND_PIN_WORDS_CAP`] distinct words marks it
    ///   incomplete.
    ///
    /// `helper_window` counts the band as one of the pause's helper windows
    /// ([`Self::peer_band_windows`]). Called by the collecting thread only,
    /// after the peer was released (it allocates).
    pub fn note_peer_band(
        &self,
        words: &[usize],
        depth: Option<usize>,
        whole: bool,
        helper_window: bool,
    ) {
        let stamp = self.stamp();
        let mut g = self.peer_bands.lock();
        g.at(stamp);
        let Some(depth) = depth.filter(|_| whole) else {
            g.incomplete = true;
            return;
        };
        g.depth = g.depth.saturating_add(depth);
        if helper_window {
            g.windows = g.windows.saturating_add(1);
        }
        if g.incomplete {
            return;
        }
        // gcj j1pn ledger item 7: sort the incoming band alone, then MERGE it
        // into the sorted pause set -- the stable sort finds the two sorted
        // runs and merges them in linear time -- instead of re-sorting the
        // whole set for every band.
        let mut incoming = words.to_vec();
        incoming.sort_unstable();
        incoming.dedup();
        g.words.extend_from_slice(&incoming);
        g.words.sort();
        g.words.dedup();
        if g.words.len() > PEER_BAND_PIN_WORDS_CAP {
            g.words.clear();
            g.incomplete = true;
        }
    }

    /// gcj-j1pn-pinned-cycle-review-findings item 1: tag `regs`, the young
    /// words of a captured peer's REGISTER file that the caller also passes
    /// to [`Self::note_peer_band`] inside the band's words, as register words
    /// for [`Self::read_register_words`]. Neither completeness nor depth
    /// changes: past [`PEER_BAND_PIN_WORDS_CAP`] the rest stay untagged
    /// (tested as stack words, the rule before the tag).
    pub fn note_peer_band_registers(&self, regs: &[usize]) {
        let stamp = self.stamp();
        let mut g = self.peer_bands.lock();
        g.at(stamp);
        let room = PEER_BAND_PIN_WORDS_CAP.saturating_sub(g.regs.len());
        g.regs.extend(regs.iter().copied().take(room));
    }

    /// The current pause's band capture failed outright (gcd d3/m): a peer
    /// the pass had to read into the ledger could not be read at all.
    pub fn mark_peer_band_incomplete(&self) {
        let stamp = self.stamp();
        let mut g = self.peer_bands.lock();
        g.at(stamp);
        g.incomplete = true;
    }

    /// Helper windows whose band [`Self::note_peer_band`] recorded WHOLE, with
    /// a known depth, for the current pause.
    pub fn peer_band_windows(&self) -> u32 {
        let stamp = self.stamp();
        let g = self.peer_bands.lock();
        if g.stamp == stamp {
            g.windows
        } else {
            0
        }
    }

    /// A pause opens: every deposit made so far becomes stale.
    pub fn open_pause(&self) {
        self.orphans_at_open.store(
            YOUNG_PIN_ORPHAN_WRITES.load(Ordering::Acquire),
            Ordering::Release,
        );
        self.stamp.fetch_add(1, Ordering::AcqRel);
    }

    /// The current pause stamp.
    pub fn stamp(&self) -> u64 {
        self.stamp.load(Ordering::Acquire)
    }

    /// Hand out a new slot. Allocates (once per thread, at its first deposit).
    pub fn register(&self) -> std::sync::Arc<YoungPinSlot> {
        let slot = std::sync::Arc::new(YoungPinSlot::new());
        self.slots.lock().push(std::sync::Arc::clone(&slot));
        slot
    }

    /// Drop a slot (its owning thread is exiting).
    pub fn unregister(&self, slot: &std::sync::Arc<YoungPinSlot>) {
        self.slots
            .lock()
            .retain(|s| !std::sync::Arc::ptr_eq(s, slot));
    }

    /// Open a deposit into `slot` for the CURRENT pause. The slot reads as
    /// unpublished (stamp 0) until the deposit commits.
    pub fn begin_deposit(&self, slot: std::sync::Arc<YoungPinSlot>) -> YoungPinDeposit {
        let stamp = self.stamp();
        slot.stamp.store(0, Ordering::Relaxed);
        // The seqlock's writer half: the invalidation above is ordered before
        // every word store that follows.
        std::sync::atomic::fence(Ordering::Release);
        // gcj j1pn ledger item 7: a fresh dedup index (owner-only; no reader).
        for cell in slot.seen.iter() {
            cell.store(0, Ordering::Relaxed);
        }
        YoungPinDeposit {
            slot,
            stamp,
            len: 0,
            native: 0,
            regs: 0,
            incomplete: false,
            overflowed: false,
        }
    }

    /// Append every word deposited for the current pause to `out`, and say
    /// whether those deposits account for `jit_depth` JIT entries.
    ///
    /// Stale (earlier-stamp) and never-published slots contribute neither
    /// words nor depth. On an incomplete answer the appended words are a
    /// lower bound and must not be acted on.
    pub fn read(&self, jit_depth: usize, out: &mut Vec<usize>) -> YoungPinRead {
        self.read_into(jit_depth, out, None)
    }

    /// [`Self::read`], also appending to `layout_free` every word of `out`
    /// that did NOT come from a compiled frame's band alone: a deposit's
    /// layout-free-band and register words ([`YoungPinDeposit::note_native`]),
    /// the peer register words and the blocked-peer band words (a band read
    /// whole has no frame layout either).
    ///
    /// gcj-j1pn-pinned-cycle-review-findings item 1: the i35/wy start-word
    /// rule's argument ("the marking side already treats the word as no
    /// reference") is about compiled-band slots; these are the VM-frame and
    /// register holders the pinned plan exists for, and the plan counts them
    /// as rooted (`gen_heap::pinned_watched_start_rooted_set`).
    pub fn read_tagged(
        &self,
        jit_depth: usize,
        out: &mut Vec<usize>,
        layout_free: &mut Vec<usize>,
    ) -> YoungPinRead {
        self.read_into(jit_depth, out, Some(layout_free))
    }

    fn read_into(
        &self,
        jit_depth: usize,
        out: &mut Vec<usize>,
        mut layout_free: Option<&mut Vec<usize>>,
    ) -> YoungPinRead {
        let stamp = self.stamp();
        let start = out.len();
        let mut r = YoungPinRead {
            complete: true,
            ..YoungPinRead::default()
        };
        let slots = self.slots.lock();
        for s in slots.iter() {
            let s1 = s.stamp.load(Ordering::Acquire);
            if s1 != stamp {
                continue;
            }
            let incomplete = s.incomplete.load(Ordering::Relaxed);
            let depth = s.depth.load(Ordering::Relaxed);
            let n = s.len.load(Ordering::Relaxed).min(s.words.len());
            let native = s.native.load(Ordering::Relaxed).min(n);
            let mark = out.len();
            let lf_mark = layout_free.as_ref().map_or(0, |v| v.len());
            out.extend(s.words[..n].iter().map(|c| c.load(Ordering::Relaxed)));
            if let Some(lf) = layout_free.as_mut() {
                lf.extend(
                    s.words[..n]
                        .iter()
                        .zip(s.layout_free[..n].iter())
                        .filter(|(_, t)| t.load(Ordering::Relaxed))
                        .map(|(c, _)| c.load(Ordering::Relaxed)),
                );
            }
            // The seqlock's reader half: a deposit that restarted while this
            // slot was being copied has changed the stamp.
            std::sync::atomic::fence(Ordering::Acquire);
            if s.stamp.load(Ordering::Relaxed) != s1 {
                out.truncate(mark);
                if let Some(lf) = layout_free.as_mut() {
                    lf.truncate(lf_mark);
                }
                r.complete = false;
                r.deposit_incomplete = true;
                continue;
            }
            if incomplete {
                r.complete = false;
                r.deposit_incomplete = true;
            }
            r.deposits += 1;
            r.depth = r.depth.saturating_add(depth);
            r.native += native;
        }
        drop(slots);
        // gen r4w6/pinstale6: the peer register words captured for this pause.
        // They credit no depth (their threads' JIT entries, if any, must still
        // be accounted for by deposits), and an overflow is incompleteness.
        {
            let g = self.peer_regs.lock();
            if g.stamp == stamp {
                if g.overflow {
                    r.complete = false;
                    r.deposit_incomplete = true;
                }
                out.extend_from_slice(&g.words);
                if let Some(lf) = layout_free.as_mut() {
                    lf.extend_from_slice(&g.words);
                }
                r.peer_regs = g.words.len();
                r.native += g.words.len();
            }
        }
        // gcd d3/m: the blocked-peer bands the helper-window pass read for
        // this pause. They DO credit depth (a band read whole covers every JIT
        // frame on its thread), and any failure is incompleteness.
        {
            let g = self.peer_bands.lock();
            if g.stamp == stamp {
                if g.incomplete {
                    r.complete = false;
                    r.deposit_incomplete = true;
                }
                out.extend_from_slice(&g.words);
                if let Some(lf) = layout_free.as_mut() {
                    lf.extend_from_slice(&g.words);
                }
                r.peer_bands = g.words.len();
                r.native += g.words.len();
                r.depth = r.depth.saturating_add(g.depth);
            }
        }
        if r.depth < jit_depth {
            r.complete = false;
            r.depth_short = true;
        }
        r.words = out.len() - start;
        r
    }

    /// gcj-j1pn-pinned-cycle-review-findings item 1: append the current
    /// pause's REGISTER words to `out` -- every deposit's register image
    /// ([`YoungPinDeposit::note_register`]) and the peer register words the
    /// cross-thread scan captured ([`Self::note_peer_reg_word`]). A subset of
    /// what [`Self::read`] returns; no completeness verdict (the caller only
    /// widens a rooted set with it, the over-retaining direction). A slot torn
    /// by a restarted deposit contributes nothing.
    pub fn read_register_words(&self, out: &mut Vec<usize>) {
        let stamp = self.stamp();
        let slots = self.slots.lock();
        for s in slots.iter() {
            let s1 = s.stamp.load(Ordering::Acquire);
            if s1 != stamp {
                continue;
            }
            let n = s.len.load(Ordering::Relaxed).min(s.words.len());
            let regs = s.regs.load(Ordering::Relaxed).min(n);
            let mark = out.len();
            out.extend(s.words[..regs].iter().map(|c| c.load(Ordering::Relaxed)));
            std::sync::atomic::fence(Ordering::Acquire);
            if s.stamp.load(Ordering::Relaxed) != s1 {
                out.truncate(mark);
            }
        }
        drop(slots);
        let g = self.peer_regs.lock();
        if g.stamp == stamp {
            out.extend_from_slice(&g.words);
        }
        drop(g);
        let g = self.peer_bands.lock();
        if g.stamp == stamp {
            out.extend_from_slice(&g.regs);
        }
    }
}

/// The option-B census of [`young_pin_ledger_census`]: monotone counts over
/// the process's life, whichever VM's ledger each evaluation read. Split out
/// of [`YoungPinLedger`] when the ledger became per VM (gc-common w36-c): the
/// census is a process report, not per-pause state.
pub struct YoungPinCensus {
    counts: [AtomicU64; YOUNG_PIN_CENSUS_LEN],
    /// gcj j1/yg: the unproven-frame arm's ledger reads, by the first fact
    /// that refused each ([`unproven_refusal`]).
    unproven: [AtomicU64; unproven_refusal::LEN],
}

/// gcj j1/yg: slots of [`young_pin_unproven_read_census`]: every read, then
/// the first fact that refused a read.
pub mod unproven_refusal {
    /// Reads taken.
    pub const READS: usize = 0;
    /// A deposit (or peer capture) was incomplete or torn.
    pub const DEPOSIT: usize = 1;
    /// The deposits accounted for less JIT depth than the process holds.
    pub const DEPTH: usize = 2;
    /// An unbound thread wrote while the pause was open.
    pub const ORPHAN: usize = 3;
    /// The published young regions or bounds do not describe this heap.
    pub const BOUNDS: usize = 4;
    /// Raw JNI local references are open.
    pub const RAW_JNI: usize = 5;
    /// The take-over verdict does not admit the band ledger.
    pub const TAKEOVER: usize = 6;
    /// A coverage reason outside the accepted set (or none recorded).
    pub const COVERAGE: usize = 7;
    /// The blocked-peer stack capture is off or saturated.
    pub const PEER_STACKS: usize = 8;
    /// Deposits (any arm) incomplete because they overflowed the word cap.
    pub const DEPOSIT_OVERFLOW: usize = 9;
    /// Deposits (any arm) a scan marked incomplete.
    pub const DEPOSIT_MARKED: usize = 10;
    /// Slots.
    pub const LEN: usize = 11;
}

impl Default for YoungPinCensus {
    fn default() -> Self {
        Self::new()
    }
}

impl YoungPinCensus {
    /// Every count at zero.
    pub const fn new() -> Self {
        Self {
            counts: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
            unproven: [const { AtomicU64::new(0) }; unproven_refusal::LEN],
        }
    }

    /// One term-4 evaluation: its read, whether option B cleared it, and how
    /// many words the from-space screen dropped before the verdict.
    fn note(&self, r: YoungPinRead, cleared: bool, screened_out: usize) {
        let c = &self.counts;
        c[7].fetch_add(screened_out as u64, Ordering::Relaxed);
        c[0].fetch_add(1, Ordering::Relaxed);
        if r.complete {
            // Widening: usize word counts on a 64-bit (or narrower) target.
            let words = r.words as u64;
            if words == 0 {
                c[2].fetch_add(1, Ordering::Relaxed);
            }
            c[3].fetch_add(words, Ordering::Relaxed);
            c[4].fetch_max(words, Ordering::Relaxed);
            c[5].fetch_add(r.native as u64, Ordering::Relaxed);
        } else {
            c[1].fetch_add(1, Ordering::Relaxed);
        }
        if cleared {
            c[6].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// See [`young_pin_ledger_census`].
    pub fn snapshot(&self) -> [u64; YOUNG_PIN_CENSUS_LEN] {
        let c = &self.counts;
        [
            c[0].load(Ordering::Relaxed),
            c[1].load(Ordering::Relaxed),
            c[2].load(Ordering::Relaxed),
            c[3].load(Ordering::Relaxed),
            c[4].load(Ordering::Relaxed),
            c[5].load(Ordering::Relaxed),
            c[6].load(Ordering::Relaxed),
            c[7].load(Ordering::Relaxed),
        ]
    }
}

// # Per VM (gc-common w36-c, 2026-09-26)
//
// The ledger was the process static `YOUNG_PIN_LEDGER`, the last per-pause
// row of `docs/feature-designs/generative-gc-architecture.md`.
// Another VM's `request_stw` advanced its stamp (staling this VM's deposits:
// fail-closed), and another VM's deposits were read with this VM's (their
// depth could make up this VM's shortfall, and their words joined this VM's
// pin set). It is now the `young_pins` field of each VM's [`PauseLedger`],
// reached through the calling thread's binding, exactly like the coverage rows
// (see [`YoungPinOwner`]). Every writer and reader is bound in production:
//
// * the stamp: `reset_peer_proven_jit_depth`, run by the barrier's request
//   after it binds its ledger;
// * a parked peer's deposit: `publish_peer_jit_coverage_for_stw`, inside
//   `safepoint_check`'s `with_pause_ledger` (w8-d);
// * the initiator's deposit (`refresh_moving_young_coverage_for_collection`),
//   the peer register words (`record_peer_reg`, the take-over) and the read
//   (`gen_heap`'s term 4 and pinned copy): the requester, bound from its
//   winning request to `complete_gc`.
//
// An UNBOUND writer has no ledger to write: its deposit or word is dropped and
// counted ([`note_young_pin_orphan_write`]), and every ledger whose pause was
// open at the time then reads incomplete ([`YoungPinLedger::orphaned_since_open`]).
// An unbound READ is incomplete. So a writer this audit missed costs a divert,
// never a word. The completeness test still compares against the PROCESS-wide
// JIT depth (`active_depth_get`), as the peer-proof ledger does (w8-d): a
// per-VM ledger measured against the process depth can only read incomplete
// when another VM has compiled frames live, which is the fail-closed direction.
//
// Under `cfg(test)` an unbound thread uses its own leaked ledger instead
// (this module's convention: the gc unit tests run in parallel threads).

/// Young pin-word writes no VM's ledger could take, since the process started
/// (gc-common w36-c). See [`note_young_pin_orphan_write`].
static YOUNG_PIN_ORPHAN_WRITES: AtomicU64 = AtomicU64::new(0);

/// An unbound thread made a young pin-word deposit or noted a register word:
/// it is dropped, and this makes every ledger's open pause read incomplete
/// ([`YoungPinLedger::orphaned_since_open`]). Also counted in
/// [`coverage_orphan_writes`], the `orphan_coverage_writes=` field of the
/// `[stw-ttsp]` line, so one probe reading covers both kinds of orphan.
fn note_young_pin_orphan_write() {
    YOUNG_PIN_ORPHAN_WRITES.fetch_add(1, Ordering::AcqRel);
    COVERAGE_ORPHAN_WRITES.fetch_add(1, Ordering::Relaxed);
}

/// Young pin-word writes dropped because no ledger was bound (gc-common
/// w36-c). A production run should count 0; see [`note_young_pin_orphan_write`].
pub fn young_pin_orphan_writes() -> u64 {
    YOUNG_PIN_ORPHAN_WRITES.load(Ordering::Relaxed)
}

#[cfg(not(test))]
static YOUNG_PIN_CENSUS: YoungPinCensus = YoungPinCensus::new();

#[cfg(not(test))]
fn with_young_pin_census<R>(f: impl FnOnce(&YoungPinCensus) -> R) -> R {
    f(&YOUNG_PIN_CENSUS)
}

#[cfg(test)]
thread_local! {
    static TEST_YOUNG_PIN_CENSUS: YoungPinCensus = const { YoungPinCensus::new() };
    static TEST_YOUNG_PIN_LEDGER: &'static YoungPinLedger =
        Box::leak(Box::new(YoungPinLedger::new()));
}

#[cfg(test)]
fn with_young_pin_census<R>(f: impl FnOnce(&YoungPinCensus) -> R) -> R {
    TEST_YOUNG_PIN_CENSUS.with(|c| f(c))
}

/// The calling thread's own ledger, under `cfg(test)`, for an unbound thread.
#[cfg(test)]
fn young_pin_ledger() -> &'static YoungPinLedger {
    TEST_YOUNG_PIN_LEDGER.with(|l| *l)
}

/// The young pin-word ledger the calling thread writes and reads: its bound
/// [`PauseLedger`]'s (its VM's), else -- under `cfg(test)` only -- its own.
enum YoungPinOwner {
    Vm(std::sync::Arc<PauseLedger>),
    #[cfg(test)]
    Thread(&'static YoungPinLedger),
}

impl YoungPinOwner {
    fn ledger(&self) -> &YoungPinLedger {
        match self {
            Self::Vm(ledger) => &ledger.young_pins,
            #[cfg(test)]
            Self::Thread(ledger) => *ledger,
        }
    }
}

/// The calling thread's binding, if any.
fn bound_pause_ledger() -> Option<std::sync::Arc<PauseLedger>> {
    BOUND_PAUSE_LEDGER
        .try_with(|bound| bound.try_borrow().ok().and_then(|slot| slot.clone()))
        .ok()
        .flatten()
}

#[cfg(not(test))]
fn young_pin_owner() -> Option<YoungPinOwner> {
    bound_pause_ledger().map(YoungPinOwner::Vm)
}

#[cfg(test)]
fn young_pin_owner() -> Option<YoungPinOwner> {
    match bound_pause_ledger() {
        Some(ledger) => Some(YoungPinOwner::Vm(ledger)),
        None => TEST_YOUNG_PIN_LEDGER
            .try_with(|l| YoungPinOwner::Thread(*l))
            .ok(),
    }
}

/// The ledger a thread's slot is registered in. A VM's is held WEAKLY: a
/// thread's slot must not keep a dropped VM's pause ledger alive, and the weak
/// reference keeps the allocation, so a later VM's ledger cannot reuse its
/// address and be mistaken for it.
enum YoungPinSlotOwner {
    Vm(std::sync::Weak<PauseLedger>),
    #[cfg(test)]
    Thread(&'static YoungPinLedger),
}

/// Owns the calling thread's slot, and unregisters it when the thread exits
/// or moves to another VM's ledger.
struct YoungPinSlotHandle {
    owner: YoungPinSlotOwner,
    slot: std::sync::Arc<YoungPinSlot>,
}

impl YoungPinSlotHandle {
    fn new(owner: &YoungPinOwner) -> Self {
        let slot = owner.ledger().register();
        let owner = match owner {
            YoungPinOwner::Vm(ledger) => YoungPinSlotOwner::Vm(std::sync::Arc::downgrade(ledger)),
            #[cfg(test)]
            YoungPinOwner::Thread(ledger) => YoungPinSlotOwner::Thread(*ledger),
        };
        Self { owner, slot }
    }

    /// Whether this slot is registered in `owner`'s ledger.
    fn is_in(&self, owner: &YoungPinOwner) -> bool {
        match (&self.owner, owner) {
            (YoungPinSlotOwner::Vm(mine), YoungPinOwner::Vm(ledger)) => {
                std::ptr::eq(mine.as_ptr(), std::sync::Arc::as_ptr(ledger))
            }
            #[cfg(test)]
            (YoungPinSlotOwner::Thread(mine), YoungPinOwner::Thread(ledger)) => {
                std::ptr::eq(*mine, *ledger)
            }
            #[cfg(test)]
            _ => false,
        }
    }
}

impl Drop for YoungPinSlotHandle {
    fn drop(&mut self) {
        match &self.owner {
            YoungPinSlotOwner::Vm(ledger) => {
                if let Some(ledger) = ledger.upgrade() {
                    ledger.young_pins.unregister(&self.slot);
                }
            }
            #[cfg(test)]
            YoungPinSlotOwner::Thread(ledger) => ledger.unregister(&self.slot),
        }
    }
}

thread_local! {
    static SELF_YOUNG_PIN_SLOT: std::cell::RefCell<Option<YoungPinSlotHandle>> =
        const { std::cell::RefCell::new(None) };
}

/// A pause opens. Called from [`reset_peer_proven_jit_depth`], which the
/// barrier's `request_stw` runs under its lock before any peer can observe the
/// pause. NOT called from [`begin_moving_young_coverage_cycle`]: the ledger
/// has to survive that reset (and every other per-pause clear), which is the
/// point of stamping instead of clearing.
///
/// Stamps the calling thread's bound VM's ledger only (gc-common w36-c); an
/// unbound call stamps nothing, and that thread's read is incomplete anyway.
pub fn open_young_pin_pause() {
    if let Some(owner) = young_pin_owner() {
        owner.ledger().open_pause();
    }
}

/// The current pause stamp of the calling thread's bound VM's ledger
/// (diagnostics); 0 when unbound.
pub fn young_pin_pause_stamp() -> u64 {
    young_pin_owner().map_or(0, |owner| owner.ledger().stamp())
}

/// Callee-saved GPRs a [`CollectorEntry`] holds (the VM's capture stores
/// x86-64's 8 or AArch64's 11).
pub const COLLECTOR_ENTRY_GPRS: usize = 11;

/// gcj-j1pn-young-pin-ledger-review-findings item 3: where the calling
/// thread entered the collection pause door (`gc_and_alloc::run_collection_pause`),
/// captured by the VM's `conservative_roots::with_collector_entry_capture`
/// in a frame above every frame of the pause: the stack pointer there and the
/// callee-saved registers at that instant.
///
/// Every frame below `sp` is the pause machinery itself (the request, the root
/// gathering, the collector's own driver), which holds arena bases, ends and
/// tops -- all inside the ledger's young range -- and no word a moving cycle
/// must leave in place: the same frames run on every default moving cycle, on
/// which nothing sweeps them, so a raw reference they held across the
/// collection would be stale there already. The initiator's deposit (and a
/// losing initiator's park deposit) therefore sweeps `[sp, top)` and reads
/// the registers captured here, not its own: a mutator value a collector
/// frame's prologue spilled below `sp` is still in `regs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CollectorEntry {
    /// The stack pointer of the capturing frame, after its prologue.
    pub sp: usize,
    /// The callee-saved registers at the capture.
    pub regs: [usize; COLLECTOR_ENTRY_GPRS],
}

thread_local! {
    static COLLECTOR_ENTRY: std::cell::Cell<Option<CollectorEntry>> =
        const { std::cell::Cell::new(None) };
}

/// Holds this thread's [`CollectorEntry`] for the life of a pause door; nests
/// (a pause door entered from inside another restores the outer entry).
#[must_use = "the entry ends when the guard drops"]
pub struct CollectorEntryScope(Option<CollectorEntry>);

impl CollectorEntryScope {
    /// Publish `entry` (`None`: the target could not capture; the deposits
    /// then sweep as before) for this thread until the guard drops.
    pub fn enter(entry: Option<CollectorEntry>) -> Self {
        CollectorEntryScope(COLLECTOR_ENTRY.try_with(|c| c.replace(entry)).ok().flatten())
    }
}

impl Drop for CollectorEntryScope {
    fn drop(&mut self) {
        let prev = self.0;
        let _ = COLLECTOR_ENTRY.try_with(|c| c.set(prev));
    }
}

/// The calling thread's [`CollectorEntry`], when it is inside a pause door.
pub fn collector_entry() -> Option<CollectorEntry> {
    COLLECTOR_ENTRY.try_with(|c| c.get()).ok().flatten()
}

/// Initiator deposits that took the collector-entry cut, and the stack bytes
/// the cut left unswept (process-wide; the engagement census of gcj j1pn
/// ledger item 3, printed as `cjdiv_ledger_collector_cuts` /
/// `cjdiv_ledger_collector_cut_bytes`).
static YOUNG_PIN_COLLECTOR_CUTS: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// One initiator deposit took the collector-entry cut over `bytes` of stack.
pub fn note_young_pin_collector_cut(bytes: usize) {
    YOUNG_PIN_COLLECTOR_CUTS[0].fetch_add(1, Ordering::Relaxed);
    YOUNG_PIN_COLLECTOR_CUTS[1].fetch_add(bytes as u64, Ordering::Relaxed);
}

/// `(cuts, bytes)` so far; see [`note_young_pin_collector_cut`].
pub fn young_pin_collector_cut_census() -> (u64, u64) {
    (
        YOUNG_PIN_COLLECTOR_CUTS[0].load(Ordering::Relaxed),
        YOUNG_PIN_COLLECTOR_CUTS[1].load(Ordering::Relaxed),
    )
}

/// `CRATONVM_GEN_YOUNG_PIN_SKIPS_COLLECTOR_FRAMES` -- gcj j1pn ledger item 3,
/// default ON (`=0` kills: a deposit inside the pause door sweeps from its own
/// stack pointer and reads its own registers, the collector's frames
/// included). Generational only in effect (only its heap reads the ledger).
/// Read per deposit made inside a pause door.
pub fn young_pin_skips_collector_frames_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GEN_YOUNG_PIN_SKIPS_COLLECTOR_FRAMES")
}

/// The young range deposits screen words against — see [`YoungPinRange`].
pub fn young_pin_range() -> YoungPinRange {
    YoungPinRange::published()
}

/// gen r4w6/pinstale6: a peer REGISTER word the cross-thread scan read. If it
/// lies in a published young region it joins this pause's ledger
/// ([`YoungPinLedger::note_peer_reg_word`]); anything else is not the ledger's
/// business. Called from [`record_peer_reg`] for every register index.
pub fn note_peer_reg_pin_word(word: usize) {
    if young_pin_range().contains(word) {
        match young_pin_owner() {
            Some(owner) => owner.ledger().note_peer_reg_word(word),
            None => note_young_pin_orphan_write(),
        }
    }
}

/// gcj-j1pn-pinned-cycle-review-findings item 1: tag a captured peer's
/// REGISTER-file young words, which the caller also passes inside the band's
/// words to [`note_peer_band_pin_words`] (see
/// [`YoungPinLedger::note_peer_band_registers`]). Unbound: nothing (the
/// band's own write already fails the read closed).
pub fn note_peer_band_register_words(regs: &[usize]) {
    if let Some(owner) = young_pin_owner() {
        owner.ledger().note_peer_band_registers(regs);
    }
}

/// gcd d3/m: one blocked peer's band -- `words`, already screened to
/// [`young_pin_range`] by the caller -- into this pause's ledger, crediting
/// `depth` when the band was read `whole` (see
/// [`YoungPinLedger::note_peer_band`]). The helper-window pass calls it for
/// every helper window it pinned (under
/// [`pinned_young_copy_takeover_enabled`]) and for every blocked-monitor peer
/// credited by proof (`helper_window` false). Unbound: an orphan write, so the
/// pause's read fails closed.
pub fn note_peer_band_pin_words(
    words: &[usize],
    depth: Option<usize>,
    whole: bool,
    helper_window: bool,
) {
    match young_pin_owner() {
        Some(owner) => owner
            .ledger()
            .note_peer_band(words, depth, whole, helper_window),
        None => note_young_pin_orphan_write(),
    }
}

/// gcd d3/m: a peer whose band this pause's ledger needed could not be read at
/// all (see [`YoungPinLedger::mark_peer_band_incomplete`]).
pub fn mark_peer_band_capture_incomplete() {
    match young_pin_owner() {
        Some(owner) => owner.ledger().mark_peer_band_incomplete(),
        None => note_young_pin_orphan_write(),
    }
}

/// Open a deposit into the CALLING thread's slot for the current pause of its
/// bound VM's ledger, registering the slot on the thread's first deposit for
/// that ledger (one allocation; a thread that moves to another VM's pause
/// moves its slot). `None` when the thread-local is being torn down or
/// re-entered; the thread then simply does not deposit, which the reader
/// counts as a missing deposit. `None`, counted as an orphan write, when the
/// thread is bound to no VM's pause (gc-common w36-c; see
/// [`note_young_pin_orphan_write`]).
///
/// Must not be called from a signal or fault handler (a first call
/// allocates). The two production callers run at a safepoint park and in the
/// initiator's root gathering.
pub fn begin_self_young_pin_deposit() -> Option<YoungPinDeposit> {
    let Some(owner) = young_pin_owner() else {
        note_young_pin_orphan_write();
        return None;
    };
    let slot = SELF_YOUNG_PIN_SLOT
        .try_with(|c| {
            let mut held = c.try_borrow_mut().ok()?;
            if !held.as_ref().is_some_and(|handle| handle.is_in(&owner)) {
                // The thread's first deposit, or its first for this VM:
                // dropping the old handle unregisters its slot from the
                // ledger it was in.
                *held = Some(YoungPinSlotHandle::new(&owner));
            }
            held.as_ref()
                .map(|handle| std::sync::Arc::clone(&handle.slot))
        })
        .ok()
        .flatten()?;
    Some(owner.ledger().begin_deposit(slot))
}

/// The ledger read, plus the facts the ledger cannot see and must fail
/// closed on.
fn pause_young_pin_read(out: &mut Vec<usize>) -> YoungPinRead {
    pause_young_pin_read_tolerating(out, None, None)
}

/// gcj j1/yg item 1: [`pause_young_pin_words`] for the pinned copy's
/// unproven-frame arm (`gen_heap::PINNED_COPY_UNPROVEN_REASONS`, passed as
/// `accepted`). The same read and the same fail-closed facts, except two:
/// an unregistered compiled frame and an incomplete coverage proof do not
/// refuse the read when every reason this cycle recorded is in `accepted`
/// (and at least one was). Those frames' words are in the deposits: an
/// unregistered frame lies in its thread's layout-free sweep, an unpublished
/// oop is a word no map slot excuses, and a peer whose proof failed only for
/// accepted reasons deposits under the same switch
/// (`conservative_roots::publish_peer_jit_coverage_for_stw`) while one that
/// failed for any other reason deposits nothing and its JIT depth leaves the
/// read short. An unregistered frame flagged without its reason refuses.
pub fn pause_young_pin_words_for_unproven_frames(out: &mut Vec<usize>, accepted: usize) -> bool {
    pause_young_pin_read_tolerating(out, Some(accepted), None).complete
}

/// gcj-j1pn-pinned-cycle-review-findings item 1: [`pause_young_pin_words`]
/// (`accepted == None`) or [`pause_young_pin_words_for_unproven_frames`]
/// (`Some`), also appending to `layout_free` the subset of the words that
/// came from a layout-free band, a register image or a blocked-peer band
/// ([`YoungPinLedger::read_tagged`]). Same completeness answer.
pub fn pause_young_pin_words_tagged(
    out: &mut Vec<usize>,
    layout_free: &mut Vec<usize>,
    accepted: Option<usize>,
) -> bool {
    pause_young_pin_read_tolerating(out, accepted, Some(layout_free)).complete
}

/// [`pause_young_pin_read`], tolerating the coverage reasons in `accepted`
/// (see [`pause_young_pin_words_for_unproven_frames`]); `None` tolerates none.
/// `layout_free`: see [`pause_young_pin_words_tagged`].
fn pause_young_pin_read_tolerating(
    out: &mut Vec<usize>,
    accepted: Option<usize>,
    layout_free: Option<&mut Vec<usize>>,
) -> YoungPinRead {
    // gc-common w36-c: an unbound reader cannot name its pause's ledger; its
    // read is incomplete (`YoungPinRead::default()` is).
    let Some(owner) = young_pin_owner() else {
        return YoungPinRead::default();
    };
    let ledger = owner.ledger();
    let mut r = match layout_free {
        Some(lf) => ledger.read_tagged(active_depth_get(), out, lf),
        None => ledger.read(active_depth_get(), out),
    };
    // An unbound writer dropped a deposit or a register word while this
    // pause was open.
    if ledger.orphaned_since_open() {
        r.complete = false;
    }
    // * No published young region: the deposits' range screen was vacuous.
    // * A take-over froze or failed to read a peer: that peer's words were
    //   read by the initiator into pin sets, never into this ledger.
    // * An unregistered (guardless) compiled frame on the initiator, or any
    //   thread's coverage proof failing: frames the chain walk does not see.
    // * gen r4w6/pinstale6: the blocked-peer native-stack capture is off
    //   (`CRATONVM_GC_NO_BLOCKED_PEER_STACK_REMAP=1`) or refused a word. That
    //   capture is what rewrites a blocked peer's stack BASE words at wake and
    //   what the pinned copy pins the non-base ones from; without it whole,
    //   neither holds.
    //
    // gcd d3/m: except, under `CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER`, a
    // take-over whose only unrewritable peers are helper windows the pass
    // pinned whole AND read into this ledger as bands
    // ([`takeover_admits_band_ledger`]): their words are then this ledger's
    // words, and their depth is credited by the bands.
    let verdict = takeover_verdict();
    let takeover_read = verdict == TakeoverVerdict::NONE
        || (pinned_young_copy_takeover_enabled()
            && takeover_admits_band_ledger(verdict, ledger.peer_band_windows()));
    // gcd d4/m: two more, both "a relocating cycle here could move an object
    // a word the ledger cannot see still names":
    // * a thread of this VM holds raw JNI local references (see
    //   `RawJniLocalsScope`);
    // * more than one relocatable heap is live: `JIT_REGION_BOUNDS` (and so
    //   `YoungPinRange`, which every deposit and band screens words against)
    //   describes ONE heap, and a deposit screened against another heap's range
    //   drops this heap's young words while claiming completeness. Asked here
    //   directly rather than left to the coverage verifier's
    //   `BOUNDS_NOT_REPRESENTATIVE`, which a thread without compiled frames never
    //   runs and `CRATONVM_MOVING_YOUNG_NO_BOUNDS_GUARD` switches off. With two
    //   Generational VMs the pinned copy and option B therefore never run: a
    //   missed optimisation, never a wrong move.
    let bounds = !crate::gen_heap::published_young_regions_are_live()
        || !crate::gen_heap::published_bounds_represent_every_live_heap();
    let raw_jni = raw_jni_locals_open();
    let coverage = coverage_refuses_ledger_read(accepted);
    let peer_stacks = !blocked_peer_stack_remap_enabled() || peer_stack_slots_saturated();
    let orphaned = !r.complete && !r.deposit_incomplete && !r.depth_short;
    if bounds || raw_jni || !takeover_read || coverage || peer_stacks {
        r.complete = false;
    }
    // gcj j1/yg: the unproven-frame arm's reads, by the first refusing fact.
    if accepted.is_some() {
        use unproven_refusal as u;
        let slot = if r.deposit_incomplete {
            Some(u::DEPOSIT)
        } else if r.depth_short {
            Some(u::DEPTH)
        } else if orphaned {
            Some(u::ORPHAN)
        } else if bounds {
            Some(u::BOUNDS)
        } else if raw_jni {
            Some(u::RAW_JNI)
        } else if !takeover_read {
            Some(u::TAKEOVER)
        } else if coverage {
            Some(u::COVERAGE)
        } else if peer_stacks {
            Some(u::PEER_STACKS)
        } else {
            None
        };
        with_young_pin_census(|c| {
            c.unproven[u::READS].fetch_add(1, Ordering::Relaxed);
            if let Some(s) = slot {
                c.unproven[s].fetch_add(1, Ordering::Relaxed);
            }
        });
    }
    r
}

/// gcj j1/yg: the unproven-frame arm's ledger reads over the process's life,
/// indexed by [`unproven_refusal`].
pub fn young_pin_unproven_read_census() -> [u64; unproven_refusal::LEN] {
    with_young_pin_census(|c| std::array::from_fn(|i| c.unproven[i].load(Ordering::Relaxed)))
}

/// The coverage half of the ledger read's fail-closed facts: an unregistered
/// compiled frame or an incomplete coverage proof refuses the read, unless
/// `accepted` is given, at least one reason was recorded, every recorded one
/// is in it, and an unregistered frame (if flagged) carries its reason.
fn coverage_refuses_ledger_read(accepted: Option<usize>) -> bool {
    coverage_refuses_ledger_read_with(
        unregistered_jit_frame_on_stack(),
        moving_young_coverage_incomplete(),
        moving_young_incomplete_reason_mask(),
        accepted,
    )
}

/// [`coverage_refuses_ledger_read`] over its inputs. Pure.
fn coverage_refuses_ledger_read_with(
    unregistered: bool,
    incomplete: bool,
    mask: usize,
    accepted: Option<usize>,
) -> bool {
    let Some(accepted) = accepted else {
        return unregistered || incomplete;
    };
    if !unregistered && !incomplete {
        return false;
    }
    mask == 0
        || mask & !accepted != 0
        || (unregistered && mask & (1usize << incomplete_reason::UNREGISTERED_JIT_FRAME) == 0)
}

/// gcj j1/yg item 1: the coverage half of the ledger read. Without an
/// accepted mask either fact refuses (the read every arm but the
/// unproven-frame one takes); with one, only an unaccepted or missing reason
/// does.
#[cfg(test)]
#[test]
fn gcj_j1yg_the_unproven_read_tolerates_only_accepted_reasons() {
    use incomplete_reason as r;
    let unreg = 1usize << r::UNREGISTERED_JIT_FRAME;
    let accepted = unreg | (1usize << r::UNPUBLISHED_FRAME_OOP);
    assert!(!coverage_refuses_ledger_read_with(false, false, 0, None));
    assert!(coverage_refuses_ledger_read_with(true, false, unreg, None));
    assert!(coverage_refuses_ledger_read_with(false, true, unreg, None));
    assert!(!coverage_refuses_ledger_read_with(true, true, unreg, Some(accepted)));
    assert!(!coverage_refuses_ledger_read_with(false, false, 0, Some(accepted)));
    // A reason outside the accepted set, no reason at all, or an unregistered
    // frame flagged without its reason.
    let foreign = 1usize << r::FOREIGN_INNERMOST_RBP;
    assert!(coverage_refuses_ledger_read_with(true, true, unreg | foreign, Some(accepted)));
    assert!(coverage_refuses_ledger_read_with(false, true, 0, Some(accepted)));
    let unpub = 1usize << r::UNPUBLISHED_FRAME_OOP;
    assert!(coverage_refuses_ledger_read_with(true, true, unpub, Some(accepted)));
    assert!(!coverage_refuses_ledger_read_with(false, true, unpub, Some(accepted)));
}

/// Every conservative word published for the CURRENT pause that lies inside a published
/// young region and that no precise channel rewrites (interior and derived words included,
/// NOT resolved to object bases). Appends to `out`. Returns false when the ledger is
/// incomplete for this pause (a thread's scan did not deposit); the caller must then divert.
///
/// Sources: each cooperatively parked peer deposits at its park
/// (`conservative_roots::publish_peer_jit_coverage_for_stw`), the initiator
/// deposits during its root gathering
/// (`conservative_roots::refresh_moving_young_coverage_for_collection`). Each
/// deposit is the thread's callee-saved registers, every young word of the
/// native / layout-free part of its JIT band, and the young words of its
/// compiled frames that sit neither in a slot the active oop map names (the
/// slot `remap_one_jit_frame` rewrites) nor in a slot the compiler proves dead.
/// See `conservative_roots::deposit_pause_young_pin_words` for the exact rule.
///
/// On `false` the appended words are a lower bound. Duplicates across threads
/// are not removed.
pub fn pause_young_pin_words(out: &mut Vec<usize>) -> bool {
    pause_young_pin_read(out).complete
}

/// gcj-j1pn-pinned-cycle-review-findings item 1: the current pause's
/// REGISTER words in the young pin ledger -- the subset of
/// [`pause_young_pin_words`]' words that came from a depositing thread's
/// register image or a peer's captured register file
/// ([`YoungPinLedger::read_register_words`]), and the register files of the
/// captured peer bands ([`note_peer_band_register_words`]). Appends to `out`;
/// an unbound reader appends nothing.
pub fn pause_young_pin_register_words(out: &mut Vec<usize>) {
    if let Some(owner) = young_pin_owner() {
        owner.ledger().read_register_words(out);
    }
}

/// Option B's decision, pure: may term 4 stand down on this read?
#[inline]
fn young_pin_ledger_verdict(read: YoungPinRead, enabled: bool) -> bool {
    enabled && read.complete && read.words == 0
}

/// Term 4's option-B predicate (`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4`,
/// token `gen-young-pin-ledger-term4`): `true` means "this pause's ledger is
/// complete and empty, so the conservative-JIT-root divert need not fire".
///
/// Evaluated as the LAST conjunct of `unrewritable_conservative_jit_roots` in
/// `gen_heap::collect_garbage_inner`, so it runs exactly on the cycles term 4
/// would otherwise divert — and it records the census on every one of them,
/// flag on or off, so the census prices option B before anyone turns it on.
/// Allocates one `Vec` per such cycle (at the pause, never in a signal
/// context).
///
/// `from` (gcj-j1pn-young-pin-ledger-review-findings item 2) is the
/// collecting heap's from-space at the pause, `(base, used)`: a complete
/// read's words are screened to it ([`screen_young_pin_words_to_from_space`])
/// before "empty" is decided. `None` keeps the deposits' whole-range screen.
pub fn young_pin_ledger_clears_term4(from: Option<(usize, usize)>) -> bool {
    let mut words = Vec::new();
    let mut read = pause_young_pin_read(&mut words);
    let screened = match from {
        Some((base, used)) if read.complete && young_pin_from_space_screen_enabled() => {
            let dropped = screen_young_pin_words_to_from_space(&mut words, base, used);
            read.words = words.len();
            dropped
        }
        _ => 0,
    };
    let cleared = young_pin_ledger_verdict(read, gc_flags().gen_young_pin_ledger_term4);
    with_young_pin_census(|census| census.note(read, cleared, screened));
    cleared
}

/// gcj-j1pn-young-pin-ledger-review-findings item 2: does the ledger word
/// `w` name from-space, `[base - YOUNG_PIN_LOW_SLACK, base + used]` (the
/// deposits' low slack and inclusive one-past-end, applied to the ALLOCATED
/// part)? Pure.
///
/// The deposits screen against [`YoungPinRange::published`], i.e. BOTH
/// semispaces' whole capacity (`JIT_REGION_BOUNDS`, which is also the JIT's
/// inline young-containment table, so it cannot be narrowed at the source).
/// At a pause to-space holds no young object (the pinned in-place cycle
/// copies inside from-space; often it is not even committed), and from-space
/// above its bump top holds none either, so a word there names nothing a
/// cycle moves -- yet one such stale stack word blocked option B's clear and
/// was charged to the pinned copy's census and 1/8 bound.
#[inline]
pub fn young_pin_word_in_from_space(w: usize, base: usize, used: usize) -> bool {
    base != 0
        && used != 0
        && w >= base.saturating_sub(YOUNG_PIN_LOW_SLACK)
        && w <= base.saturating_add(used)
}

/// Keep only the words of `words` that name from-space
/// ([`young_pin_word_in_from_space`]); returns how many were dropped.
pub fn screen_young_pin_words_to_from_space(
    words: &mut Vec<usize>,
    base: usize,
    used: usize,
) -> usize {
    let before = words.len();
    words.retain(|&w| young_pin_word_in_from_space(w, base, used));
    before - words.len()
}

/// `CRATONVM_GEN_YOUNG_PIN_FROM_SPACE_SCREEN` -- gcj j1pn ledger item 2,
/// default ON (`=0` kills: a complete read counts every word in either
/// semispace's capacity, as before). Generational only (the ledger's only
/// reader). Read once per term-4 evaluation and per pinned-copy read.
pub fn young_pin_from_space_screen_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GEN_YOUNG_PIN_FROM_SPACE_SCREEN")
}

/// The option-B census over the process's life, printed on the shutdown
/// `[GC] young_conservative_divert:` line as `cjdiv_ledger_*`:
/// `[evals, incomplete, complete_empty, words_sum, words_max, native_sum, cleared,
/// screened_out_sum]`.
///
/// * `evals` — cycles term 4 would have diverted (with the flag off this
///   equals `cjdiv_diverts`; with it on, `cjdiv_diverts = evals - cleared`);
/// * `incomplete` — of those, cycles whose ledger was incomplete (some thread
///   in compiled code did not deposit, a deposit overflowed, a take-over ran);
/// * `complete_empty` — cycles option B alone would unlock: the prize;
/// * `words_sum` / `words_max` — ledger words per COMPLETE cycle (mean =
///   `words_sum / (evals - incomplete)`);
/// * `native_sum` — the subset read from registers and layout-free bands,
///   counted BEFORE the from-space screen (the read does not say which words
///   were native), so it can exceed `words_sum`;
/// * `cleared` — cycles on which option B actually stood term 4 down;
/// * `screened_out_sum` — words of complete reads the from-space screen
///   dropped before the verdict (gcj j1pn ledger item 2: to-space words and
///   words above from-space's top; `words_sum` no longer counts them).
pub fn young_pin_ledger_census() -> [u64; YOUNG_PIN_CENSUS_LEN] {
    with_young_pin_census(YoungPinCensus::snapshot)
}

#[cfg(test)]
mod young_pin_ledger_tests {
    use super::*;

    const BASE: usize = 0x7000_0000;
    const END: usize = 0x7010_0000;

    fn deposit(
        l: &YoungPinLedger,
        slot: &std::sync::Arc<YoungPinSlot>,
        words: &[usize],
        depth: usize,
    ) {
        let mut d = l.begin_deposit(std::sync::Arc::clone(slot));
        for &w in words {
            d.note(w);
        }
        d.commit(depth);
    }

    /// gcj-j1pn-pinned-cycle-review-findings item 1: `read_tagged` names
    /// every layout-free / register word (a word deposited from a compiled
    /// band first and a register later counts as layout-free), every peer
    /// register word, and no compiled-band-only word; a torn slot's tags are
    /// dropped with its words.
    #[test]
    fn gcj1pn_cycle_item1_read_tagged_names_layout_free_words() {
        let l = YoungPinLedger::new();
        l.open_pause();
        let slot = l.register();
        let mut d = l.begin_deposit(std::sync::Arc::clone(&slot));
        d.note(BASE + 0x10); // compiled band only
        d.note_native(BASE + 0x20); // layout-free band
        d.note(BASE + 0x30);
        d.note_native(BASE + 0x30); // compiled, then a register
        d.note_native(BASE + 0x20); // a repeat changes nothing
        d.commit(1);
        l.note_peer_reg_word(BASE + 0x40);
        let (mut out, mut lf) = (Vec::new(), Vec::new());
        let r = l.read_tagged(1, &mut out, &mut lf);
        assert!(r.complete);
        out.sort_unstable();
        lf.sort_unstable();
        assert_eq!(out, vec![BASE + 0x10, BASE + 0x20, BASE + 0x30, BASE + 0x40]);
        assert_eq!(lf, vec![BASE + 0x20, BASE + 0x30, BASE + 0x40]);
        assert_eq!(r.native, 3, "the upgraded word is counted once as native");
        // The untagged read is the same words.
        let mut plain = Vec::new();
        assert_eq!(l.read(1, &mut plain).words, out.len());
        // A fresh deposit forgets the previous pause's tags.
        l.open_pause();
        deposit(&l, &slot, &[BASE + 0x20], 1);
        let (mut out, mut lf) = (Vec::new(), Vec::new());
        assert!(l.read_tagged(1, &mut out, &mut lf).complete);
        assert_eq!(out, vec![BASE + 0x20]);
        assert!(lf.is_empty(), "compiled-band word in a reused cell: {lf:x?}");
    }

    /// THE prerequisite. A peer deposits for pause N; the initiator then runs
    /// every per-pause reset it runs AFTER the peers parked — the coverage
    /// cycle open and the root gatherer's clears — and the deposit is still
    /// read as this pause's. Only the NEXT pause's opening makes it stale.
    #[test]
    fn the_ledger_survives_the_initiators_reset() {
        // A pause opens (the barrier's `request_stw`).
        reset_peer_proven_jit_depth();
        let l = young_pin_ledger();
        let peer = l.register();
        let initiator = l.register();
        deposit(l, &peer, &[BASE + 0x40], 2);

        // The initiator's resets.
        begin_moving_young_coverage_cycle();
        clear_pinned_jit_roots();
        clear_movable_jit_roots();
        clear_unrewritable_jit_roots();
        clear_unregistered_jit_frame_on_stack();
        deposit(l, &initiator, &[], 1);

        let mut out = Vec::new();
        let r = l.read(3, &mut out);
        assert!(
            r.complete,
            "a peer deposit must survive the initiator's resets: {r:?}"
        );
        assert_eq!(out, vec![BASE + 0x40]);
        assert_eq!(r.deposits, 2);

        // The next pause opens: both deposits are now stale.
        reset_peer_proven_jit_depth();
        let mut out = Vec::new();
        let r = l.read(3, &mut out);
        assert!(
            !r.complete,
            "last pause's deposits must not satisfy this one"
        );
        assert!(out.is_empty());
    }

    /// gcj-j1pn-pinned-cycle-review-findings item 1: the register words a
    /// deposit noted first, and the peer register words, read back as the
    /// pause's register subset; a register word noted after a stack word, a
    /// duplicate, and a stale pause's registers are not in it.
    #[test]
    fn gcj1pn_register_words_are_read_back_as_their_own_subset() {
        let l = YoungPinLedger::new();
        l.open_pause();
        let slot = l.register();
        let mut d = l.begin_deposit(std::sync::Arc::clone(&slot));
        d.note_register(BASE + 0x10);
        d.note_register(BASE + 0x10);
        d.note_register(BASE + 0x20);
        d.note(BASE + 0x30);
        d.note_register(BASE + 0x40);
        d.note_native(BASE + 0x50);
        d.commit(1);
        l.note_peer_reg_word(BASE + 0x60);
        l.note_peer_band(&[BASE + 0x70, BASE + 0x80], Some(0), true, false);
        l.note_peer_band_registers(&[BASE + 0x70]);
        let mut regs = Vec::new();
        l.read_register_words(&mut regs);
        assert_eq!(
            regs,
            vec![BASE + 0x10, BASE + 0x20, BASE + 0x60, BASE + 0x70]
        );
        let mut all = Vec::new();
        let r = l.read(1, &mut all);
        assert_eq!(all.len(), 8, "{all:x?}");
        assert_eq!(r.native, 7, "registers count as layout-free words: {r:?}");
        // The next pause: nothing is this pause's.
        l.open_pause();
        let mut regs = Vec::new();
        l.read_register_words(&mut regs);
        assert!(regs.is_empty());
    }

    /// A thread inside compiled code that did not deposit leaves a shortfall.
    #[test]
    fn a_missing_peer_deposit_returns_false() {
        let l = YoungPinLedger::new();
        l.open_pause();
        let initiator = l.register();
        let peer = l.register();
        deposit(&l, &initiator, &[], 1);
        let mut out = Vec::new();
        assert!(
            !l.read(3, &mut out).complete,
            "depth 3 in the process, 1 deposited: incomplete"
        );
        // Registered is not deposited: the peer's slot is still at stamp 0.
        assert!(out.is_empty());
        deposit(&l, &peer, &[], 2);
        let mut out = Vec::new();
        let r = l.read(3, &mut out);
        assert!(
            r.complete && r.words == 0,
            "every entry accounted, nothing young: {r:?}"
        );
    }

    /// A deposit from an earlier pause contributes neither words nor depth.
    #[test]
    fn a_stale_stamp_deposit_is_ignored() {
        let l = YoungPinLedger::new();
        let peer = l.register();
        deposit(&l, &peer, &[BASE + 8, BASE + 16], 2);
        l.open_pause();
        let mut out = Vec::new();
        let r = l.read(2, &mut out);
        assert!(!r.complete, "a stale deposit must not account for depth");
        assert!(
            out.is_empty(),
            "a stale deposit's words must not be reported"
        );
        // With no JIT depth left to account for, the stale words are still not
        // read back as this pause's.
        let mut out = Vec::new();
        let r = l.read(0, &mut out);
        assert!(r.complete && out.is_empty(), "{r:?} {out:?}");
        // An unfinished deposit (begun, never committed) reads as unpublished.
        let open = l.begin_deposit(std::sync::Arc::clone(&peer));
        drop(open);
        let mut out = Vec::new();
        assert!(!l.read(1, &mut out).complete);
    }

    /// Words are stored RAW: an interior word is reported as itself, never as
    /// the base of the object it points into. And the range keeps the
    /// one-past-end and low-slack words the design asks for.
    #[test]
    fn an_interior_word_is_kept_raw() {
        let range = YoungPinRange::from_regions([(BASE, END), (0, 0)]);
        let interior = BASE + 0x1000 + 24;
        for w in [BASE, interior, END, BASE - YOUNG_PIN_LOW_SLACK] {
            assert!(range.contains(w), "0x{w:x} must count as young");
        }
        for w in [END + 8, BASE - YOUNG_PIN_LOW_SLACK - 8, 0, 8] {
            assert!(!range.contains(w), "0x{w:x} must not count as young");
        }
        assert!(YoungPinRange::from_regions([(0, 0), (0, 0)]).is_empty());
        // A header read is only licensed inside the region proper: the
        // one-past-end and low-slack words are young, but not readable.
        assert!(range.contains_span(BASE, 16));
        assert!(range.contains_span(END - 16, 16));
        for w in [END, END - 8, BASE - 8, BASE - YOUNG_PIN_LOW_SLACK] {
            assert!(!range.contains_span(w, 16), "0x{w:x} must not be read");
        }
        assert!(!range.contains_span(usize::MAX - 4, 16));

        let l = YoungPinLedger::new();
        let t = l.register();
        deposit(&l, &t, &[interior, interior], 1);
        let mut out = Vec::new();
        let r = l.read(1, &mut out);
        assert!(r.complete);
        assert_eq!(
            out,
            vec![interior],
            "raw word, deduplicated, not resolved to 0x{:x}",
            BASE + 0x1000
        );
    }

    /// Overflow is incompleteness, not truncation.
    #[test]
    fn overflow_returns_false() {
        let l = YoungPinLedger::new();
        let t = l.register();
        let full: Vec<usize> = (0..YOUNG_PIN_WORDS_PER_THREAD)
            .map(|i| BASE + 8 * i)
            .collect();
        deposit(&l, &t, &full, 1);
        let mut out = Vec::new();
        let r = l.read(1, &mut out);
        assert!(r.complete, "exactly the cap is complete");
        assert_eq!(out.len(), YOUNG_PIN_WORDS_PER_THREAD);

        let mut over = full.clone();
        over.push(END);
        deposit(&l, &t, &over, 1);
        let mut out = Vec::new();
        assert!(
            !l.read(1, &mut out).complete,
            "one word past the cap must read incomplete"
        );

        // Duplicates do not consume the cap.
        let mut dups = full;
        dups.extend_from_slice(&[BASE, BASE + 8]);
        deposit(&l, &t, &dups, 1);
        let mut out = Vec::new();
        assert!(l.read(1, &mut out).complete);

        // A scan that could not read its whole band is incomplete too.
        let mut d = l.begin_deposit(std::sync::Arc::clone(&t));
        d.mark_incomplete();
        d.commit(1);
        let mut out = Vec::new();
        assert!(!l.read(1, &mut out).complete);
    }

    /// A departed thread's slot is gone, so it can neither satisfy nor spoil
    /// a later read.
    #[test]
    fn an_unregistered_slot_is_not_read() {
        let l = YoungPinLedger::new();
        let t = l.register();
        deposit(&l, &t, &[BASE], 1);
        l.unregister(&t);
        let mut out = Vec::new();
        let r = l.read(0, &mut out);
        assert!(r.complete && out.is_empty() && r.deposits == 0, "{r:?}");
    }

    /// Option B's rule and its census: it clears only when enabled AND
    /// complete AND empty, and the census counts every evaluation either way.
    #[test]
    fn the_term4_verdict_and_its_census() {
        let empty = YoungPinRead {
            complete: true,
            ..YoungPinRead::default()
        };
        let words = YoungPinRead {
            complete: true,
            words: 3,
            native: 1,
            deposits: 2,
            depth: 2,
            peer_regs: 0,
            peer_bands: 0,
            deposit_incomplete: false,
            depth_short: false,
        };
        let partial = YoungPinRead {
            complete: false,
            ..YoungPinRead::default()
        };
        assert!(young_pin_ledger_verdict(empty, true));
        assert!(
            !young_pin_ledger_verdict(empty, false),
            "the flag is opt-in"
        );
        assert!(
            !young_pin_ledger_verdict(words, true),
            "a young word keeps the divert"
        );
        assert!(
            !young_pin_ledger_verdict(partial, true),
            "incomplete keeps the divert"
        );

        let c = YoungPinCensus::new();
        c.note(empty, true, 0);
        c.note(words, false, 2);
        c.note(partial, false, 0);
        assert_eq!(c.snapshot(), [3, 1, 1, 3, 3, 1, 1, 2]);
    }

    /// gcj-j1pn-young-pin-ledger-review-findings item 7: the hash-indexed
    /// dedup keeps exactly the distinct words, in first-seen order, across
    /// colliding home cells; a second deposit into the same slot starts from
    /// an empty index; the 257th distinct word still marks the deposit
    /// incomplete.
    #[test]
    fn gcj1pn_the_deposit_dedup_keeps_each_distinct_word_once() {
        let l = YoungPinLedger::new();
        let slot = l.register();
        // Words 4 KiB apart share low bits; a run of them stresses probing.
        let distinct: Vec<usize> = (0..200).map(|i| BASE + i * 0x1000).collect();
        let mut d = l.begin_deposit(std::sync::Arc::clone(&slot));
        for round in 0..3 {
            for &w in &distinct {
                d.note(w);
            }
            assert_eq!(d.len(), distinct.len(), "round {round}");
        }
        d.commit(1);
        let mut out = Vec::new();
        let r = l.read(1, &mut out);
        assert!(r.complete);
        assert_eq!(out, distinct);

        // A fresh deposit: the old index must not hide its words.
        let mut d = l.begin_deposit(std::sync::Arc::clone(&slot));
        d.note(distinct[7]);
        assert_eq!(d.len(), 1);
        for i in 0..YOUNG_PIN_WORDS_PER_THREAD {
            d.note(END + i * 8);
        }
        assert!(d.is_incomplete(), "past the cap");
        d.commit(1);
        let mut out = Vec::new();
        assert!(!l.read(1, &mut out).complete);
    }

    /// gcj-j1pn-young-pin-ledger-review-findings item 2: the from-space screen
    /// keeps the allocated part of from-space with the deposits' low slack and
    /// inclusive one-past-end, and drops to-space and everything above the
    /// bump top; an empty or unpublished from-space keeps nothing.
    #[test]
    fn gcj1pn_the_from_space_screen_keeps_only_allocated_from_space() {
        const USED: usize = 0x4000;
        let to = END + 0x10_0000;
        for w in [BASE, BASE + 8, BASE + USED, BASE - YOUNG_PIN_LOW_SLACK] {
            assert!(young_pin_word_in_from_space(w, BASE, USED), "{w:#x} kept");
        }
        for w in [
            BASE + USED + 8,
            END,
            to,
            to + 0x40,
            BASE - YOUNG_PIN_LOW_SLACK - 8,
            0,
        ] {
            assert!(!young_pin_word_in_from_space(w, BASE, USED), "{w:#x} dropped");
        }
        assert!(!young_pin_word_in_from_space(BASE, BASE, 0), "empty from-space");
        assert!(!young_pin_word_in_from_space(8, 0, USED), "no from-space");
        let mut words = vec![BASE + 0x40, to, END, BASE + USED, to + 8];
        assert_eq!(screen_young_pin_words_to_from_space(&mut words, BASE, USED), 3);
        assert_eq!(words, vec![BASE + 0x40, BASE + USED]);
        let on = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GEN_YOUNG_PIN_FROM_SPACE_SCREEN", None)],
            young_pin_from_space_screen_enabled,
        );
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GEN_YOUNG_PIN_FROM_SPACE_SCREEN", Some("0"))],
            young_pin_from_space_screen_enabled,
        );
        assert!(on && !off, "default ON, `=0` kills");
    }

    /// The production predicate records one evaluation per call and, with the
    /// flag unset, never clears.
    #[test]
    fn the_term4_helper_is_opt_in_and_counted() {
        let before = young_pin_ledger_census();
        assert!(!young_pin_ledger_clears_term4(None) || gc_flags().gen_young_pin_ledger_term4);
        let after = young_pin_ledger_census();
        assert_eq!(after[0], before[0] + 1);
    }

    /// gen r4w6/pinstale6: a peer REGISTER word the cross-thread scan captured
    /// is read back with the pause's deposits, raw and deduplicated, credits no
    /// JIT depth, and goes stale when the next pause opens.
    #[test]
    fn peer_register_words_join_the_pause_and_go_stale_with_it() {
        let l = YoungPinLedger::new();
        let t = l.register();
        deposit(&l, &t, &[BASE + 8], 1);
        let interior = BASE + 0x2000 + 24;
        l.note_peer_reg_word(interior);
        l.note_peer_reg_word(interior);
        l.note_peer_reg_word(BASE + 0x3000);
        let mut out = Vec::new();
        let r = l.read(1, &mut out);
        assert!(r.complete, "{r:?}");
        assert_eq!(r.peer_regs, 2, "deduplicated: {r:?}");
        assert_eq!(r.words, 3, "{r:?}");
        assert_eq!(r.native, 2, "register words are layout-free words: {r:?}");
        assert_eq!(out, vec![BASE + 8, interior, BASE + 0x3000]);

        // Register words credit no depth: a JIT entry nobody deposited for
        // still leaves the ledger incomplete.
        l.open_pause();
        l.note_peer_reg_word(BASE + 0x40);
        let mut out = Vec::new();
        assert!(!l.read(1, &mut out).complete);

        // The next pause opens: last pause's register words are not read.
        l.open_pause();
        let mut out = Vec::new();
        let r = l.read(0, &mut out);
        assert!(
            r.complete && out.is_empty() && r.peer_regs == 0,
            "{r:?} {out:?}"
        );
    }

    /// More distinct register words than the cap is incompleteness for that
    /// pause only; duplicates never consume the cap.
    #[test]
    fn a_register_word_overflow_is_incompleteness() {
        let l = YoungPinLedger::new();
        for i in 0..PEER_REG_PIN_WORDS_CAP {
            l.note_peer_reg_word(BASE + 8 * i);
        }
        let mut out = Vec::new();
        assert!(l.read(0, &mut out).complete, "exactly the cap is complete");
        assert_eq!(out.len(), PEER_REG_PIN_WORDS_CAP);
        l.note_peer_reg_word(BASE);
        assert!(
            l.read(0, &mut Vec::new()).complete,
            "a duplicate is not an overflow"
        );
        l.note_peer_reg_word(END);
        assert!(
            !l.read(0, &mut Vec::new()).complete,
            "one distinct word past the cap must read incomplete"
        );
        l.open_pause();
        assert!(
            l.read(0, &mut Vec::new()).complete,
            "the overflow was that pause's"
        );
    }

    /// gci i1/pn: the register words are kept sorted (the dedup is a binary
    /// search now). Words noted in any order, with repeats, read back as the
    /// same distinct set, ascending; a repeat at the cap is still not an
    /// overflow.
    #[test]
    fn gci_i1pn_peer_register_words_dedup_in_any_order() {
        let l = YoungPinLedger::new();
        for &w in &[BASE + 0x300, BASE + 0x100, BASE + 0x300, BASE + 0x200, BASE + 0x100] {
            l.note_peer_reg_word(w);
        }
        let mut out = Vec::new();
        let r = l.read(0, &mut out);
        assert!(r.complete, "{r:?}");
        assert_eq!(r.peer_regs, 3, "{r:?}");
        assert_eq!(out, vec![BASE + 0x100, BASE + 0x200, BASE + 0x300]);

        // Fill to the cap in DESCENDING order, then repeat the lowest and the
        // highest: still complete. One new word: incomplete.
        l.open_pause();
        for i in (0..PEER_REG_PIN_WORDS_CAP).rev() {
            l.note_peer_reg_word(BASE + 8 * i);
        }
        l.note_peer_reg_word(BASE);
        l.note_peer_reg_word(BASE + 8 * (PEER_REG_PIN_WORDS_CAP - 1));
        let mut out = Vec::new();
        assert!(l.read(0, &mut out).complete, "repeats at the cap are not an overflow");
        assert_eq!(out.len(), PEER_REG_PIN_WORDS_CAP);
        assert!(out.windows(2).all(|w| w[0] < w[1]), "sorted and distinct");
        l.note_peer_reg_word(END);
        assert!(!l.read(0, &mut Vec::new()).complete, "one distinct word past the cap");
    }

    /// Option B must not stand term 4 down over a young word a peer holds in a
    /// register: the word is in the ledger, so the read is not empty.
    #[test]
    fn option_b_keeps_the_divert_over_a_peer_register_word() {
        let l = YoungPinLedger::new();
        l.note_peer_reg_word(BASE + 16);
        let r = l.read(0, &mut Vec::new());
        assert!(r.complete && r.words == 1, "{r:?}");
        assert!(!young_pin_ledger_verdict(r, true));
    }

    /// Deposit `words` at `depth` through the production door, on the calling
    /// thread, into whatever ledger it is bound to.
    fn deposit_self(words: &[usize], depth: usize) {
        let mut d = begin_self_young_pin_deposit().expect("a bound or test thread has a ledger");
        for &w in words {
            d.note(w);
        }
        d.commit(depth);
    }

    /// gc-common w36-c: the ledger is per VM. Two VMs' pauses overlap (the
    /// coverage slot's give-up allows it): B's open does not stale A's
    /// deposit, B's deposit is not read by A (neither its words nor its
    /// depth), and a thread's slot follows the VM it deposits for.
    #[test]
    fn the_young_pin_ledger_is_the_bound_vms_only() {
        let a = std::sync::Arc::new(PauseLedger::new());
        let b = std::sync::Arc::new(PauseLedger::new());

        // A's pause opens; a thread of A deposits one young word, depth 1.
        with_pause_ledger(&a, reset_peer_proven_jit_depth);
        let a_stamp = a.young_pin_stamp();
        assert_eq!(with_pause_ledger(&a, young_pin_pause_stamp), a_stamp);
        with_pause_ledger(&a, || deposit_self(&[BASE + 8], 1));

        // B's pause opens and a thread of B deposits, inside A's pause.
        let b_thread = std::sync::Arc::clone(&b);
        std::thread::spawn(move || {
            let b = b_thread;
            with_pause_ledger(&b, reset_peer_proven_jit_depth);
            with_pause_ledger(&b, || deposit_self(&[BASE + 16], 2));
            let mut out = Vec::new();
            let r = b.young_pins.read(2, &mut out);
            assert!(r.complete, "B's own deposit accounts for B: {r:?}");
            assert_eq!(out, vec![BASE + 16], "B reads only B's words");
        })
        .join()
        .expect("B's thread");
        // The thread has exited (a native join waits for its thread-local
        // destructors): its slot left B's ledger.
        assert!(b.young_pins.slots.lock().is_empty());

        assert_eq!(a.young_pin_stamp(), a_stamp, "B's open moved A's stamp");
        let mut out = Vec::new();
        let r = a.young_pins.read(1, &mut out);
        assert!(r.complete, "A's deposit survives B's pause: {r:?}");
        assert_eq!(out, vec![BASE + 8], "A reads only A's words");
        let mut out = Vec::new();
        assert!(
            !a.young_pins.read(3, &mut out).complete,
            "B's depth does not make up A's shortfall"
        );

        // The same thread now deposits for B: its slot moves to B's ledger.
        with_pause_ledger(&b, || deposit_self(&[BASE + 24], 0));
        assert!(
            a.young_pins.slots.lock().is_empty(),
            "A's slot was released"
        );
        assert_eq!(b.young_pins.slots.lock().len(), 1);

        // An unbound thread (under `cfg(test)`) uses its own ledger, which
        // neither VM reads.
        deposit_self(&[BASE + 32], 0);
        assert_eq!(
            b.young_pins.slots.lock().len(),
            0,
            "the slot followed the thread"
        );
        let mut out = Vec::new();
        assert!(b.young_pins.read(0, &mut out).complete && out.is_empty());
        assert!(a.young_pins.slots.lock().is_empty());
    }

    /// An orphan write (an unbound deposit or register word in production)
    /// fails the read of every ledger whose pause was open at the time, until
    /// that ledger's next open.
    #[test]
    fn an_orphan_write_fails_the_open_pauses_read() {
        let l = YoungPinLedger::new();
        l.open_pause();
        assert!(!l.orphaned_since_open());
        let before = young_pin_orphan_writes();
        let coverage_before = coverage_orphan_writes();
        note_young_pin_orphan_write();
        assert!(young_pin_orphan_writes() > before);
        assert!(coverage_orphan_writes() > coverage_before);
        assert!(l.orphaned_since_open());
        l.open_pause();
        // Another test's orphan may land between these two lines; retry the
        // open until none does (bounded: orphans are rare in this binary).
        let mut clean = !l.orphaned_since_open();
        for _ in 0..100 {
            if clean {
                break;
            }
            l.open_pause();
            clean = !l.orphaned_since_open();
        }
        assert!(clean, "the next open clears the verdict");
        // A ledger never opened since an orphan fails closed too.
        let fresh = YoungPinLedger::new();
        assert!(fresh.orphaned_since_open());
    }

    /// `record_peer_reg`'s index split: registers feed the ledger, the two
    /// stack populations do not.
    #[test]
    fn only_register_indices_are_registers() {
        for reg in [0u8, 3, 15, 16] {
            assert!(peer_reg_is_register(reg), "r{reg}");
        }
        assert!(!peer_reg_is_register(PEER_REG_STACK_TAKEOVER));
        assert!(!peer_reg_is_register(PEER_REG_STACK_HELPER));
    }
}

/// gcd d3/m: the blocked-peer band rows of the young pin ledger, the take-over
/// admission rules the pinned copy's take-over arm reads, and the proven
/// blocked-monitor stash.
#[cfg(test)]
mod gcd_d3m_band_ledger_tests {
    use super::*;

    const BASE: usize = 0x7000_0000;

    /// A band read whole credits its depth, and its young words are read back
    /// raw, deduplicated, with the pause's other words.
    #[test]
    fn a_whole_band_credits_depth_and_its_words_are_read() {
        let l = YoungPinLedger::new();
        l.note_peer_band(&[BASE + 16, BASE + 8, BASE + 16], Some(2), true, true);
        l.note_peer_band(&[BASE + 8, BASE + 0x2000 + 3], Some(1), true, false);
        let mut out = Vec::new();
        let r = l.read(3, &mut out);
        assert!(r.complete, "two bands account for depth 3: {r:?}");
        assert_eq!(r.depth, 3, "{r:?}");
        assert_eq!(r.peer_bands, 3, "deduplicated across bands: {r:?}");
        assert_eq!(r.words, 3, "{r:?}");
        assert_eq!(r.native, 3, "band words are layout-free words: {r:?}");
        assert_eq!(out, vec![BASE + 8, BASE + 16, BASE + 0x2000 + 3]);
        assert_eq!(
            l.peer_band_windows(),
            1,
            "only the helper-window band counts as a window"
        );
        // One more JIT entry than the bands cover: a shortfall.
        assert!(!l.read(4, &mut Vec::new()).complete);
    }

    /// A band that was not read whole, or whose peer has no published depth,
    /// fails the pause and credits nothing; the next pause is clean.
    #[test]
    fn a_partial_or_depthless_band_fails_only_its_pause() {
        let l = YoungPinLedger::new();
        l.note_peer_band(&[BASE + 8], None, true, true);
        let r = l.read(0, &mut Vec::new());
        assert!(!r.complete && r.depth == 0, "{r:?}");
        assert_eq!(l.peer_band_windows(), 0);

        l.open_pause();
        l.note_peer_band(&[BASE + 8], Some(1), false, false);
        let r = l.read(0, &mut Vec::new());
        assert!(!r.complete && r.depth == 0, "{r:?}");

        l.open_pause();
        l.mark_peer_band_incomplete();
        assert!(!l.read(0, &mut Vec::new()).complete);

        l.open_pause();
        let mut out = Vec::new();
        let r = l.read(0, &mut out);
        assert!(r.complete && out.is_empty() && r.peer_bands == 0, "{r:?}");
    }

    /// More distinct band words than the cap is incompleteness, not
    /// truncation; duplicates never consume the cap.
    #[test]
    fn a_band_word_overflow_is_incompleteness() {
        let l = YoungPinLedger::new();
        let full: Vec<usize> = (0..PEER_BAND_PIN_WORDS_CAP).map(|i| BASE + 8 * i).collect();
        l.note_peer_band(&full, Some(1), true, true);
        l.note_peer_band(&full[..10], Some(0), true, false);
        let mut out = Vec::new();
        let r = l.read(1, &mut out);
        assert!(r.complete, "exactly the cap is complete: {r:?}");
        assert_eq!(out.len(), PEER_BAND_PIN_WORDS_CAP);
        l.note_peer_band(&[BASE - 8], Some(0), true, false);
        let r = l.read(1, &mut Vec::new());
        assert!(!r.complete, "one distinct word past the cap: {r:?}");
    }

    /// Last pause's bands are neither words nor depth of this one.
    #[test]
    fn a_stale_band_is_ignored() {
        let l = YoungPinLedger::new();
        l.note_peer_band(&[BASE + 8], Some(2), true, true);
        l.open_pause();
        let mut out = Vec::new();
        let r = l.read(2, &mut out);
        assert!(!r.complete && out.is_empty(), "{r:?}");
        assert_eq!(l.peer_band_windows(), 0);
    }

    /// The take-over arm's admission: only the licence a pinning backend gets,
    /// with nobody frozen and at least one window, and only once every window
    /// is in the ledger.
    #[test]
    fn the_takeover_admission_rules() {
        let hw = TakeoverVerdict {
            helper_windows: 2,
            ..TakeoverVerdict::NONE
        };
        assert_eq!(hw.licence(true), MoveLicence::MovePinned);
        assert!(takeover_verdict_admits_pinned_copy(hw));
        assert!(!takeover_admits_band_ledger(hw, 1), "one window missing");
        assert!(takeover_admits_band_ledger(hw, 2));

        assert!(
            !takeover_verdict_admits_pinned_copy(TakeoverVerdict::NONE),
            "no window: term 3 did not fire through the take-over"
        );
        let frozen = TakeoverVerdict { frozen: 1, ..hw };
        assert!(
            !takeover_verdict_admits_pinned_copy(frozen),
            "a frozen peer"
        );
        let unread = TakeoverVerdict {
            unreadable: 1,
            ..hw
        };
        assert!(
            !takeover_verdict_admits_pinned_copy(unread),
            "an unread peer"
        );
        let unpinned = TakeoverVerdict {
            pins_complete: false,
            ..hw
        };
        assert!(
            !takeover_verdict_admits_pinned_copy(unpinned),
            "an unpinned window"
        );
        let derived = TakeoverVerdict {
            derived_pointers_resolved: false,
            ..hw
        };
        assert!(
            !takeover_verdict_admits_pinned_copy(derived),
            "unresolved derived pointers"
        );
        assert!(!takeover_verdict_admits_pinned_copy(
            TakeoverVerdict::UNKNOWN
        ));
        assert!(!takeover_admits_band_ledger(unread, 9));
    }

    /// The proven blocked-monitor stash is the pause's own row: drained once,
    /// cleared by a take-over start and by a pause open, and not another VM's.
    #[test]
    fn the_proven_monitor_stash_is_per_pause_and_per_vm() {
        let a = std::sync::Arc::new(PauseLedger::new());
        let b = std::sync::Arc::new(PauseLedger::new());
        with_pause_ledger(&a, || {
            stash_proven_monitor_peer_for_ledger(11, 2);
            stash_proven_monitor_peer_for_ledger(12, 1);
        });
        with_pause_ledger(&b, || stash_proven_monitor_peer_for_ledger(21, 3));
        assert_eq!(
            with_pause_ledger(&a, take_proven_monitor_peers_for_ledger),
            vec![(11, 2), (12, 1)]
        );
        assert!(with_pause_ledger(&a, take_proven_monitor_peers_for_ledger).is_empty());
        with_pause_ledger(&b, reset_xt_cycle);
        assert!(
            with_pause_ledger(&b, take_proven_monitor_peers_for_ledger).is_empty(),
            "a take-over start clears the stash"
        );
        with_pause_ledger(&a, || stash_proven_monitor_peer_for_ledger(13, 1));
        with_pause_ledger(&a, reset_peer_proven_jit_depth);
        assert!(
            with_pause_ledger(&a, take_proven_monitor_peers_for_ledger).is_empty(),
            "a pause open clears the stash"
        );
    }
}

/// gcd d4/m: the per-VM raw-JNI-locals count the young pin ledger fails closed
/// on.
#[cfg(test)]
mod gcd_d4m_raw_jni_tests {
    use super::*;

    /// A raw scope counts in its own VM's ledger only, until it drops; an
    /// indirect one counts nothing; a pause open does not clear the count.
    #[test]
    fn a_raw_jni_scope_counts_in_its_own_vm_until_it_drops() {
        let a = std::sync::Arc::new(PauseLedger::new());
        let b = std::sync::Arc::new(PauseLedger::new());
        assert!(!with_pause_ledger(&a, raw_jni_locals_open));
        {
            let _indirect = RawJniLocalsScope::enter(&a, false);
            assert!(
                !with_pause_ledger(&a, raw_jni_locals_open),
                "indirect locals do not count"
            );
        }
        let outer = RawJniLocalsScope::enter(&a, true);
        let inner = RawJniLocalsScope::enter(&a, true);
        assert!(with_pause_ledger(&a, raw_jni_locals_open));
        assert!(
            !with_pause_ledger(&b, raw_jni_locals_open),
            "another VM's natives do not count"
        );
        with_pause_ledger(&a, reset_peer_proven_jit_depth);
        assert!(
            with_pause_ledger(&a, raw_jni_locals_open),
            "a standing count survives a pause open"
        );
        drop(inner);
        assert!(
            with_pause_ledger(&a, raw_jni_locals_open),
            "nested: the outer call still holds"
        );
        drop(outer);
        assert!(!with_pause_ledger(&a, raw_jni_locals_open));
    }

    /// The unscoped pair (a foreign attachment) balances and never goes below
    /// zero.
    #[test]
    fn the_unscoped_pair_balances_and_saturates() {
        let a = std::sync::Arc::new(PauseLedger::new());
        note_raw_jni_locals_open(&a);
        assert!(with_pause_ledger(&a, raw_jni_locals_open));
        note_raw_jni_locals_closed(&a);
        note_raw_jni_locals_closed(&a);
        assert!(!with_pause_ledger(&a, raw_jni_locals_open));
        note_raw_jni_locals_open(&a);
        assert!(
            with_pause_ledger(&a, raw_jni_locals_open),
            "a stray close did not go negative"
        );
        note_raw_jni_locals_closed(&a);
    }

    /// gch h2/yg: the census names both imbalances -- a standing count with
    /// no native running, and the stray closes the saturation absorbs -- of
    /// the ledger asked, never another VM's.
    #[test]
    fn gch_h2yg_the_raw_jni_census_counts_the_standing_count_and_stray_closes() {
        let a = std::sync::Arc::new(PauseLedger::new());
        let b = std::sync::Arc::new(PauseLedger::new());
        assert_eq!(raw_jni_locals_census(&a), (0, 0));
        note_raw_jni_locals_open(&a);
        let scope = RawJniLocalsScope::enter(&a, true);
        assert_eq!(raw_jni_locals_census(&a), (2, 0));
        drop(scope);
        note_raw_jni_locals_closed(&a);
        note_raw_jni_locals_closed(&a);
        note_raw_jni_locals_closed(&a);
        assert_eq!(raw_jni_locals_census(&a), (0, 2), "two stray closes");
        assert_eq!(raw_jni_locals_census(&b), (0, 0), "another VM's ledger");
    }

    /// gci i4/pn: a scope whose count a stray unscoped close already took
    /// drops to zero and is counted as an underflow; it used to wrap the
    /// count to `usize::MAX`, which read "raw locals open" for the VM's life.
    #[test]
    fn gci_i4pn_a_scope_drop_after_a_stray_close_saturates_and_is_counted() {
        let a = std::sync::Arc::new(PauseLedger::new());
        let scope = RawJniLocalsScope::enter(&a, true);
        note_raw_jni_locals_closed(&a);
        assert_eq!(raw_jni_locals_census(&a), (0, 0));
        drop(scope);
        assert_eq!(raw_jni_locals_census(&a), (0, 1), "saturated and counted, not wrapped");
        assert!(!with_pause_ledger(&a, raw_jni_locals_open));
        // A balanced scope still counts and releases exactly.
        let again = RawJniLocalsScope::enter(&a, true);
        assert_eq!(raw_jni_locals_census(&a), (1, 1));
        drop(again);
        assert_eq!(raw_jni_locals_census(&a), (0, 1));
    }

    /// gcd d10/t: the take-over's stack-incomplete count is a row of the
    /// pause's OWN ledger -- another VM's notes do not reach it -- and a
    /// take-over start (`reset_xt_cycle`) clears it.
    #[test]
    fn takeover_stack_incomplete_is_a_row_of_the_pause_ledger() {
        let a = std::sync::Arc::new(PauseLedger::new());
        let b = std::sync::Arc::new(PauseLedger::new());
        with_pause_ledger(&a, note_xt_takeover_stack_incomplete);
        with_pause_ledger(&a, note_xt_takeover_stack_incomplete);
        with_pause_ledger(&b, note_xt_takeover_stack_incomplete);
        assert_eq!(
            with_pause_ledger(&a, xt_takeover_stack_incomplete_this_pause),
            Some(2)
        );
        assert_eq!(
            with_pause_ledger(&b, xt_takeover_stack_incomplete_this_pause),
            Some(1)
        );
        with_pause_ledger(&a, reset_xt_cycle);
        assert_eq!(
            with_pause_ledger(&a, xt_takeover_stack_incomplete_this_pause),
            Some(0)
        );
        assert_eq!(
            with_pause_ledger(&b, xt_takeover_stack_incomplete_this_pause),
            Some(1)
        );
    }
}

#[cfg(test)]
mod band_root_census_tests {
    use super::*;

    /// gci i6/mv: the audit may skip an address only when every occurrence of
    /// it in the re-scan is a band word the active map proves dead.
    #[test]
    fn gci_i6mv_only_map_dead_needs_every_occurrence_dead() {
        let mut c = BandRootCensus::default();
        c.note(0x1000, true);
        c.note(0x1000, true);
        assert!(c.only_map_dead(0x1000, 2), "two dead words, two occurrences");
        // A third occurrence from another source (a live, unrewritten word
        // of any section) is a finding.
        assert!(!c.only_map_dead(0x1000, 3));
        // A list that merged entries disagrees with the census: report.
        assert!(!c.only_map_dead(0x1000, 1));
        // One live band word beside a dead one: report.
        c.note(0x2000, true);
        c.note(0x2000, false);
        assert_eq!((c.rooted(0x2000), c.map_dead(0x2000)), (2, 1));
        assert!(!c.only_map_dead(0x2000, 2));
        assert!(!c.only_map_dead(0x2000, 1));
        // Nothing recorded: never skipped.
        assert!(!c.only_map_dead(0x3000, 0));
        assert!(!c.only_map_dead(0x3000, 1));
    }

    /// gci i6/mv: armed by the audit only, recorded per thread, taken once.
    #[test]
    fn gci_i6mv_band_root_census_is_armed_recorded_and_taken_once() {
        assert!(take_band_root_census().is_none(), "never armed on a fresh thread");
        note_band_root_census(0x10, true);
        assert!(!band_root_census_armed());
        assert!(take_band_root_census().is_none(), "an unarmed note records nothing");
        arm_band_root_census();
        assert!(band_root_census_armed());
        note_band_root_census(0x10, true);
        note_band_root_census(0x18, false);
        let c = take_band_root_census().expect("armed");
        assert_eq!((c.rooted(0x10), c.map_dead(0x10)), (1, 1));
        assert_eq!((c.rooted(0x18), c.map_dead(0x18)), (1, 0));
        assert!(!band_root_census_armed(), "taking disarms");
        assert!(take_band_root_census().is_none());
    }
}

#[cfg(test)]
mod gci_i9ng_label_tests {
    use super::*;

    #[test]
    fn gci_i9ng_the_label_door_names_the_reason_and_decides_nothing() {
        begin_moving_young_coverage_cycle();
        mark_moving_young_coverage_incomplete();
        assert_eq!(moving_young_incomplete_reason(), incomplete_reason::NONE);
        label_moving_young_coverage_incomplete(incomplete_reason::XT_HELPER_WINDOW);
        assert_eq!(moving_young_incomplete_reason(), incomplete_reason::XT_HELPER_WINDOW);
        assert_eq!(moving_young_incomplete_reason_mask(), 0, "deciders read the mask");
        assert!(!peer_state_reason_recorded());
        assert!(!unrewritable_peer_state());
        assert!(moving_young_coverage_incomplete());
        label_moving_young_coverage_incomplete(incomplete_reason::XT_TAKEOVER);
        assert_eq!(
            moving_young_incomplete_reason(),
            incomplete_reason::XT_HELPER_WINDOW,
            "first wins"
        );
        begin_moving_young_coverage_cycle();
        mark_moving_young_coverage_incomplete_because(incomplete_reason::UNPUBLISHED_FRAME_OOP);
        label_moving_young_coverage_incomplete(incomplete_reason::XT_TAKEOVER);
        assert_eq!(moving_young_incomplete_reason(), incomplete_reason::UNPUBLISHED_FRAME_OOP);
        begin_moving_young_coverage_cycle();
        label_moving_young_coverage_incomplete(incomplete_reason::XT_TAKEOVER);
        assert!(!moving_young_coverage_incomplete(), "a label alone refuses nothing");
        begin_moving_young_coverage_cycle();
    }
}

#[cfg(test)]
mod gci_i12sx_takeover_switch_tests {
    //! gci i12/sx: `CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER` is read from the
    //! flag snapshot per call (no process `OnceLock`), so a per-thread test
    //! override reaches both of its readers in one test process, in both
    //! directions.
    use cratonvm_types::flags::with_thread_overrides;

    const TAKEOVER: &str = "CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER";
    const PINNED: &str = "CRATONVM_GEN_PINNED_YOUNG_COPY";

    #[test]
    fn gci_i12sx_takeover_switch_follows_thread_overrides() {
        with_thread_overrides(&[(TAKEOVER, Some("1")), (PINNED, Some("1"))], || {
            assert!(super::pinned_young_copy_takeover_enabled());
            assert!(super::generational_takeover_pins_honoured());
        });
        with_thread_overrides(&[(TAKEOVER, Some("0")), (PINNED, Some("1"))], || {
            assert!(!super::pinned_young_copy_takeover_enabled(), "=0 is off");
            assert!(!super::generational_takeover_pins_honoured());
        });
        with_thread_overrides(&[(TAKEOVER, Some("1")), (PINNED, Some("0"))], || {
            assert!(super::pinned_young_copy_takeover_enabled());
            assert!(
                !super::generational_takeover_pins_honoured(),
                "needs the pinned copy too"
            );
        });
        // gcj j1yg-close: default ON since 2026-10-27.
        with_thread_overrides(&[(TAKEOVER, None)], || {
            assert!(super::pinned_young_copy_takeover_enabled(), "default ON");
        });
        // And back on after an off read: nothing latched.
        with_thread_overrides(&[(TAKEOVER, Some("yes"))], || {
            assert!(super::pinned_young_copy_takeover_enabled());
        });
    }

    /// Source pin: no `OnceLock` (or any static) in the reader's body.
    #[test]
    fn gci_i12sx_takeover_switch_has_no_process_latch() {
        let src = include_str!("gc_quiescence.rs").replace("\r\n", "\n");
        let start = src
            .find("pub fn pinned_young_copy_takeover_enabled() -> bool {")
            .expect("reader");
        let body = &src[start..start + src[start..].find("\n}\n").expect("end")];
        assert!(!body.contains("OnceLock"), "{body}");
        assert!(!body.contains(&["sta", "tic "].concat()), "{body}");
    }
}

#[cfg(test)]
mod gci_i47w_sg47_tests {
    //! gci i47w/sg47: the `System.gc()` tenure-all request
    //! (`CRATONVM_GC_SYSTEM_GC_TENURES_ALL`) is armed only under the switch,
    //! is read without being consumed, and is withdrawn by the take.
    use cratonvm_types::flags::with_thread_overrides;

    const SWITCH: &str = "CRATONVM_GC_SYSTEM_GC_TENURES_ALL";

    #[test]
    fn gci_i47w_sg47_tenure_request_is_armed_only_under_the_switch() {
        let _ = super::take_system_gc_tenure_request();
        with_thread_overrides(&[(SWITCH, None)], || {
            assert!(super::system_gc_tenures_all_enabled(), "default ON (2026-10-17 flip)");
            super::request_system_gc_tenure();
            assert!(super::system_gc_tenure_requested(), "default ON arms the request");
            assert!(super::take_system_gc_tenure_request());
        });
        with_thread_overrides(&[(SWITCH, Some("0"))], || {
            super::request_system_gc_tenure();
            assert!(!super::system_gc_tenure_requested(), "=0 is off");
        });
        with_thread_overrides(&[(SWITCH, Some("1"))], || {
            assert!(super::system_gc_tenures_all_enabled());
            super::request_system_gc_tenure();
            assert!(super::system_gc_tenure_requested());
            assert!(
                super::system_gc_tenure_requested(),
                "a read does not consume: the door's refusal-and-retry reads it again"
            );
            assert!(super::take_system_gc_tenure_request());
            assert!(!super::system_gc_tenure_requested(), "withdrawn by the take");
            assert!(!super::take_system_gc_tenure_request());
        });
    }

    /// Source pin: the reader has no process latch (per-VM flag snapshot).
    #[test]
    fn gci_i47w_sg47_tenure_switch_has_no_process_latch() {
        let src = include_str!("gc_quiescence.rs").replace("\r\n", "\n");
        let start = src
            .find("pub fn system_gc_tenures_all_enabled() -> bool {")
            .expect("reader");
        let body = &src[start..start + src[start..].find("\n}\n").expect("end")];
        assert!(!body.contains("OnceLock"), "{body}");
        assert!(body.contains("runtime_flag_default_on"), "{body}");
    }
}
