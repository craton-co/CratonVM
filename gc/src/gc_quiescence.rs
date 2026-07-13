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
}

/// Request that the next collection on this thread run a full (major) cycle
/// regardless of old-gen occupancy. Set by `System.gc()`'s native
/// implementation (`force_gc_from_native`).
pub fn request_major_gc() {
    MAJOR_GC_REQUESTED.with(|c| c.set(true));
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
    std::sync::Mutex<std::collections::HashMap<std::thread::ThreadId, std::collections::HashSet<usize>>>,
> = std::sync::OnceLock::new();

fn pinned_jit_map(
) -> &'static std::sync::Mutex<std::collections::HashMap<std::thread::ThreadId, std::collections::HashSet<usize>>>
{
    PINNED_JIT_ROOTS_BY_THREAD.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
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
    if let Ok(mut map) = pinned_jit_map().lock() {
        map.entry(std::thread::current().id()).or_default().insert(addr);
    }
}

/// Replace the CALLING thread's pin entry wholesale with `addrs` (removing it
/// when empty). Used by the root-snapshot deposit paths so a thread's pins
/// always reflect its CURRENT live JIT frames.
pub fn publish_pinned_jit_roots(addrs: &[usize]) {
    arm_pinned_guard();
    if let Ok(mut map) = pinned_jit_map().lock() {
        let tid = std::thread::current().id();
        if addrs.is_empty() {
            map.remove(&tid);
        } else {
            map.insert(tid, addrs.iter().copied().collect());
        }
    }
}

/// Snapshot the conservative-pinned-JIT-root addresses published by ALL
/// threads. `G1Collector` maps these to regions it must exclude from the
/// collection set.
pub fn pinned_jit_roots_snapshot() -> Vec<usize> {
    match pinned_jit_map().lock() {
        Ok(map) => map.values().flat_map(|s| s.iter().copied()).collect(),
        Err(_) => Vec::new(),
    }
}

/// Count of conservative-pinned JIT roots currently published (diagnostics).
pub fn pinned_jit_root_count() -> usize {
    match pinned_jit_map().lock() {
        Ok(map) => map.values().map(|s| s.len()).sum(),
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
// docs/known-issues/elasticsearch-randomizedcontext-per-thread-null.md).
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

/// Is `addr` a currently-registered Weak/Soft/Phantom referent this cycle?
/// Consulted by the non-moving young sweep for each kept-in-place survivor.
pub fn is_watched_referent(addr: usize) -> bool {
    WATCHED_REFERENTS.with(|s| s.borrow().contains(&addr))
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
