// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Java reference processing for the garbage collector.
//!
//! Implements the four reference types defined by `java.lang.ref`:
//! [`SoftReference`], [`WeakReference`], [`PhantomReference`], and the
//! internal [`Cleaner`] / [`FinalReference`] types used by the JDK.
//!
//! Processing order matches HotSpot:
//! 1. SoftReferences  -- cleared only under memory pressure (LRU policy)
//! 2. WeakReferences  -- always cleared when referent is unreachable
//! 3. FinalReferences  -- enqueued so the finalizer thread can run `finalize()`
//! 4. PhantomReferences -- enqueued after finalisation (Java 9+: referent NOT cleared)

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};
use crate::gc_flags;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The kind of `java.lang.ref.Reference` subclass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceType {
    Strong,
    Soft,
    Weak,
    Phantom,
    Cleaner,
    Finalizer,
}

/// A single discovered reference.
#[derive(Debug, Clone)]
pub struct ReferenceEntry {
    pub ref_type: ReferenceType,
    /// Address of the `Reference` object itself on the heap.
    pub reference_obj: usize,
    /// Address of the referent (the object the reference points to).
    pub referent: usize,
    /// Address of the associated `ReferenceQueue`, if any.
    pub queue_addr: Option<usize>,
    /// Whether this reference has already been enqueued.
    pub enqueued: bool,
    /// Whether `clear()` has been called / the referent nulled.
    pub cleared: bool,
    /// Timestamp of the last `get()` call -- used for SoftReference LRU.
    pub last_access_time_ms: u64,
    /// bc math-ec 0x4 ROOT-CAUSE FIX (2026-06-10): once-only emission flags.
    /// The VM-side consumers WRITE heap fields for each emitted action
    /// (null the referent; submit the cleaner action). Before these flags the
    /// registry RE-EMITTED every cleared entry on EVERY GC, forever — and once
    /// the Reference object died, its recycled address was "remapped" onto
    /// whatever innocent object reused the memory, corrupting it with
    /// perfectly-legal-looking writes (the FixedPointTest `Object(Some(0x4))`
    /// / silent-null corruption; see docs/internal/h2-testscript-segv-findings.md).
    /// `clear_emitted`: the referent-null for this entry was already handed out.
    pub clear_emitted: bool,
    /// `action_emitted`: the cleaner action for this entry was already handed out.
    pub action_emitted: bool,
}

/// Aggregated stats for one round of reference processing.
#[derive(Debug, Default, Clone)]
pub struct ReferenceProcessingStats {
    pub soft_refs_discovered: usize,
    pub soft_refs_cleared: usize,
    pub weak_refs_discovered: usize,
    pub weak_refs_cleared: usize,
    pub phantom_refs_discovered: usize,
    pub phantom_refs_enqueued: usize,
    pub cleaner_refs_processed: usize,
    pub finalizer_refs_discovered: usize,
    pub finalizer_refs_enqueued: usize,
}

/// Outcome of [`ReferenceProcessor::process_references`].
pub struct ReferenceProcessingResult {
    /// `(reference_obj, queue_addr)` pairs to enqueue.
    pub to_enqueue: Vec<(usize, usize)>,
    /// Objects that need `finalize()` executed.
    pub to_finalize: Vec<usize>,
    /// Cleaner action addresses to run.
    pub cleaner_actions: Vec<usize>,
    /// Stats snapshot.
    pub stats: ReferenceProcessingStats,
}

// ---------------------------------------------------------------------------
// ReferenceQueue
// ---------------------------------------------------------------------------

/// A `java.lang.ref.ReferenceQueue` analogue.
pub struct ReferenceQueue {
    pub queue_addr: usize,
    pending: VecDeque<usize>,
    max_capacity: usize,
    /// Count of references dropped due to capacity overflow.
    overflow_count: usize,
}

impl ReferenceQueue {
    pub fn new(queue_addr: usize, max_capacity: usize) -> Self {
        Self {
            queue_addr,
            pending: VecDeque::new(),
            max_capacity,
            overflow_count: 0,
        }
    }

    /// Enqueue a reference object. If at capacity, the oldest reference
    /// is dropped to make room (log-and-evict policy instead of silent loss).
    pub fn enqueue(&mut self, reference_obj: usize) -> bool {
        if self.pending.len() >= self.max_capacity {
            // Evict oldest to make room rather than silently dropping
            self.pending.pop_front();
            self.overflow_count += 1;
            tracing::debug!(
                "ReferenceQueue 0x{:x} overflow: evicted oldest entry (total overflows: {})",
                self.queue_addr,
                self.overflow_count
            );
        }
        self.pending.push_back(reference_obj);
        true
    }

    /// Poll for the next enqueued reference (non-blocking).
    pub fn poll(&mut self) -> Option<usize> {
        self.pending.pop_front()
    }

    /// Blocking remove with timeout. Spin-waits up to `timeout_ms` milliseconds
    /// for an element to become available. Returns `None` if timeout expires.
    pub fn remove_timeout(&mut self, timeout_ms: u64) -> Option<usize> {
        if let Some(val) = self.pending.pop_front() {
            return Some(val);
        }
        // Spin-wait with yield, checking periodically
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_millis(timeout_ms);
        while start.elapsed() < timeout {
            std::thread::yield_now();
            if let Some(val) = self.pending.pop_front() {
                return Some(val);
            }
        }
        None
    }

    /// Blocking remove (indefinite wait). Spin-waits with yield.
    /// Has a safety cap of 60 seconds to prevent deadlock.
    pub fn remove_blocking(&mut self) -> Option<usize> {
        self.remove_timeout(60_000)
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn overflow_count(&self) -> usize {
        self.overflow_count
    }
}

// ---------------------------------------------------------------------------
// ReferenceProcessor
// ---------------------------------------------------------------------------

/// Central reference processor invoked during GC pauses.
pub struct ReferenceProcessor {
    soft_refs: Vec<ReferenceEntry>,
    weak_refs: Vec<ReferenceEntry>,
    phantom_refs: Vec<ReferenceEntry>,
    cleaner_refs: Vec<ReferenceEntry>,
    finalizer_refs: Vec<ReferenceEntry>,

    /// `-XX:SoftRefLRUPolicyMSPerMB` equivalent (default 1000).
    soft_ref_lru_policy_ms_per_mb: u64,

    /// `queue_addr -> [reference_obj ...]` for pending enqueue notifications.
    /// T10.9.B: FxHashMap — queue addresses are internal pointer values.
    pending_queues: FxHashMap<usize, Vec<usize>>,

    /// Objects awaiting `finalize()`.
    ///
    /// Round-2 fix (GC §7): `VecDeque` so `take_pending_finalizer` is O(1)
    /// pop-front instead of O(n) `Vec::remove(0)`.
    finalization_queue: std::collections::VecDeque<usize>,

    /// Index of soft references by `last_access_time_ms` for efficient
    /// range-based LRU clearing. Maps `(timestamp, index_in_soft_refs)` to
    /// allow O(log n) range queries instead of O(n) linear scans.
    soft_ref_lru_index: BTreeMap<(u64, usize), usize>,

    /// PERF: address -> position-in-`soft_refs` index, so `touch_soft_reference`
    /// (called on every `SoftReference.get()` that advances the LRU timer) is
    /// O(1) instead of a linear scan over all soft refs. For large soft-ref
    /// populations (memory-sensitive caches) the old per-`get()` linear scan
    /// made `get()` O(n). This map is kept in lock-step with `soft_refs`
    /// positions at every mutation site (discover/relocate/remove_collected);
    /// the `(reference_obj -> idx)` invariant mirrors the `soft_ref_lru_index`
    /// `(timestamp, idx) -> idx` invariant the LRU-leak fix already maintains.
    soft_ref_addr_index: FxHashMap<usize, usize>,

    stats: ReferenceProcessingStats,
}

impl ReferenceProcessor {
    pub fn new() -> Self {
        Self::new_with_policy(1000)
    }

    pub fn new_with_policy(soft_ref_lru_ms_per_mb: u64) -> Self {
        Self {
            soft_refs: Vec::new(),
            weak_refs: Vec::new(),
            phantom_refs: Vec::new(),
            cleaner_refs: Vec::new(),
            finalizer_refs: Vec::new(),
            soft_ref_lru_policy_ms_per_mb: soft_ref_lru_ms_per_mb,
            pending_queues: FxHashMap::default(),
            finalization_queue: std::collections::VecDeque::new(),
            soft_ref_lru_index: BTreeMap::new(),
            soft_ref_addr_index: FxHashMap::default(),
            stats: ReferenceProcessingStats::default(),
        }
    }

    // -- Discovery ----------------------------------------------------------

    /// Register a newly-discovered reference during the marking phase.
    pub fn discover_reference(
        &mut self,
        ref_type: ReferenceType,
        reference_obj: usize,
        referent: usize,
        queue: Option<usize>,
    ) {
        let entry = ReferenceEntry {
            ref_type,
            reference_obj,
            referent,
            queue_addr: queue,
            enqueued: false,
            cleared: false,
            last_access_time_ms: 0,
            clear_emitted: false,
            action_emitted: false,
        };
        match ref_type {
            ReferenceType::Soft => {
                let idx = self.soft_refs.len();
                self.soft_ref_lru_index
                    .insert((entry.last_access_time_ms, idx), idx);
                // PERF: maintain the address->idx index for O(1) touch.
                // First-occurrence wins (matches the old linear scan, which
                // returned at the lowest-index match) — `or_insert` so a
                // duplicate-address discovery does not steal the target from
                // the earlier entry.
                self.soft_ref_addr_index
                    .entry(entry.reference_obj)
                    .or_insert(idx);
                self.soft_refs.push(entry);
            }
            ReferenceType::Weak => self.weak_refs.push(entry),
            ReferenceType::Phantom => self.phantom_refs.push(entry),
            ReferenceType::Cleaner => self.cleaner_refs.push(entry),
            ReferenceType::Finalizer => self.finalizer_refs.push(entry),
            ReferenceType::Strong => {} // strong refs need no special treatment
        }
    }

    /// Record an access to a SoftReference's referent, updating the LRU
    /// timestamp used by [`Self::process_soft_refs`].
    ///
    /// The runtime should call this from the `SoftReference.get()` native
    /// after confirming the referent is still live. Without this call the
    /// LRU index forever sees `last_access_time_ms == 0`, so every
    /// SoftReference looks infinitely stale and gets cleared on the next
    /// major GC — defeating the whole point of memory-sensitive caches.
    ///
    /// Cost is O(1) for the address-index lookup plus O(log n) for the
    /// BTreeMap re-key; no global lock beyond the caller's existing
    /// `Mutex<ReferenceProcessor>` is taken.
    pub fn touch_soft_reference(&mut self, reference_obj: usize, now_ms: u64) {
        // PERF: O(1) address-index lookup replaces the former O(n) linear
        // scan over all soft refs. `SoftReference.get()` calls this on the
        // timer-advance path; for large cache populations the linear scan made
        // `get()` O(n). `soft_ref_addr_index` is kept in lock-step with
        // `soft_refs` positions, so the resolved `idx` indexes the same entry
        // the old scan would have found (first-occurrence wins for duplicates).
        let idx = match self.soft_ref_addr_index.get(&reference_obj) {
            Some(&idx) => idx,
            None => return,
        };
        let entry = &mut self.soft_refs[idx];
        let old_key = (entry.last_access_time_ms, idx);
        // Only re-insert when the timestamp actually advances;
        // many caches `get()` faster than the timer resolution.
        if now_ms == entry.last_access_time_ms {
            return;
        }
        self.soft_ref_lru_index.remove(&old_key);
        entry.last_access_time_ms = now_ms;
        self.soft_ref_lru_index.insert((now_ms, idx), idx);
    }

    // -- Main entry point ---------------------------------------------------

    /// Process all reference types in HotSpot order.
    ///
    /// This is the legacy entry point: it has no access to a heap-graph
    /// tracing primitive, so it can only keep the *direct* finalizer referents
    /// (for soft+weak clearing) and the *direct* surviving soft referents (for
    /// weak clearing, per the JLS soft > weak ordering) alive — the deeper
    /// transitive cases need a caller-supplied tracer (see
    /// [`Self::process_references_with_finalizer_trace`] for the full
    /// transitive closure and a discussion of the residual gap).
    pub fn process_references(
        &mut self,
        is_marked: &dyn Fn(usize) -> bool,
        free_heap_mb: usize,
        current_time_ms: u64,
    ) -> ReferenceProcessingResult {
        // SECURITY FIX (V18): route the legacy entry point through the
        // finalizer-aware path with no transitive tracer. Passing `None`
        // still closes the common single-hop case (a weak/soft ref whose
        // referent *is* a finalizable object) by treating the direct
        // finalizer referents as live for Phases 1-2.
        self.process_references_with_finalizer_trace(is_marked, None, free_heap_mb, current_time_ms)
    }

    /// Process all reference types in HotSpot order, recomputing the
    /// finalizer-reachable closure before soft/weak refs are cleared and the
    /// soft-reachable closure before weak refs are cleared.
    ///
    /// SECURITY FIX (V18): spec-conformance. Previously, Phase 1 (soft) and
    /// Phase 2 (weak) cleared references using a single `is_marked` snapshot
    /// that did *not* include objects kept alive only because they are
    /// reachable from an about-to-be-finalized object. A finalizer that ran
    /// in this same cycle could therefore observe an already-nulled
    /// weak/soft referent. HotSpot avoids this by treating the
    /// finalizer-reachable set as live during weak/soft processing.
    ///
    /// SPEC FIX (JLS reachability ordering): Phase 2 (weak) additionally
    /// honours the *soft*-reachable closure. Per the JLS strength ordering
    /// (strong > soft > weak > phantom), a weak reference must not be cleared
    /// while its referent is still softly reachable — i.e. reachable from a
    /// soft referent that Phase 1 chose to retain. Previously a weak ref to an
    /// object kept alive only via a surviving soft reference was wrongly
    /// cleared. We now re-trace from the surviving (uncleared) soft referents
    /// after Phase 1 and fold that closure into the weak-clearing predicate.
    ///
    /// `trace_from`, when supplied, is the heap's "mark + trace from a set of
    /// roots" primitive: given the finalizer roots (the referents of finalizer
    /// references whose referent is currently unmarked, i.e. those about to be
    /// enqueued for finalization), it returns *all* addresses transitively
    /// reachable from them. This is the same `&dyn Fn` callback mechanism the
    /// module already uses for `is_marked` — the heap owns the field layout,
    /// so only it can walk the graph; reference.rs has no field-offset
    /// knowledge and intentionally does not gain a cross-module dependency on
    /// `gen_heap`/`g1`.
    ///
    /// When `trace_from` is `None` (legacy callers), we fall back to marking
    /// only the *direct* finalizer referents as live. RESIDUAL GAP: in that
    /// mode a weak/soft ref to an object reachable only *transitively* through
    /// a finalizable object (depth >= 2) can still be cleared prematurely.
    /// Closing that gap fully requires a caller to pass `trace_from`, which in
    /// turn requires the heap (`gen_heap.rs` / `g1.rs`) to expose its tracing
    /// primitive — an out-of-scope change for this fix.
    pub fn process_references_with_finalizer_trace(
        &mut self,
        is_marked: &dyn Fn(usize) -> bool,
        trace_from: Option<&dyn Fn(&[usize]) -> Vec<usize>>,
        free_heap_mb: usize,
        current_time_ms: u64,
    ) -> ReferenceProcessingResult {
        self.stats = ReferenceProcessingStats::default();

        self.stats.soft_refs_discovered = self.soft_refs.len();
        self.stats.weak_refs_discovered = self.weak_refs.len();
        self.stats.phantom_refs_discovered = self.phantom_refs.len();
        self.stats.finalizer_refs_discovered = self.finalizer_refs.len();

        // SECURITY FIX (V18): Phase 0 — compute the finalizer-reachable
        // closure *before* any soft/weak ref is cleared.
        //
        // Roots are the referents of finalizer references that are not
        // already marked (those are exactly the objects Phase 3 will enqueue
        // for finalization and which must stay live for the finalizer to run)
        // and not already cleared/enqueued in a prior cycle.
        let finalizer_roots: Vec<usize> = self
            .finalizer_refs
            .iter()
            .filter(|e| !e.cleared && !e.enqueued && !is_marked(e.referent))
            .map(|e| e.referent)
            .collect();

        // Build the live closure. With a tracer we get the full transitive
        // set; without one we keep only the direct referents (single hop).
        let finalizer_live: HashSet<usize> = if finalizer_roots.is_empty() {
            HashSet::new()
        } else if let Some(trace) = trace_from {
            trace(&finalizer_roots).into_iter().collect()
        } else {
            finalizer_roots.iter().copied().collect()
        };

        // Augmented liveness predicate used for Phase 1 (soft) only: an object
        // is "live" if the collector already marked it OR it is reachable from
        // a to-be-finalized object. Phases 3-4 keep using the raw `is_marked`
        // snapshot so finalizers/phantoms are still discovered correctly.
        let soft_is_live =
            |addr: usize| -> bool { is_marked(addr) || finalizer_live.contains(&addr) };

        // Phase 1 (soft) honours the finalizer-reachable closure.
        self.process_soft_refs(&soft_is_live, free_heap_mb, current_time_ms);

        // SPEC FIX (JLS reachability ordering, strong > soft > weak > phantom):
        // weak references must NOT be cleared for a referent that is still
        // softly reachable. A soft referent that survived Phase 1 keeps
        // everything strongly reachable *from it* alive with respect to the
        // weaker reference levels; clearing a weak ref to such an object
        // (whether the object is itself a surviving soft referent, or is only
        // reachable through one) would violate the ordering — HotSpot keeps the
        // soft-reachable set live across weak processing.
        //
        // Roots are the referents of soft references that Phase 1 left
        // uncleared (and which are not already enqueued/cleared from a prior
        // cycle). With a tracer we fold the full transitive closure from those
        // roots into the weak-clearing liveness predicate; without one we fall
        // back to keeping the direct surviving soft referents alive (single
        // hop), which still protects a weak ref pointing straight at a retained
        // soft referent. The deeper, tracer-less transitive case is the same
        // documented residual gap as the finalizer closure above.
        let soft_survivor_roots: Vec<usize> = self
            .soft_refs
            .iter()
            .filter(|e| !e.cleared && !e.enqueued)
            .map(|e| e.referent)
            .collect();
        let soft_live: HashSet<usize> = if soft_survivor_roots.is_empty() {
            HashSet::new()
        } else if let Some(trace) = trace_from {
            trace(&soft_survivor_roots).into_iter().collect()
        } else {
            soft_survivor_roots.iter().copied().collect()
        };

        // Phase 2 (weak) liveness folds in BOTH the finalizer-reachable closure
        // and the soft-reachable closure computed above.
        let weak_is_live = |addr: usize| -> bool {
            is_marked(addr) || finalizer_live.contains(&addr) || soft_live.contains(&addr)
        };
        self.process_weak_refs(&weak_is_live);
        // Phase 3-4 (final, phantom) use the raw collector marking so that
        // the about-to-be-finalized objects are still discovered/enqueued and
        // phantom reachability is unaffected by the resurrection closure.
        self.process_final_refs(is_marked);
        self.process_phantom_refs(is_marked);

        // Build result
        let mut to_enqueue = Vec::new();
        let mut to_finalize = Vec::new();
        let mut cleaner_actions = Vec::new();

        // bc math-ec 0x4 ROOT-CAUSE FIX (2026-06-10): ONCE-ONLY emission.
        //
        // Previously this block RE-EMITTED, on EVERY GC forever: every pending
        // enqueue pair (`pending_queues` was read non-destructively), every
        // pending finalization (`finalization_queue.iter()`), and every
        // cleared cleaner. The VM-side consumer WRITES heap fields per emitted
        // action (queue head/size/next; referent null; cleaner submit). Those
        // raw registry addresses are only single-step-remapped per cycle, so
        // the moment an emitted Reference/queue object DIED, its recycled
        // address aliased an innocent live object on a later cycle — and the
        // per-GC re-emission then corrupted that object with valid-looking
        // writes every collection (FixedPointTest `Object(Some(0x4))` /
        // silent-null corruption — proven by hexdump + the NO_REFPROC 6/6
        // exclusion run; docs/internal/h2-testscript-segv-findings.md).
        //
        // Java semantics want each of these EXACTLY ONCE: Reference.enqueue is
        // one-shot, a referent is nulled once, a Cleaner runs once, finalize()
        // runs once. Drain/flag accordingly.

        // Gather enqueue pairs — DRAIN pending_queues (each pair was pushed
        // exactly once when its entry was first cleared).
        for (queue_addr, refs) in std::mem::take(&mut self.pending_queues) {
            for ref_obj in refs {
                to_enqueue.push((ref_obj, queue_addr));
            }
        }

        // DRAIN the finalization queue. (Also fixes the double-enqueue: the
        // interpreter additionally drains via `dequeue_for_finalization`
        // right after consuming this result — that loop now finds it empty.)
        to_finalize.extend(self.finalization_queue.drain(..));

        // Cleaner actions: emit each cleared cleaner ONCE.
        for entry in &mut self.cleaner_refs {
            if entry.cleared && !entry.action_emitted {
                entry.action_emitted = true;
                cleaner_actions.push(entry.reference_obj);
            }
        }
        self.stats.cleaner_refs_processed = cleaner_actions.len();

        ReferenceProcessingResult {
            to_enqueue,
            to_finalize,
            cleaner_actions,
            stats: self.stats.clone(),
        }
    }

    // -- Phase 1: SoftReferences -------------------------------------------

    fn process_soft_refs(
        &mut self,
        is_marked: &dyn Fn(usize) -> bool,
        free_heap_mb: usize,
        current_time_ms: u64,
    ) {
        let threshold_ms = self
            .soft_ref_lru_policy_ms_per_mb
            .saturating_mul(free_heap_mb as u64);

        // Use the BTreeMap index to efficiently find soft refs whose
        // last_access_time is old enough to exceed the idle threshold.
        // Only entries with last_access_time <= cutoff can have idle_ms > threshold_ms.
        let cutoff = current_time_ms.saturating_sub(threshold_ms);

        // Collect indices of candidates from the BTreeMap range [0..=cutoff].
        let candidate_indices: Vec<usize> = self
            .soft_ref_lru_index
            .range(..=(cutoff, usize::MAX))
            .map(|(_, &idx)| idx)
            .collect();

        for idx in candidate_indices {
            // MEDIUM FIX (LRU-index leak): collect the LRU keys to drop in this
            // pass and remove them from the BTreeMap *after* the `entry` borrow
            // of `self.soft_refs[idx]` is released — `soft_ref_lru_index` and
            // `soft_refs` are disjoint fields, but the borrow checker can't see
            // that across the index's `&mut self` method call while `entry`
            // (a borrow of `self.soft_refs`) is live.
            let stale_lru_key: Option<(u64, usize)>;
            {
                let entry = &mut self.soft_refs[idx];
                if entry.cleared {
                    // A previously-cleared entry whose key the range query still
                    // returned. Its referent is dead and it will never become a
                    // clear candidate again, so drop its key — a defensive sweep
                    // in case some other path set `cleared` without pruning.
                    stale_lru_key = Some((entry.last_access_time_ms, idx));
                } else if is_marked(entry.referent) {
                    continue; // referent still live; keep its LRU key intact
                } else {
                    // Double-check LRU policy (the BTreeMap range is an
                    // approximation since we key on insertion time; re-verify the
                    // exact idle window).
                    let idle_ms = current_time_ms.saturating_sub(entry.last_access_time_ms);
                    if idle_ms > threshold_ms {
                        entry.cleared = true;
                        self.stats.soft_refs_cleared += 1;
                        if let Some(q) = entry.queue_addr {
                            self.pending_queues
                                .entry(q)
                                .or_default()
                                .push(entry.reference_obj);
                            entry.enqueued = true;
                        }
                        // A cleared/enqueued soft ref will never be a clear
                        // candidate again (the `entry.cleared` guard skips it).
                        // Leaving its `(timestamp, idx)` key in the BTreeMap
                        // means the range scan in every subsequent GC re-walks
                        // it (and `touch_soft_reference` would scan it), so the
                        // index grew unbounded relative to live soft refs. Drop
                        // its key so the LRU index stays in sync with the *live*
                        // (uncleared) soft refs. Surviving entries keep their
                        // keys untouched, so LRU ordering for them is preserved.
                        // The key must use the entry's current
                        // `last_access_time_ms` (what `touch_soft_reference`
                        // last inserted).
                        stale_lru_key = Some((entry.last_access_time_ms, idx));
                    } else {
                        // Not idle enough this cycle; keep its LRU key.
                        stale_lru_key = None;
                    }
                }
            }
            if let Some(key) = stale_lru_key {
                self.soft_ref_lru_index.remove(&key);
            }
        }
    }

    // -- Phase 2: WeakReferences -------------------------------------------

    fn process_weak_refs(&mut self, is_marked: &dyn Fn(usize) -> bool) {
        let dbg = gc_flags().dbg_watchref;
        for entry in &mut self.weak_refs {
            if entry.cleared {
                continue;
            }
            if is_marked(entry.referent) {
                if dbg {
                    eprintln!(
                        "[watchref] weak KEEP ref_obj=0x{:x} referent=0x{:x}",
                        entry.reference_obj, entry.referent
                    );
                }
                continue;
            }
            if dbg {
                eprintln!(
                    "[watchref] weak CLEAR ref_obj=0x{:x} referent=0x{:x}",
                    entry.reference_obj, entry.referent
                );
            }
            entry.cleared = true;
            self.stats.weak_refs_cleared += 1;
            if let Some(q) = entry.queue_addr {
                self.pending_queues
                    .entry(q)
                    .or_default()
                    .push(entry.reference_obj);
                entry.enqueued = true;
            }
        }
    }

    // -- Phase 3: Cleaner / FinalReferences --------------------------------

    fn process_final_refs(&mut self, is_marked: &dyn Fn(usize) -> bool) {
        // Cleaners
        for entry in &mut self.cleaner_refs {
            if entry.cleared {
                continue;
            }
            if is_marked(entry.referent) {
                continue;
            }
            entry.cleared = true;
            // Cleaners don't use ReferenceQueue -- they run an action directly
        }

        // Finalizers: enqueue referent for finalization but do NOT clear the
        // referent yet (the finalizer thread needs to reach it).
        for entry in &mut self.finalizer_refs {
            if entry.enqueued || entry.cleared {
                continue;
            }
            if is_marked(entry.referent) {
                continue;
            }
            entry.enqueued = true;
            self.stats.finalizer_refs_enqueued += 1;
            self.finalization_queue.push_back(entry.referent);
            if let Some(q) = entry.queue_addr {
                self.pending_queues
                    .entry(q)
                    .or_default()
                    .push(entry.reference_obj);
            }
        }
    }

    // -- Phase 4: PhantomReferences ----------------------------------------

    fn process_phantom_refs(&mut self, is_marked: &dyn Fn(usize) -> bool) {
        for entry in &mut self.phantom_refs {
            if entry.enqueued {
                continue;
            }
            if is_marked(entry.referent) {
                continue;
            }
            // Java 9+: referent is NOT cleared for PhantomReferences.
            entry.enqueued = true;
            self.stats.phantom_refs_enqueued += 1;
            if let Some(q) = entry.queue_addr {
                self.pending_queues
                    .entry(q)
                    .or_default()
                    .push(entry.reference_obj);
            }
        }
    }

    // -- Post-GC maintenance ------------------------------------------------

    /// Relocate addresses after a compacting / copying GC.
    ///
    /// Validates that all target addresses in the pointer map are non-null
    /// and within a plausible heap range (non-zero) to prevent corruption
    /// from a bad relocation map.
    pub fn update_after_gc(&mut self, pointer_map: &HashMap<usize, usize>) {
        fn relocate_list(list: &mut [ReferenceEntry], map: &HashMap<usize, usize>) {
            for e in list.iter_mut() {
                if let Some(&new_addr) = map.get(&e.reference_obj) {
                    if new_addr != 0 {
                        e.reference_obj = new_addr;
                    } else {
                        tracing::warn!(
                            "update_after_gc: null target for reference_obj 0x{:x}",
                            e.reference_obj
                        );
                    }
                }
                if let Some(&new_addr) = map.get(&e.referent) {
                    if new_addr != 0 {
                        e.referent = new_addr;
                    } else {
                        tracing::warn!(
                            "update_after_gc: null target for referent 0x{:x}",
                            e.referent
                        );
                    }
                }
                if let Some(q) = e.queue_addr {
                    if let Some(&new_q) = map.get(&q) {
                        if new_q != 0 {
                            e.queue_addr = Some(new_q);
                        } else {
                            tracing::warn!("update_after_gc: null target for queue 0x{:x}", q);
                        }
                    }
                }
            }
        }

        relocate_list(&mut self.soft_refs, pointer_map);
        relocate_list(&mut self.weak_refs, pointer_map);
        relocate_list(&mut self.phantom_refs, pointer_map);
        relocate_list(&mut self.cleaner_refs, pointer_map);
        relocate_list(&mut self.finalizer_refs, pointer_map);

        // PERF: relocation rewrote `reference_obj` on the soft refs, so the
        // address->idx index is stale. Positions in `soft_refs` did not
        // change (relocate is in-place), so rebuild the keys from the new
        // addresses. First-occurrence wins to match `discover_reference` /
        // the old linear scan. (Cheap: only runs after a compacting GC, which
        // is far rarer than `get()`.)
        self.soft_ref_addr_index.clear();
        for (idx, entry) in self.soft_refs.iter().enumerate() {
            self.soft_ref_addr_index
                .entry(entry.reference_obj)
                .or_insert(idx);
        }

        // Relocate finalization queue entries
        for addr in &mut self.finalization_queue {
            if let Some(&new_addr) = pointer_map.get(addr) {
                if new_addr != 0 {
                    *addr = new_addr;
                } else {
                    tracing::warn!(
                        "update_after_gc: null target for finalizer queue entry 0x{:x}",
                        *addr
                    );
                }
            }
        }
    }

    /// Remove entries whose `Reference` object has itself been collected.
    ///
    /// `soft_refs` is indexed by position into [`Self::soft_ref_lru_index`]
    /// (keyed `(last_access_time_ms, idx) -> idx`); after `retain` shrinks
    /// the Vec the old indices are stale and would index out of bounds in
    /// `process_soft_refs`. We rebuild the LRU index from scratch by
    /// re-walking the surviving entries so the `(timestamp, idx)` keys and
    /// stored values reflect the new positions.
    pub fn remove_collected(&mut self, is_live: &dyn Fn(usize) -> bool) {
        self.soft_refs.retain(|e| is_live(e.reference_obj));
        self.weak_refs.retain(|e| is_live(e.reference_obj));
        self.phantom_refs.retain(|e| is_live(e.reference_obj));
        self.cleaner_refs.retain(|e| is_live(e.reference_obj));
        self.finalizer_refs.retain(|e| is_live(e.reference_obj));

        // Rebuild soft_ref_lru_index to match the shrunk soft_refs Vec.
        // Without this, surviving (timestamp, old_idx) keys would point
        // past the new soft_refs.len(), causing a panic when
        // process_soft_refs indexes the BTreeMap-returned `idx`.
        //
        // PERF: rebuild the address->idx index in the same walk for the same
        // reason — `retain` shifted positions, so old `idx` values are stale.
        // First-occurrence wins (matches discovery / the old linear scan).
        self.soft_ref_lru_index.clear();
        self.soft_ref_addr_index.clear();
        for (new_idx, entry) in self.soft_refs.iter().enumerate() {
            self.soft_ref_lru_index
                .insert((entry.last_access_time_ms, new_idx), new_idx);
            self.soft_ref_addr_index
                .entry(entry.reference_obj)
                .or_insert(new_idx);
        }
    }

    /// Return reference_obj addresses of all entries whose referent was cleared.
    /// The caller should null the referent field (field 0) on each of these objects.
    pub fn cleared_ref_objects(&self) -> Vec<usize> {
        let mut result = Vec::new();
        for e in &self.soft_refs {
            if e.cleared {
                result.push(e.reference_obj);
            }
        }
        for e in &self.weak_refs {
            if e.cleared {
                result.push(e.reference_obj);
            }
        }
        // Phantom refs: Java 9+ does NOT clear the referent, but we enqueue them.
        // Cleaners: cleared flag used for cleaner actions, already handled.
        result
    }

    /// bc math-ec 0x4 ROOT-CAUSE FIX (2026-06-10): once-only variant of
    /// [`Self::cleared_ref_objects`] for the post-GC referent-null writer.
    /// Returns each cleared soft/weak Reference EXACTLY ONCE across the
    /// registry's lifetime (flagging `clear_emitted`). The legacy idempotent
    /// accessor re-emitted every cleared entry on every GC forever — and once
    /// the Reference object died, the per-cycle `set_field(.., 0,
    /// Object(None))` through its recycled/remapped address corrupted
    /// whatever innocent object reused the memory (the FixedPointTest
    /// `0x4`/silent-null corruption). A referent is nulled once; re-nulling
    /// is never needed.
    pub fn take_newly_cleared(&mut self) -> Vec<usize> {
        let mut result = Vec::new();
        for e in self.soft_refs.iter_mut().chain(self.weak_refs.iter_mut()) {
            if e.cleared && !e.clear_emitted {
                e.clear_emitted = true;
                result.push(e.reference_obj);
            }
        }
        result
    }

    // -- Finalization helpers -----------------------------------------------

    pub fn pending_finalization_count(&self) -> usize {
        self.finalization_queue.len()
    }

    pub fn dequeue_for_finalization(&mut self) -> Option<usize> {
        // Round-2 fix (GC §7): VecDeque::pop_front is O(1) vs old Vec::remove(0) which was O(n).
        self.finalization_queue.pop_front()
    }

    /// INT-8: addresses of every registered Weak/Soft/Phantom `Reference`
    /// OBJECT (not referent). The G1 marker hides slot 0 (the referent) of
    /// exactly these objects for the duration of a concurrent mark cycle so
    /// the trace cannot keep weak referents alive through their References.
    ///
    /// Deliberately EXCLUDED:
    /// - Finalizer entries — their `reference_obj` IS the finalizable object
    ///   itself; its slot 0 is an ordinary strong field.
    /// - Cleaner entries — the registered object is the cleanable, whose
    ///   layout is not the referent-at-slot-0 `Reference` shape; hiding a
    ///   strong slot would under-mark (freed-live corruption), whereas
    ///   including nothing merely over-retains cleaner referents one cycle.
    ///
    /// Cleared/enqueued entries are included: their referent slot is already
    /// null, so hiding it is a no-op either way and the set stays cheap to
    /// build (no per-entry filtering).
    pub fn reference_object_addresses(&self) -> Vec<usize> {
        self.weak_refs
            .iter()
            .chain(self.soft_refs.iter())
            .chain(self.phantom_refs.iter())
            .map(|e| e.reference_obj)
            .collect()
    }

    /// INT-8: referents of soft references that the last processing round
    /// chose to KEEP (uncleared, unenqueued). Under referent-slot hiding the
    /// marker never traced these, so a softly-only-reachable referent is
    /// unmarked even though the policy retained it — the remark driver must
    /// resurrect (mark) each one before cleanup frees regions, or the kept
    /// soft reference's `get()` would return a dangling pointer. HotSpot
    /// does the same: policy-retained soft referents are kept alive by
    /// reference processing itself.
    pub fn soft_survivor_referents(&self) -> Vec<usize> {
        self.soft_refs
            .iter()
            .filter(|e| !e.cleared && !e.enqueued)
            .map(|e| e.referent)
            .collect()
    }

    /// INT-8: addresses of registered, not-yet-fired `Cleanable` objects
    /// whose action may still run — the remark driver resurrects these so a
    /// cleanup in-place free can never invalidate a pending cleaner chain
    /// (the HotSpot equivalent is the Cleaner's internal strong list of
    /// PhantomCleanables). Two exclusions:
    /// - entries whose action already fired (`action_emitted`) — nothing
    ///   left to protect;
    /// - SELF-REFERENT entries (`reference_obj == referent`, the
    ///   finalizer-style `discover_reference(3, obj, obj, ..)` shape) —
    ///   resurrecting those would keep the referent itself alive forever
    ///   and the cleaner would never fire.
    ///
    /// Note the Cleanable's heap layout holds only the ACTION (slot 0), not
    /// the referent, so keeping it live cannot retain the referent.
    pub fn cleaner_pending_object_addresses(&self) -> Vec<usize> {
        self.cleaner_refs
            .iter()
            .filter(|e| !e.action_emitted && e.reference_obj != e.referent)
            .map(|e| e.reference_obj)
            .collect()
    }

    /// Return the referent addresses of all registered (non-cleared, non-enqueued)
    /// finalizer references.  Used before GC to add them as resurrection roots.
    pub fn finalizer_referent_addresses(&self) -> Vec<usize> {
        self.finalizer_refs
            .iter()
            .filter(|e| !e.cleared && !e.enqueued)
            .map(|e| e.referent)
            .collect()
    }

    /// Mark the finalizer entries whose CURRENT referent address appears in
    /// `referents` as enqueued.
    ///
    /// Called by the VM after the GC's resurrection channel
    /// (`collect_garbage_with_finalizers`) reports which finalizables were
    /// dead-but-resurrected: without this flag, a resurrected object looks
    /// ALIVE to the next collection's `is_marked` (it is in the pointer map),
    /// so `process_final_refs` never claims it and
    /// `finalizer_referent_addresses` keeps re-returning it — the GC then
    /// resurrects and re-enqueues it EVERY cycle and `finalize()` runs
    /// repeatedly (observed 3× per object) instead of exactly once.
    ///
    /// Must be called AFTER `update_after_gc` for the same collection, so
    /// entry referents already hold the post-GC addresses the resurrection
    /// channel reports.
    pub fn mark_finalizer_enqueued(&mut self, referents: &[usize]) {
        if referents.is_empty() {
            return;
        }
        let set: HashSet<usize> = referents.iter().copied().collect();
        for e in &mut self.finalizer_refs {
            if !e.enqueued && set.contains(&e.referent) {
                e.enqueued = true;
                self.stats.finalizer_refs_enqueued += 1;
            }
        }
    }

    // -- Stats & config -----------------------------------------------------

    pub fn stats(&self) -> &ReferenceProcessingStats {
        &self.stats
    }

    pub fn reset_stats(&mut self) {
        self.stats = ReferenceProcessingStats::default();
    }

    pub fn set_soft_ref_lru_policy(&mut self, ms_per_mb: u64) {
        self.soft_ref_lru_policy_ms_per_mb = ms_per_mb;
    }

    pub fn weak_ref_count(&self) -> usize {
        self.weak_refs.len()
    }

    pub fn soft_ref_count(&self) -> usize {
        self.soft_refs.len()
    }

    pub fn phantom_ref_count(&self) -> usize {
        self.phantom_refs.len()
    }

    /// Number of registered cleaner references (phantom-typed Cleaner entries).
    /// Exposed for NEW-17 tests verifying that `Cleaner.register` / direct
    /// buffer allocation correctly discover their cleanables with the GC.
    pub fn cleaner_ref_count(&self) -> usize {
        self.cleaner_refs.len()
    }

    /// HIB-CV-24 — `(reference_obj, referent)` for every Weak/Phantom reference
    /// that is neither cleared nor already enqueued. Used by the VM's
    /// before/after-GC referent fixup: the referent slots are nulled before a
    /// collection (so the mark phase does NOT keep them alive through the live
    /// Reference object), then survivors are restored afterwards. The addresses
    /// are the processor's current (pre-collection) view; the caller must apply
    /// the GC pointer map to locate the post-collection objects.
    ///
    /// SoftReferences are intentionally excluded — they stay strongly reachable
    /// (kept alive) so soft-cache semantics are unchanged; only weak + phantom
    /// references must allow their referent to be reclaimed.
    pub fn weak_phantom_active_pairs(&self) -> Vec<(usize, usize)> {
        let mut v = Vec::with_capacity(self.weak_refs.len() + self.phantom_refs.len());
        for e in self.weak_refs.iter().chain(self.phantom_refs.iter()) {
            if !e.cleared && !e.enqueued {
                v.push((e.reference_obj, e.referent));
            }
        }
        v
    }

    /// Queue addresses for active Weak/Phantom references. These are live
    /// through the Reference object's queue field, but a non-moving young
    /// sweep needs an identity pointer-map entry before post-GC enqueue writes.
    pub fn weak_phantom_active_queue_addrs(&self) -> Vec<usize> {
        self.weak_refs
            .iter()
            .chain(self.phantom_refs.iter())
            .filter(|e| !e.cleared && !e.enqueued)
            .filter_map(|e| e.queue_addr)
            .collect()
    }
}

impl Default for ReferenceProcessor {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// CleanerThread
// ---------------------------------------------------------------------------

/// Manages pending `Cleaner` actions that need to be invoked.
pub struct CleanerThread {
    pending_actions: Mutex<VecDeque<usize>>,
    running: AtomicBool,
}

impl CleanerThread {
    pub fn new() -> Self {
        Self {
            pending_actions: Mutex::new(VecDeque::new()),
            running: AtomicBool::new(false),
        }
    }

    pub fn submit_action(&self, addr: usize) {
        self.pending_actions.lock().push_back(addr);
    }

    pub fn drain_actions(&self) -> Vec<usize> {
        let mut lock = self.pending_actions.lock();
        lock.drain(..).collect()
    }

    /// Relocate every pending cleaner-action address through a GC pointer map.
    ///
    /// Cleaner actions can be *deferred* across GC cycles (e.g. when the only
    /// safepoint is reached from inside a JIT helper that holds the `&mut
    /// JvmThread`, running the action's `run()` there would re-enter the JIT
    /// and alias the borrow — so the interpreter leaves the actions queued and
    /// runs them at the next top-level safepoint). While queued, the cleanable
    /// objects they point at can be evacuated by a compacting collector, so
    /// their raw addresses must be remapped on every GC or a later
    /// `drain_actions` would deref freed/moved memory → SEGV.
    pub fn update_after_gc(&self, pointer_map: &HashMap<usize, usize>) {
        if pointer_map.is_empty() {
            return;
        }
        let mut lock = self.pending_actions.lock();
        for addr in lock.iter_mut() {
            if let Some(&new) = pointer_map.get(addr) {
                *addr = new;
            }
        }
    }

    pub fn pending_count(&self) -> usize {
        self.pending_actions.lock().len()
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub fn start(&self) {
        self.running.store(true, Ordering::Release);
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
    }
}

impl Default for CleanerThread {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// FinalizerThread
// ---------------------------------------------------------------------------

/// Maximum number of objects in the finalizer queue to prevent memory exhaustion.
const FINALIZER_QUEUE_MAX_CAPACITY: usize = 100_000;

/// Per-finalizer timeout in milliseconds (matches HotSpot's 2-second default).
const FINALIZER_TIMEOUT_MS: u64 = 2_000;

/// Manages the queue of objects awaiting `finalize()` execution.
///
/// Includes resurrection detection: objects that have already been finalized
/// once are tracked and will not be finalized again (per JLS §12.6).
pub struct FinalizerThread {
    finalization_queue: Mutex<VecDeque<usize>>,
    /// Set of object addresses that have already been finalized once.
    /// Prevents double-finalization from resurrection attacks.
    /// T10.9.B: FxHashSet — object addresses are internal.
    already_finalized: Mutex<FxHashSet<usize>>,
    running: AtomicBool,
    /// Counter of dropped objects due to queue overflow.
    dropped_count: AtomicUsize,
}

impl FinalizerThread {
    pub fn new() -> Self {
        Self {
            finalization_queue: Mutex::new(VecDeque::new()),
            already_finalized: Mutex::new(FxHashSet::default()),
            running: AtomicBool::new(false),
            dropped_count: AtomicUsize::new(0),
        }
    }

    /// Enqueue an object for finalization. Returns `false` if the object
    /// has already been finalized (resurrection) or the queue is full.
    pub fn enqueue(&self, obj_addr: usize) -> bool {
        // Check resurrection: skip if already finalized once
        if self.already_finalized.lock().contains(&obj_addr) {
            return false;
        }
        let mut queue = self.finalization_queue.lock();
        if queue.len() >= FINALIZER_QUEUE_MAX_CAPACITY {
            self.dropped_count.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                "Finalizer queue at capacity ({}), dropping object 0x{:x}",
                FINALIZER_QUEUE_MAX_CAPACITY,
                obj_addr
            );
            return false;
        }
        queue.push_back(obj_addr);
        true
    }

    /// Dequeue the next object for finalization, marking it as finalized.
    pub fn dequeue(&self) -> Option<usize> {
        let addr = self.finalization_queue.lock().pop_front()?;
        // Mark as finalized — prevents double-finalization on resurrection
        self.already_finalized.lock().insert(addr);
        Some(addr)
    }

    /// Check if an object has already been finalized.
    pub fn was_finalized(&self, obj_addr: usize) -> bool {
        self.already_finalized.lock().contains(&obj_addr)
    }

    /// Relocate every queued finalizable object address through a GC pointer
    /// map. Like cleaner actions, finalizers can be deferred across GC cycles
    /// (a `finalize()` invoked while a JIT borrow is live would alias the
    /// `&mut JvmThread`), so a persisted queue entry must track object motion
    /// or `dequeue` would later hand the interpreter a freed/moved address.
    pub fn update_after_gc(&self, pointer_map: &HashMap<usize, usize>) {
        if pointer_map.is_empty() {
            return;
        }
        let mut queue = self.finalization_queue.lock();
        for addr in queue.iter_mut() {
            if let Some(&new) = pointer_map.get(addr) {
                *addr = new;
            }
        }
    }

    pub fn pending_count(&self) -> usize {
        self.finalization_queue.lock().len()
    }

    pub fn dropped_count(&self) -> usize {
        self.dropped_count.load(Ordering::Relaxed)
    }

    /// Get the per-finalizer timeout in milliseconds.
    pub fn timeout_ms(&self) -> u64 {
        FINALIZER_TIMEOUT_MS
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub fn start(&self) {
        self.running.store(true, Ordering::Release);
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
    }

    /// Clean up finalization tracking for objects that have been GC'd.
    pub fn cleanup_collected(&self, is_live: &dyn Fn(usize) -> bool) {
        self.already_finalized.lock().retain(|addr| is_live(*addr));
    }
}

impl Default for FinalizerThread {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // Helpers ---------------------------------------------------------------

    fn always_dead(_addr: usize) -> bool {
        false
    }
    fn always_live(_addr: usize) -> bool {
        true
    }
    fn live_set(set: &[usize]) -> impl Fn(usize) -> bool + '_ {
        move |addr| set.contains(&addr)
    }

    // 1. Discover soft reference -------------------------------------------
    #[test]
    fn discover_soft_ref() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Soft, 100, 200, Some(300));
        assert_eq!(proc.soft_refs.len(), 1);
        assert_eq!(proc.soft_refs[0].referent, 200);
    }

    // 2. Discover weak reference -------------------------------------------
    #[test]
    fn discover_weak_ref() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 100, 200, None);
        assert_eq!(proc.weak_refs.len(), 1);
    }

    // 3. Discover phantom reference ----------------------------------------
    #[test]
    fn discover_phantom_ref() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Phantom, 100, 200, Some(300));
        assert_eq!(proc.phantom_refs.len(), 1);
    }

    // 4. Discover cleaner reference ----------------------------------------
    #[test]
    fn discover_cleaner_ref() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Cleaner, 100, 200, None);
        assert_eq!(proc.cleaner_refs.len(), 1);
    }

    // 5. Discover finalizer reference --------------------------------------
    #[test]
    fn discover_finalizer_ref() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Finalizer, 100, 200, Some(400));
        assert_eq!(proc.finalizer_refs.len(), 1);
    }

    // 6. Strong references are not tracked ---------------------------------
    #[test]
    fn strong_refs_not_tracked() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Strong, 100, 200, None);
        assert!(proc.soft_refs.is_empty());
        assert!(proc.weak_refs.is_empty());
        assert!(proc.phantom_refs.is_empty());
    }

    // 7. SoftRef cleared when time exceeds threshold -----------------------
    #[test]
    fn soft_ref_cleared_when_old() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 100, 200, Some(300));
        // last_access_time_ms = 0, current = 5000, free = 2 MB
        // threshold = 1000 * 2 = 2000; idle = 5000 > 2000 => clear
        let result = proc.process_references(&always_dead, 2, 5000);
        assert_eq!(result.stats.soft_refs_cleared, 1);
        assert!(proc.soft_refs[0].cleared);
    }

    // 8. SoftRef kept when recently accessed --------------------------------
    #[test]
    fn soft_ref_kept_when_recent() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 100, 200, Some(300));
        proc.soft_refs[0].last_access_time_ms = 4500;
        // threshold = 1000 * 10 = 10000; idle = 5000 - 4500 = 500 < 10000
        let result = proc.process_references(&always_dead, 10, 5000);
        assert_eq!(result.stats.soft_refs_cleared, 0);
    }

    // 9. SoftRef kept when referent is live --------------------------------
    #[test]
    fn soft_ref_kept_when_referent_live() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 100, 200, Some(300));
        let result = proc.process_references(&always_live, 0, 99999);
        assert_eq!(result.stats.soft_refs_cleared, 0);
    }

    // 10. WeakRef cleared when referent unreachable -------------------------
    #[test]
    fn weak_ref_cleared_when_dead() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 100, 200, Some(300));
        let result = proc.process_references(&always_dead, 100, 0);
        assert_eq!(result.stats.weak_refs_cleared, 1);
        assert!(proc.weak_refs[0].cleared);
    }

    // 11. WeakRef kept when referent reachable ------------------------------
    #[test]
    fn weak_ref_kept_when_live() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 100, 200, None);
        let result = proc.process_references(&always_live, 100, 0);
        assert_eq!(result.stats.weak_refs_cleared, 0);
    }

    // 12. PhantomRef enqueued when referent unreachable ---------------------
    #[test]
    fn phantom_ref_enqueued_when_dead() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Phantom, 100, 200, Some(300));
        let result = proc.process_references(&always_dead, 100, 0);
        assert_eq!(result.stats.phantom_refs_enqueued, 1);
        assert!(proc.phantom_refs[0].enqueued);
        assert_eq!(result.to_enqueue.len(), 1);
        assert_eq!(result.to_enqueue[0], (100, 300));
    }

    // 13. PhantomRef referent NOT cleared (Java 9+ semantics) --------------
    #[test]
    fn phantom_ref_not_cleared() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Phantom, 100, 200, Some(300));
        let _result = proc.process_references(&always_dead, 100, 0);
        assert!(!proc.phantom_refs[0].cleared);
    }

    // 14. CleanerThread submit and drain -----------------------------------
    #[test]
    fn cleaner_submit_and_drain() {
        let ct = CleanerThread::new();
        ct.submit_action(0xA000);
        ct.submit_action(0xB000);
        assert_eq!(ct.pending_count(), 2);
        let drained = ct.drain_actions();
        assert_eq!(drained, vec![0xA000, 0xB000]);
        assert_eq!(ct.pending_count(), 0);
    }

    // 15. FinalizerThread enqueue and dequeue (FIFO) -----------------------
    #[test]
    fn finalizer_enqueue_dequeue_fifo() {
        let ft = FinalizerThread::new();
        ft.enqueue(1);
        ft.enqueue(2);
        ft.enqueue(3);
        assert_eq!(ft.pending_count(), 3);
        assert_eq!(ft.dequeue(), Some(1));
        assert_eq!(ft.dequeue(), Some(2));
        assert_eq!(ft.dequeue(), Some(3));
        assert_eq!(ft.dequeue(), None);
    }

    // 16. Reference queue association & enqueue ----------------------------
    #[test]
    fn reference_queue_enqueue_poll() {
        let mut q = ReferenceQueue::new(0x5000, 10);
        assert!(q.enqueue(100));
        assert!(q.enqueue(200));
        assert_eq!(q.pending_count(), 2);
        assert_eq!(q.poll(), Some(100));
        assert_eq!(q.poll(), Some(200));
        assert!(q.is_empty());
    }

    // 17. ReferenceQueue capacity (evict-oldest policy) ----------------------
    #[test]
    fn reference_queue_capacity_evicts_oldest() {
        let mut q = ReferenceQueue::new(0x5000, 2);
        assert!(q.enqueue(1));
        assert!(q.enqueue(2));
        // At capacity: evicts oldest (1) and enqueues 3
        assert!(q.enqueue(3));
        assert_eq!(q.pending_count(), 2);
        assert_eq!(q.overflow_count(), 1);
        // Should contain 2 and 3 (1 was evicted)
        assert_eq!(q.poll(), Some(2));
        assert_eq!(q.poll(), Some(3));
    }

    // 18. update_after_gc relocates addresses ------------------------------
    #[test]
    fn update_after_gc_relocates() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 100, 200, Some(300));
        let mut map = HashMap::new();
        map.insert(100, 1100);
        map.insert(200, 1200);
        map.insert(300, 1300);
        proc.update_after_gc(&map);
        assert_eq!(proc.weak_refs[0].reference_obj, 1100);
        assert_eq!(proc.weak_refs[0].referent, 1200);
        assert_eq!(proc.weak_refs[0].queue_addr, Some(1300));
    }

    // 19. remove_collected cleans up dead Reference objects -----------------
    #[test]
    fn remove_collected_cleans_up() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 100, 200, None);
        proc.discover_reference(ReferenceType::Weak, 101, 201, None);
        let live = [101usize];
        proc.remove_collected(&live_set(&live));
        assert_eq!(proc.weak_refs.len(), 1);
        assert_eq!(proc.weak_refs[0].reference_obj, 101);
    }

    // 20. Stats tracking ---------------------------------------------------
    #[test]
    fn stats_tracking() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 10, 20, None);
        proc.discover_reference(ReferenceType::Weak, 11, 21, None);
        proc.discover_reference(ReferenceType::Phantom, 30, 40, Some(50));
        let result = proc.process_references(&always_dead, 100, 0);
        assert_eq!(result.stats.weak_refs_discovered, 2);
        assert_eq!(result.stats.weak_refs_cleared, 2);
        assert_eq!(result.stats.phantom_refs_discovered, 1);
        assert_eq!(result.stats.phantom_refs_enqueued, 1);
    }

    // 21. Multiple reference types in single processing --------------------
    #[test]
    fn mixed_reference_types() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 1, 2, Some(500));
        proc.discover_reference(ReferenceType::Weak, 3, 4, Some(500));
        proc.discover_reference(ReferenceType::Phantom, 5, 6, Some(600));
        proc.discover_reference(ReferenceType::Finalizer, 7, 8, Some(700));
        proc.discover_reference(ReferenceType::Cleaner, 9, 10, None);

        // All referents dead, soft ref old enough to clear
        let result = proc.process_references(&always_dead, 1, 5000);
        assert_eq!(result.stats.soft_refs_cleared, 1);
        assert_eq!(result.stats.weak_refs_cleared, 1);
        assert_eq!(result.stats.phantom_refs_enqueued, 1);
        assert_eq!(result.stats.finalizer_refs_enqueued, 1);
        assert_eq!(result.stats.cleaner_refs_processed, 1);
    }

    // 22. Policy configuration change --------------------------------------
    #[test]
    fn policy_configuration() {
        let mut proc = ReferenceProcessor::new();
        proc.set_soft_ref_lru_policy(500);
        proc.discover_reference(ReferenceType::Soft, 1, 2, None);
        // threshold = 500 * 4 = 2000; idle = 2500 > 2000 => clear
        let result = proc.process_references(&always_dead, 4, 2500);
        assert_eq!(result.stats.soft_refs_cleared, 1);
    }

    // 23. Empty processor returns empty result -----------------------------
    #[test]
    fn empty_processor() {
        let mut proc = ReferenceProcessor::new();
        let result = proc.process_references(&always_dead, 100, 0);
        assert!(result.to_enqueue.is_empty());
        assert!(result.to_finalize.is_empty());
        assert!(result.cleaner_actions.is_empty());
    }

    // 24. to_enqueue contains correct pairs --------------------------------
    #[test]
    fn to_enqueue_pairs() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 10, 20, Some(500));
        proc.discover_reference(ReferenceType::Weak, 11, 21, Some(600));
        let result = proc.process_references(&always_dead, 100, 0);
        assert_eq!(result.to_enqueue.len(), 2);
        // Both should be present (order may depend on HashMap iteration)
        let has_10 = result.to_enqueue.iter().any(|&(r, q)| r == 10 && q == 500);
        let has_11 = result.to_enqueue.iter().any(|&(r, q)| r == 11 && q == 600);
        assert!(has_10);
        assert!(has_11);
    }

    // 25. Finalization queue ordering (FIFO) -------------------------------
    #[test]
    fn finalization_queue_fifo() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Finalizer, 1, 100, None);
        proc.discover_reference(ReferenceType::Finalizer, 2, 200, None);
        proc.discover_reference(ReferenceType::Finalizer, 3, 300, None);
        let result = proc.process_references(&always_dead, 100, 0);
        // Once-only emission (bc math-ec 0x4 fix, 2026-06-10): process_references
        // now DRAINS the dead finalizers into the result in FIFO discovery order
        // rather than leaving them in the internal queue, so the interpreter
        // consumes them exactly once. The internal queue is therefore empty
        // afterwards.
        assert_eq!(result.to_finalize, vec![100, 200, 300]);
        assert_eq!(proc.dequeue_for_finalization(), None);
    }

    // 26. Reset stats works ------------------------------------------------
    #[test]
    fn reset_stats() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 1, 2, None);
        let _result = proc.process_references(&always_dead, 100, 0);
        assert_eq!(proc.stats().weak_refs_cleared, 1);
        proc.reset_stats();
        assert_eq!(proc.stats().weak_refs_cleared, 0);
    }

    // 27. Weak ref not enqueued without queue ------------------------------
    #[test]
    fn weak_ref_no_queue_no_enqueue() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 10, 20, None);
        let result = proc.process_references(&always_dead, 100, 0);
        assert!(result.to_enqueue.is_empty());
        assert!(proc.weak_refs[0].cleared);
        assert!(!proc.weak_refs[0].enqueued);
    }

    // 28. Selective liveness -----------------------------------------------
    #[test]
    fn selective_liveness() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 1, 100, Some(500));
        proc.discover_reference(ReferenceType::Weak, 2, 200, Some(500));
        // Only referent 100 is live
        let live = [100usize];
        let result = proc.process_references(&live_set(&live), 100, 0);
        assert_eq!(result.stats.weak_refs_cleared, 1);
        assert!(!proc.weak_refs[0].cleared); // referent 100 alive
        assert!(proc.weak_refs[1].cleared); // referent 200 dead
    }

    // 29. CleanerThread start/stop -----------------------------------------
    #[test]
    fn cleaner_thread_lifecycle() {
        let ct = CleanerThread::new();
        assert!(!ct.is_running());
        ct.start();
        assert!(ct.is_running());
        ct.stop();
        assert!(!ct.is_running());
    }

    // 30. FinalizerThread start/stop ---------------------------------------
    #[test]
    fn finalizer_thread_lifecycle() {
        let ft = FinalizerThread::new();
        assert!(!ft.is_running());
        ft.start();
        assert!(ft.is_running());
        ft.stop();
        assert!(!ft.is_running());
    }

    // 31. update_after_gc relocates finalization queue ----------------------
    #[test]
    fn update_after_gc_finalization_queue() {
        let mut proc = ReferenceProcessor::new();
        // process_references now DRAINS the finalization queue into its result
        // (once-only emission, see finalization_queue_fifo), so populate the
        // internal queue directly to isolate update_after_gc's relocation of a
        // still-pending entry (an object moved by a GC between enqueue and
        // consumption).
        proc.finalization_queue.push_back(100);
        let mut map = HashMap::new();
        map.insert(100, 9999);
        proc.update_after_gc(&map);
        assert_eq!(proc.finalization_queue, vec![9999]);
    }

    // 32. Already-cleared entry skipped ------------------------------------
    #[test]
    fn already_cleared_skipped() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 1, 2, Some(500));
        proc.weak_refs[0].cleared = true;
        let result = proc.process_references(&always_dead, 100, 0);
        assert_eq!(result.stats.weak_refs_cleared, 0);
    }

    // 33. Phantom not re-enqueued ------------------------------------------
    #[test]
    fn phantom_not_re_enqueued() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Phantom, 1, 2, Some(500));
        let r1 = proc.process_references(&always_dead, 100, 0);
        assert_eq!(r1.stats.phantom_refs_enqueued, 1);
        // Process again -- should not double-enqueue
        let r2 = proc.process_references(&always_dead, 100, 0);
        assert_eq!(r2.stats.phantom_refs_enqueued, 0);
    }

    // 34. Default trait impl -----------------------------------------------
    #[test]
    fn default_impl() {
        let proc = ReferenceProcessor::default();
        assert_eq!(proc.soft_ref_lru_policy_ms_per_mb, 1000);
    }

    // 35. to_finalize populated correctly ----------------------------------
    #[test]
    fn to_finalize_populated() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Finalizer, 1, 0xBEEF, None);
        let result = proc.process_references(&always_dead, 100, 0);
        assert_eq!(result.to_finalize, vec![0xBEEF]);
    }

    // ======================================================================
    // M16-M20 fix verification tests
    // ======================================================================

    // 36. FinalizerThread resurrection detection (one-finalization-per-object)
    #[test]
    fn finalizer_resurrection_detection() {
        let ft = FinalizerThread::new();
        // First finalization: enqueue succeeds
        assert!(ft.enqueue(0xDEAD));
        assert_eq!(ft.pending_count(), 1);
        // Dequeue marks it as finalized
        let addr = ft.dequeue().unwrap();
        assert_eq!(addr, 0xDEAD);
        assert!(ft.was_finalized(0xDEAD));
        // Re-enqueue after resurrection: should be rejected
        assert!(!ft.enqueue(0xDEAD));
        assert_eq!(ft.pending_count(), 0);
    }

    // 37. FinalizerThread queue capacity limit
    #[test]
    fn finalizer_queue_capacity_limit() {
        let ft = FinalizerThread::new();
        // Fill to capacity
        for i in 0..FINALIZER_QUEUE_MAX_CAPACITY {
            assert!(ft.enqueue(i + 1), "enqueue #{i} should succeed");
        }
        assert_eq!(ft.pending_count(), FINALIZER_QUEUE_MAX_CAPACITY);
        // One more should be dropped
        assert!(!ft.enqueue(FINALIZER_QUEUE_MAX_CAPACITY + 1));
        assert_eq!(ft.dropped_count(), 1);
        assert_eq!(ft.pending_count(), FINALIZER_QUEUE_MAX_CAPACITY);
    }

    // 38. FinalizerThread was_finalized tracks dequeued objects
    #[test]
    fn finalizer_was_finalized_tracking() {
        let ft = FinalizerThread::new();
        assert!(!ft.was_finalized(100));
        ft.enqueue(100);
        // Still not finalized until dequeued
        assert!(!ft.was_finalized(100));
        ft.dequeue();
        assert!(ft.was_finalized(100));
    }

    // 39. FinalizerThread cleanup_collected removes dead entries from tracking
    #[test]
    fn finalizer_cleanup_collected() {
        let ft = FinalizerThread::new();
        ft.enqueue(100);
        ft.enqueue(200);
        ft.dequeue(); // 100 -> finalized
        ft.dequeue(); // 200 -> finalized
        assert!(ft.was_finalized(100));
        assert!(ft.was_finalized(200));
        // Simulate GC: only 200 is still live
        ft.cleanup_collected(&|addr| addr == 200);
        assert!(!ft.was_finalized(100)); // cleaned up
        assert!(ft.was_finalized(200)); // still tracked
    }

    // 40. FinalizerThread timeout constant
    #[test]
    fn finalizer_timeout_ms() {
        let ft = FinalizerThread::new();
        assert_eq!(ft.timeout_ms(), 2_000);
    }

    // 41. ReferenceQueue overflow counter tracks multiple overflows
    #[test]
    fn reference_queue_overflow_counter() {
        let mut q = ReferenceQueue::new(0x1000, 1);
        q.enqueue(1);
        q.enqueue(2); // overflow #1, evicts 1
        q.enqueue(3); // overflow #2, evicts 2
        assert_eq!(q.overflow_count(), 2);
        assert_eq!(q.pending_count(), 1);
        assert_eq!(q.poll(), Some(3));
    }

    // 42. ReferenceQueue remove_timeout returns immediately if data available
    #[test]
    fn reference_queue_remove_timeout_immediate() {
        let mut q = ReferenceQueue::new(0x2000, 10);
        q.enqueue(42);
        // Should return immediately, not wait
        let start = std::time::Instant::now();
        let val = q.remove_timeout(5000);
        assert!(
            start.elapsed().as_millis() < 100,
            "should return immediately"
        );
        assert_eq!(val, Some(42));
    }

    // 43. ReferenceQueue remove_timeout returns None on empty queue
    #[test]
    fn reference_queue_remove_timeout_empty() {
        let mut q = ReferenceQueue::new(0x3000, 10);
        let start = std::time::Instant::now();
        let val = q.remove_timeout(50); // 50ms timeout
        let elapsed = start.elapsed().as_millis();
        assert!(val.is_none());
        assert!(
            elapsed >= 40,
            "should wait close to timeout duration: {elapsed}ms"
        );
    }

    // 44. update_after_gc skips null target addresses (pointer validation)
    #[test]
    fn update_after_gc_skips_null_targets() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 100, 200, Some(300));
        let mut map = HashMap::new();
        // Map reference_obj to 0 (invalid) — should be skipped
        map.insert(100usize, 0usize);
        // Map referent to valid address
        map.insert(200, 1200);
        // Map queue to 0 (invalid) — should be skipped
        map.insert(300, 0);
        proc.update_after_gc(&map);
        // reference_obj and queue should remain unchanged (null target skipped)
        assert_eq!(proc.weak_refs[0].reference_obj, 100);
        assert_eq!(proc.weak_refs[0].referent, 1200);
        assert_eq!(proc.weak_refs[0].queue_addr, Some(300));
    }

    // 45. update_after_gc skips null for finalization queue entries
    #[test]
    fn update_after_gc_finalization_null_skipped() {
        let mut proc = ReferenceProcessor::new();
        // Populate the queue directly — process_references now drains it (see
        // finalization_queue_fifo) — to isolate update_after_gc's null-target
        // skip behaviour.
        proc.finalization_queue.push_back(0xBEEF);
        let mut map = HashMap::new();
        map.insert(0xBEEF, 0usize); // null target
        proc.update_after_gc(&map);
        // Should NOT have been relocated to 0
        assert_eq!(proc.finalization_queue, vec![0xBEEF]);
    }

    // 46. update_after_gc relocates across all reference types
    #[test]
    fn update_after_gc_all_ref_types() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Soft, 10, 20, Some(30));
        proc.discover_reference(ReferenceType::Weak, 40, 50, None);
        proc.discover_reference(ReferenceType::Phantom, 60, 70, Some(80));
        proc.discover_reference(ReferenceType::Cleaner, 90, 100, None);
        proc.discover_reference(ReferenceType::Finalizer, 110, 120, Some(130));

        let mut map = HashMap::new();
        for old in (10..=130).step_by(10) {
            map.insert(old, old + 1000);
        }
        proc.update_after_gc(&map);

        assert_eq!(proc.soft_refs[0].reference_obj, 1010);
        assert_eq!(proc.soft_refs[0].referent, 1020);
        assert_eq!(proc.soft_refs[0].queue_addr, Some(1030));
        assert_eq!(proc.weak_refs[0].reference_obj, 1040);
        assert_eq!(proc.weak_refs[0].referent, 1050);
        assert_eq!(proc.phantom_refs[0].reference_obj, 1060);
        assert_eq!(proc.phantom_refs[0].queue_addr, Some(1080));
        assert_eq!(proc.cleaner_refs[0].reference_obj, 1090);
        assert_eq!(proc.finalizer_refs[0].reference_obj, 1110);
        assert_eq!(proc.finalizer_refs[0].queue_addr, Some(1130));
    }

    // 47. ReferenceQueue remove_blocking delegates to remove_timeout(60s)
    #[test]
    fn reference_queue_remove_blocking_with_data() {
        let mut q = ReferenceQueue::new(0x4000, 10);
        q.enqueue(99);
        let val = q.remove_blocking();
        assert_eq!(val, Some(99));
    }

    // 48. FinalizerThread multiple resurrections all blocked
    #[test]
    fn finalizer_multiple_resurrections_blocked() {
        let ft = FinalizerThread::new();
        ft.enqueue(0xA);
        ft.enqueue(0xB);
        ft.dequeue(); // finalizes 0xA
        ft.dequeue(); // finalizes 0xB
                      // Both resurrections blocked
        assert!(!ft.enqueue(0xA));
        assert!(!ft.enqueue(0xB));
        // New objects still accepted
        assert!(ft.enqueue(0xC));
        assert_eq!(ft.pending_count(), 1);
    }

    // 49. FinalizerThread dropped_count starts at zero
    #[test]
    fn finalizer_dropped_count_initially_zero() {
        let ft = FinalizerThread::new();
        assert_eq!(ft.dropped_count(), 0);
    }

    // ======================================================================
    // V18: finalizer-reachable closure protects weak/soft refs
    // ======================================================================

    // 50. A weak ref whose referent is itself a to-be-finalized object must
    //     NOT be cleared in the same cycle the finalizer is enqueued
    //     (single-hop closure, no tracer needed).
    #[test]
    fn v18_weak_ref_to_finalizable_object_not_cleared() {
        let mut proc = ReferenceProcessor::new();
        // finalizable object lives at 0x200; nothing is marked.
        proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x200, None);
        // weak ref points directly at the finalizable object 0x200.
        proc.discover_reference(ReferenceType::Weak, 0x300, 0x200, Some(0x400));

        let result = proc.process_references(&always_dead, 64, 0);

        // The finalizer is enqueued for the object...
        assert_eq!(result.stats.finalizer_refs_enqueued, 1);
        // ...but the weak ref to that same object must survive this cycle.
        assert_eq!(result.stats.weak_refs_cleared, 0);
        assert!(!proc.weak_refs[0].cleared);
    }

    // 51. With a tracer, a weak ref reachable only transitively (depth 2)
    //     through a finalizable object is also protected.
    #[test]
    fn v18_transitive_closure_protects_weak_ref_with_tracer() {
        let mut proc = ReferenceProcessor::new();
        // 0x200 is finalizable; 0x500 is reachable only via 0x200's fields.
        proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x200, None);
        proc.discover_reference(ReferenceType::Weak, 0x300, 0x500, Some(0x400));

        // Tracer: from root 0x200, reach {0x200, 0x500}.
        let trace = |roots: &[usize]| -> Vec<usize> {
            let mut out = roots.to_vec();
            if roots.contains(&0x200) {
                out.push(0x500);
            }
            out
        };

        let result =
            proc.process_references_with_finalizer_trace(&always_dead, Some(&trace), 64, 0);

        assert_eq!(result.stats.finalizer_refs_enqueued, 1);
        // Transitively reachable referent must not be cleared this cycle.
        assert_eq!(result.stats.weak_refs_cleared, 0);
        assert!(!proc.weak_refs[0].cleared);
    }

    // 52. Residual gap: WITHOUT a tracer, a transitively-reachable (depth 2)
    //     weak referent is still cleared. This documents the known gap that
    //     requires an out-of-scope caller change to pass `trace_from`.
    #[test]
    fn v18_residual_gap_transitive_without_tracer() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x200, None);
        // weak referent 0x500 is NOT a direct finalizer referent.
        proc.discover_reference(ReferenceType::Weak, 0x300, 0x500, None);

        // Legacy entry point (no tracer): only direct referents protected.
        let result = proc.process_references(&always_dead, 64, 0);
        assert_eq!(result.stats.weak_refs_cleared, 1);
    }

    // 53. Closure does not resurrect already-marked finalizers as roots, and
    //     ordinary weak clearing of unrelated dead referents still happens.
    #[test]
    fn v18_unrelated_weak_ref_still_cleared() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x200, None);
        // weak ref to a totally unrelated dead object 0x999.
        proc.discover_reference(ReferenceType::Weak, 0x300, 0x999, None);

        let result = proc.process_references(&always_dead, 64, 0);
        assert_eq!(result.stats.weak_refs_cleared, 1);
        assert!(proc.weak_refs[0].cleared);
    }

    // ======================================================================
    // LRU-index leak fix: cleared soft refs are pruned from soft_ref_lru_index
    // ======================================================================

    // 54. Clearing a soft ref removes its key from the LRU index so the index
    //     stays in sync with the live (uncleared) soft refs and is not
    //     re-scanned on subsequent GCs.
    #[test]
    fn soft_ref_lru_index_pruned_on_clear() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 100, 200, Some(300));
        // One live key in the index after discovery.
        assert_eq!(proc.soft_ref_lru_index.len(), 1);
        // threshold = 1000 * 2 = 2000; idle = 5000 > 2000 => clear.
        let result = proc.process_references(&always_dead, 2, 5000);
        assert_eq!(result.stats.soft_refs_cleared, 1);
        assert!(proc.soft_refs[0].cleared);
        // The cleared entry's key must have been removed from the LRU index.
        assert_eq!(
            proc.soft_ref_lru_index.len(),
            0,
            "cleared soft ref left a stale LRU-index entry"
        );
    }

    // 55. A surviving soft ref keeps its LRU key while a sibling is cleared;
    //     ordering/keys of survivors are preserved.
    #[test]
    fn soft_ref_lru_index_keeps_survivors() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        // idx 0 referent dead, idx 1 referent live.
        proc.discover_reference(ReferenceType::Soft, 100, 200, Some(300));
        proc.discover_reference(ReferenceType::Soft, 101, 201, Some(301));
        assert_eq!(proc.soft_ref_lru_index.len(), 2);
        // Only referent 201 stays live; threshold small so the dead one clears.
        let live = [201usize];
        let result = proc.process_references(&live_set(&live), 1, 5000);
        assert_eq!(result.stats.soft_refs_cleared, 1);
        assert!(proc.soft_refs[0].cleared);
        assert!(!proc.soft_refs[1].cleared);
        // Exactly one (survivor) key remains, and it is the survivor's key.
        assert_eq!(proc.soft_ref_lru_index.len(), 1);
        assert!(proc
            .soft_ref_lru_index
            .contains_key(&(proc.soft_refs[1].last_access_time_ms, 1)));
    }

    // 56. Re-processing after a clear does not re-walk the cleared entry: the
    //     index no longer contains its key, and a second pass is a no-op.
    #[test]
    fn soft_ref_lru_index_no_rescan_after_clear() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 100, 200, Some(300));
        let r1 = proc.process_references(&always_dead, 2, 5000);
        assert_eq!(r1.stats.soft_refs_cleared, 1);
        assert_eq!(proc.soft_ref_lru_index.len(), 0);
        // Second pass: nothing to clear (already cleared) and index stays empty.
        let r2 = proc.process_references(&always_dead, 2, 5000);
        assert_eq!(r2.stats.soft_refs_cleared, 0);
        assert_eq!(proc.soft_ref_lru_index.len(), 0);
    }

    // ======================================================================
    // PERF: O(1) touch_soft_reference via address index
    // ======================================================================

    // 57. touch_soft_reference re-keys the matching entry in the LRU index
    //     using the O(1) address index (same effect the old linear scan had).
    #[test]
    fn touch_soft_reference_rekeys_lru_index() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0xAA, 0x10, Some(0x100));
        proc.discover_reference(ReferenceType::Soft, 0xBB, 0x20, Some(0x200));
        // Initial keys are both at timestamp 0.
        assert!(proc.soft_ref_lru_index.contains_key(&(0, 0)));
        assert!(proc.soft_ref_lru_index.contains_key(&(0, 1)));

        // Touch the second soft ref (idx 1) to time 5000.
        proc.touch_soft_reference(0xBB, 5000);
        assert_eq!(proc.soft_refs[1].last_access_time_ms, 5000);
        // Old key removed, new key inserted; first entry untouched.
        assert!(!proc.soft_ref_lru_index.contains_key(&(0, 1)));
        assert!(proc.soft_ref_lru_index.contains_key(&(5000, 1)));
        assert!(proc.soft_ref_lru_index.contains_key(&(0, 0)));
        // Index size unchanged (re-key, not add).
        assert_eq!(proc.soft_ref_lru_index.len(), 2);
    }

    // INT-8: reference_object_addresses returns weak/soft/phantom Reference
    // OBJECT addresses (cleared included), never finalizer/cleaner entries.
    #[test]
    fn reference_object_addresses_covers_weak_soft_phantom_only() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, None);
        proc.discover_reference(ReferenceType::Soft, 0x20, 0x21, None);
        proc.discover_reference(ReferenceType::Phantom, 0x30, 0x31, Some(0x99));
        proc.discover_reference(ReferenceType::Finalizer, 0x40, 0x40, None);
        proc.discover_reference(ReferenceType::Cleaner, 0x50, 0x51, None);
        let mut addrs = proc.reference_object_addresses();
        addrs.sort_unstable();
        assert_eq!(addrs, vec![0x10, 0x20, 0x30]);
    }

    // INT-8: cleaner_pending_object_addresses excludes self-referent
    // (finalizer-style) registrations and already-fired actions.
    #[test]
    fn cleaner_pending_object_addresses_excludes_self_and_fired() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Cleaner, 0x50, 0x51, None); // pending
        proc.discover_reference(ReferenceType::Cleaner, 0x60, 0x60, None); // self-referent
        proc.discover_reference(ReferenceType::Cleaner, 0x70, 0x71, None); // will fire
        // Fire 0x70's action: its referent 0x71 is dead.
        let result = proc.process_references(&|addr| addr != 0x71, 64, 0);
        assert!(result.cleaner_actions.contains(&0x70));
        let addrs = proc.cleaner_pending_object_addresses();
        assert_eq!(addrs, vec![0x50]);
    }

    // INT-8: soft_survivor_referents returns kept (uncleared, unenqueued)
    // soft referents — the set the remark driver must resurrect.
    #[test]
    fn soft_survivor_referents_tracks_kept_entries() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Soft, 0x20, 0x21, None);
        proc.discover_reference(ReferenceType::Soft, 0x30, 0x31, None);
        // Plenty of free heap: policy keeps both even though 0x31 is dead.
        let _ = proc.process_references(&|addr| addr != 0x31, 1024, 0);
        let mut kept = proc.soft_survivor_referents();
        kept.sort_unstable();
        // Whatever the policy decided, the accessor must mirror the
        // uncleared set exactly.
        let expected: Vec<usize> = proc
            .soft_refs
            .iter()
            .filter(|e| !e.cleared && !e.enqueued)
            .map(|e| e.referent)
            .collect();
        let mut expected_sorted = expected;
        expected_sorted.sort_unstable();
        assert_eq!(kept, expected_sorted);
        assert!(kept.contains(&0x21), "live soft referent is always kept");
    }

    // 58. touch on an unknown address is a no-op (O(1) miss, no scan needed).
    #[test]
    fn touch_soft_reference_unknown_addr_noop() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0xAA, 0x10, None);
        proc.touch_soft_reference(0xDEAD, 5000);
        assert_eq!(proc.soft_refs[0].last_access_time_ms, 0);
        assert_eq!(proc.soft_ref_lru_index.len(), 1);
    }

    // 59. touch with an unadvanced timestamp is a no-op (preserves the
    //     "only re-insert when the timer actually advances" fast path).
    #[test]
    fn touch_soft_reference_same_timestamp_noop() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0xAA, 0x10, None);
        proc.touch_soft_reference(0xAA, 7000);
        // Touching again with the same time must not churn the index.
        proc.touch_soft_reference(0xAA, 7000);
        assert_eq!(proc.soft_refs[0].last_access_time_ms, 7000);
        assert!(proc.soft_ref_lru_index.contains_key(&(7000, 0)));
        assert_eq!(proc.soft_ref_lru_index.len(), 1);
    }

    // 60. After remove_collected compacts soft_refs, the address index is
    //     rebuilt so touch still resolves the (now-shifted) survivor.
    #[test]
    fn touch_soft_reference_after_remove_collected() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        // idx 0 (0xAA) will be collected; idx 1 (0xBB) survives -> shifts to 0.
        proc.discover_reference(ReferenceType::Soft, 0xAA, 0x10, None);
        proc.discover_reference(ReferenceType::Soft, 0xBB, 0x20, None);
        let live = [0xBBusize];
        proc.remove_collected(&live_set(&live));
        assert_eq!(proc.soft_refs.len(), 1);
        assert_eq!(proc.soft_refs[0].reference_obj, 0xBB);

        // Touch the survivor at its NEW position; LRU key must use idx 0.
        proc.touch_soft_reference(0xBB, 9000);
        assert_eq!(proc.soft_refs[0].last_access_time_ms, 9000);
        assert!(proc.soft_ref_lru_index.contains_key(&(9000, 0)));
        assert_eq!(proc.soft_ref_lru_index.len(), 1);
        // The collected entry's address no longer resolves.
        proc.touch_soft_reference(0xAA, 9999);
        assert_eq!(proc.soft_ref_lru_index.len(), 1);
    }

    // 61. After update_after_gc relocates reference_obj, the address index is
    //     rebuilt so touch resolves the NEW address (not the stale one).
    #[test]
    fn touch_soft_reference_after_update_after_gc() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0xAA, 0x10, None);
        let mut map = HashMap::new();
        map.insert(0xAAusize, 0xCCusize); // reference_obj 0xAA -> 0xCC
        proc.update_after_gc(&map);
        assert_eq!(proc.soft_refs[0].reference_obj, 0xCC);

        // Old address must no longer resolve; new address must.
        proc.touch_soft_reference(0xAA, 1234);
        assert_eq!(proc.soft_refs[0].last_access_time_ms, 0);
        proc.touch_soft_reference(0xCC, 1234);
        assert_eq!(proc.soft_refs[0].last_access_time_ms, 1234);
        assert!(proc.soft_ref_lru_index.contains_key(&(1234, 0)));
    }

    // ======================================================================
    // JLS reachability ordering (strong > soft > weak): a weak ref must not be
    // cleared while its referent is still softly reachable through a surviving
    // soft reference.
    // ======================================================================

    // 62. A weak ref to an object reachable ONLY transitively (depth 2) through
    //     a surviving soft referent must NOT be cleared (requires a tracer).
    #[test]
    fn weak_ref_kept_alive_by_surviving_soft_referent_transitive() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        // Soft referent 0x200 is recently accessed, so Phase 1 retains it.
        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, Some(0x180));
        proc.touch_soft_reference(0x100, 5000);
        // Weak referent 0x500 is reachable only via 0x200's fields (depth 2).
        proc.discover_reference(ReferenceType::Weak, 0x300, 0x500, Some(0x400));

        // Tracer: from the surviving soft referent 0x200 reach {0x200, 0x500}.
        let trace = |roots: &[usize]| -> Vec<usize> {
            let mut out = roots.to_vec();
            if roots.contains(&0x200) {
                out.push(0x500);
            }
            out
        };

        // free_heap large + recent access => soft ref survives Phase 1.
        let result =
            proc.process_references_with_finalizer_trace(&always_dead, Some(&trace), 64, 5000);

        // The soft ref survived...
        assert_eq!(result.stats.soft_refs_cleared, 0);
        assert!(!proc.soft_refs[0].cleared);
        // ...so the transitively-reachable weak referent must NOT be cleared.
        assert_eq!(result.stats.weak_refs_cleared, 0);
        assert!(!proc.weak_refs[0].cleared);
    }

    // 63. A weak ref pointing DIRECTLY at a surviving soft referent must not be
    //     cleared even without a tracer (single-hop soft keep-alive).
    #[test]
    fn weak_ref_to_surviving_soft_referent_not_cleared_no_tracer() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        // Soft referent 0x200 retained (recent access, plenty of free heap).
        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, Some(0x180));
        proc.touch_soft_reference(0x100, 5000);
        // Weak ref points straight at the still-soft-reachable object 0x200.
        proc.discover_reference(ReferenceType::Weak, 0x300, 0x200, Some(0x400));

        let result = proc.process_references(&always_dead, 64, 5000);

        assert_eq!(result.stats.soft_refs_cleared, 0);
        assert_eq!(result.stats.weak_refs_cleared, 0);
        assert!(!proc.weak_refs[0].cleared);
    }

    // 64. When the soft ref is itself CLEARED in Phase 1 (idle past the LRU
    //     threshold), its dead referent must NOT protect a weak ref to it.
    #[test]
    fn weak_ref_not_protected_when_soft_ref_cleared() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        // last_access_time_ms = 0, current = 5000, free = 1 MB => threshold
        // 1000; idle 5000 > 1000 => soft ref cleared in Phase 1.
        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, Some(0x180));
        proc.discover_reference(ReferenceType::Weak, 0x300, 0x200, Some(0x400));

        let result = proc.process_references(&always_dead, 1, 5000);

        assert_eq!(result.stats.soft_refs_cleared, 1);
        assert!(proc.soft_refs[0].cleared);
        // The soft ref no longer keeps 0x200 alive, so the weak ref clears.
        assert_eq!(result.stats.weak_refs_cleared, 1);
        assert!(proc.weak_refs[0].cleared);
    }

    // 65. The soft-survivor closure does not over-retain: an unrelated dead
    //     weak referent is still cleared while a soft ref survives.
    #[test]
    fn unrelated_weak_ref_still_cleared_with_surviving_soft_ref() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, Some(0x180));
        proc.touch_soft_reference(0x100, 5000); // soft ref survives
                                                // Weak ref to a totally unrelated dead object, not reachable from 0x200.
        proc.discover_reference(ReferenceType::Weak, 0x300, 0x999, Some(0x400));

        let trace = |roots: &[usize]| -> Vec<usize> { roots.to_vec() }; // 0x200 -> {0x200}
        let result =
            proc.process_references_with_finalizer_trace(&always_dead, Some(&trace), 64, 5000);

        assert_eq!(result.stats.soft_refs_cleared, 0);
        assert_eq!(result.stats.weak_refs_cleared, 1);
        assert!(proc.weak_refs[0].cleared);
    }
}
