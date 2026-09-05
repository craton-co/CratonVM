// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Java reference processing for the garbage collector.
//!
//! Implements the four reference types defined by `java.lang.ref`:
//! [`SoftReference`], [`WeakReference`], [`PhantomReference`], and the
//! internal [`Cleaner`] / [`FinalReference`] types used by the JDK.
//!
//! Processing order matches HotSpot, and reachability is recomputed between
//! phases so each level only sees objects the stronger levels have released:
//! 1. SoftReferences  -- cleared only under memory pressure (LRU policy),
//!    honouring the finalizer-reachable closure;
//! 2. WeakReferences  -- cleared when the referent is unreachable, honouring
//!    the finalizer-reachable AND soft-reachable closures;
//! 3. FinalReferences -- enqueued so the finalizer thread can run `finalize()`,
//!    honouring the soft-reachable closure (soft reachability blocks
//!    finalization; finalizer reachability deliberately does not, so mutually
//!    reachable finalizables are enqueued together);
//! 4. PhantomReferences and `Cleaner`s -- enqueued/fired only once the referent
//!    is neither soft- nor weak-reachable AND has already been finalized
//!    (Java 9+: the phantom referent is NOT cleared).
//!
//! # Cost and locking
//!
//! Everything here runs inside the collector's stop-the-world pause, under the
//! single global `ref_processor` mutex (L7 in `vm/src/runtime/lock_order.rs`).
//! The lock therefore serialises mutator-side `discover_reference` /
//! `touch_soft_reference` against each other, not against the GC.
//!
//! Per-collection cost is **O(registered references)**, not O(live references):
//! phases 2-4 are flat scans over `weak_refs` / `cleaner_refs` /
//! `finalizer_refs` / `phantom_refs`, and an already-cleared or already-enqueued
//! entry is skipped but still visited. Only phase 1 is sublinear — the
//! `soft_ref_lru_index` BTreeMap range visits just the entries idle enough to be
//! clear candidates. The lists are bounded by `remove_collected`, which drops
//! entries whose `Reference` OBJECT died, so the scans stay proportional to
//! *live Reference objects* rather than to every reference ever created. A
//! `WeakHashMap` with a million live entries still costs a million-entry scan
//! per collection; see `arch-2026-07-26/refs-metaspace-unloading.md`
//! for the "cleared entries could be segregated into a cold list" sketch.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::gc_flags;
use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};

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
    /// / silent-null corruption; see gaps/h2-testscript-segv-findings.md).
    /// `clear_emitted`: the referent-null for this entry was already handed out.
    pub clear_emitted: bool,
    /// `action_emitted`: the cleaner action for this entry was already handed out.
    pub action_emitted: bool,
    /// PHANTOM entries only: this reference is a `jdk.internal.ref.Cleaner`, so
    /// clearing it must RUN it rather than enqueue it.
    ///
    /// The JDK draws exactly this distinction inside its `ReferenceHandler`
    /// thread: an ordinary phantom goes on its `ReferenceQueue`, while a
    /// `Cleaner` — whose queue is a private `dummyQueue` nothing ever polls —
    /// has `clean()` invoked directly. It has to stay a *phantom* entry rather
    /// than move to [`ReferenceType::Cleaner`], because only the phantom list
    /// gets its referent nulled before marking
    /// (`weakref_null_referents_pre_gc` -> `weak_phantom_active_pairs`): a
    /// `Cleaner` is reachable forever from its class's own static list, so
    /// without that nulling its referent is strongly reachable through it and
    /// can never die — a leak with no symptom until something counts the bytes.
    pub runs_cleaner: bool,
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
            // This is data loss: a Reference the application enqueued is
            // discarded, and the only other record is `overflow_count()`,
            // whose sole callers are this file's own tests. As `debug!`
            // it could not print in a release build at all
            // (`release_max_level_info`), so a program losing references
            // had no way to find out.
            //
            // Rate-limited to the first and then each doubling: once a
            // queue is full every subsequent enqueue overflows, and an
            // unconditional warn would turn data loss into a log flood.
            if self.overflow_count == 1 || self.overflow_count.is_power_of_two() {
                tracing::warn!(
                    "ReferenceQueue 0x{:x} overflow: evicted oldest entry (total overflows: {}). \
                     The application will never observe the dropped reference(s).",
                    self.queue_addr,
                    self.overflow_count
                );
            }
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
    soft_ref_addr_index: cratonvm_types::PointerMap,

    /// Highest wall-clock millisecond value the *mutator* side has ever handed
    /// this processor through [`Self::touch_soft_reference`].
    ///
    /// SOFT-POLICY FIX (2026-07-26): the soft-reference LRU compares two
    /// numbers that came from different places, and in production they were
    /// on different clocks, which disabled the policy outright.
    ///
    /// * `last_access_time_ms` on each soft entry is stamped by
    ///   `NativeContext::touch_soft_reference` (`vm/src/vm/vm_exec.rs`), which
    ///   uses `SystemTime::now()` — i.e. real Unix-epoch milliseconds
    ///   (~1.7e12). It fires from `SoftReference.<init>` *and* every
    ///   `SoftReference.get()` (`native-builtins/src/reference.rs`).
    /// * `current_time_ms`, the argument to [`Self::process_references`], is a
    ///   hard-coded `0` at BOTH production call sites
    ///   (`vm/src/runtime/interpreter.rs`, the post-GC path and the G1
    ///   final-remark path).
    ///
    /// With `current_time_ms == 0` the cutoff in [`Self::process_soft_refs`] is
    /// `0.saturating_sub(threshold) == 0`, so the BTreeMap range selects only
    /// entries still at timestamp `0`, and for those `idle_ms` is also `0` —
    /// never greater than the threshold. Net effect: **no SoftReference could
    /// ever be cleared on either VM path**, so soft references behaved exactly
    /// like strong ones and `OutOfMemoryError` was reached with a heap full of
    /// reclaimable soft-reachable objects. (Independently observed in
    /// `fixed-suite-bugs/springboot/
    /// core39-clusterD-lifecycle-ssl-validation-FIXED.md`: "`process_soft_refs`
    /// never ran even once during the whole failing run".)
    ///
    /// Rather than depend on a caller-side change in a file this module does
    /// not own, the processor now tracks the mutator clock itself and uses
    /// `max(current_time_ms, last_observed_clock_ms)` as "now". A caller that
    /// supplies a coherent clock (ZGC's backend, and every unit/integration
    /// test) is unaffected, because its `current_time_ms` already dominates.
    /// A caller that supplies `0` gets the real clock instead of a dead policy.
    last_observed_clock_ms: u64,

    /// `reference_obj` addresses of the SOFT entries the *pre-collection* pass
    /// condemned for this cycle, i.e. the ones whose referent slot the VM
    /// nulled before the marker ran (see [`Self::condemn_idle_soft_refs`]).
    ///
    /// Rewritten wholesale by every `condemn_idle_soft_refs` call, which is
    /// the reset point: the set describes one collection and must never span
    /// two. A cycle that skips the pre-collection pass entirely therefore sees
    /// the previous cycle's set, and that is deliberately harmless — an entry
    /// in it is only acted on when `is_marked(referent)` is *false*, and for a
    /// reference whose slot was never nulled that means the referent genuinely
    /// died on its own.
    /// `reference_obj -> the identity hash the object carried when this
    /// processor discovered it`.
    ///
    /// THE SAME-CLASS HOLE. Every write this processor's consumers perform
    /// goes through a raw address, and the guards around them are SHAPE tests:
    /// "does the object at this address still look like a `Reference`?". A
    /// shape test cannot see the case where a reclaimed `Reference`'s address
    /// is re-issued to ANOTHER `Reference` -- and H2 allocates
    /// `org.h2.util.CloseWatcher` (a `PhantomReference`) per connection, so
    /// that case is not hypothetical. Measured on the fixed VM: over 3200
    /// phantom references through one queue on 8 threads, 2-5 of them per run
    /// arrive as a DIFFERENT, half-constructed instance of the same class.
    ///
    /// The identity hash is the exact test the shape test approximates. It is
    /// minted from a monotonic counter (`VmHeap::identity_hash_code`), lives in
    /// the object's own mark word, and travels with the object when a collector
    /// relocates it -- so it is the "monotonic registration id written into the
    /// object" the write-up asked for, and it already exists. A reused address
    /// answers with a different hash (or is unhashed and mints a fresh one,
    /// which is likewise different), so the mismatch is decisive.
    ///
    /// `0` and a missing key both mean UNSTAMPED, and unstamped falls back to
    /// the shape guards rather than declining: the in-tree tests construct
    /// entries with no heap behind them, and a stamp that defaulted to
    /// "refuse" would silently stop reference processing there.
    identity_stamps: FxHashMap<usize, i32>,

    /// The CLASS of each entry's referent, keyed by the same `reference_obj`
    /// address as [`Self::identity_stamps`].
    ///
    /// The identity stamp above proves the REFERENCE object is the one that
    /// was discovered. Nothing proved the same about the REFERENT, and the
    /// post-GC restore pass writes it into slot 0 — so an address that was
    /// freed and handed to a different object made that pass install a
    /// stranger as somebody's referent. Under a compacting collector that is
    /// not exotic: survivors slide DOWN into the space dead objects vacated,
    /// so a dead referent's base is very often a live object's new base, and
    /// ZGC's `is_object_address` answers "yes, an object lives here" for it.
    /// Measured: `SoftReference.get()` returning a `java.io.ClassCache$CacheRef`
    /// where `MethodTypeForm.cachedLambdaForm` expects a `LambdaForm`.
    ///
    /// A class id is a header READ — unlike an identity hash it mints
    /// nothing, which matters because the only place with the referent in hand
    /// is the pre-GC null pass, and minting into a mark word the collector is
    /// about to use is not a trade worth making.
    ///
    /// `0` and a missing key both mean UNSTAMPED, and unstamped admits — same
    /// convention as the identity stamps, and for the same reason.
    referent_class_stamps: FxHashMap<usize, u32>,

    soft_pre_nulled: FxHashSet<usize>,

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
            soft_ref_addr_index: cratonvm_types::PointerMap::default(),
            last_observed_clock_ms: 0,
            identity_stamps: FxHashMap::default(),
            referent_class_stamps: FxHashMap::default(),
            soft_pre_nulled: FxHashSet::default(),
            stats: ReferenceProcessingStats::default(),
        }
    }

    // -- Discovery ----------------------------------------------------------

    /// Register a newly-discovered reference during the marking phase.
    /// [`Self::discover_reference`] for a `jdk.internal.ref.Cleaner`.
    ///
    /// Tracked as a PHANTOM (which is what it is — the class extends
    /// `PhantomReference`) so it participates in the pre-GC referent nulling,
    /// but flagged [`ReferenceEntry::runs_cleaner`] so clearing it emits a
    /// cleaner ACTION instead of a queue entry. See that field for why the two
    /// halves cannot be separated.
    pub fn discover_phantom_cleaner(
        &mut self,
        reference_obj: usize,
        referent: usize,
        queue: Option<usize>,
    ) {
        self.discover_reference(ReferenceType::Phantom, reference_obj, referent, queue);
        if let Some(entry) = self.phantom_refs.last_mut() {
            entry.runs_cleaner = true;
        }
        if dm_dbg_enabled() {
            DM_PC_DISCOVERED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub fn discover_reference(
        &mut self,
        ref_type: ReferenceType,
        reference_obj: usize,
        referent: usize,
        queue: Option<usize>,
    ) {
        // SOFT-POLICY FIX (2026-07-26): stamp a soft entry with the processor's
        // best knowledge of the mutator clock at *creation* time, mirroring
        // `java.lang.ref.SoftReference`'s constructor (`this.timestamp = clock`).
        //
        // This matters now that [`Self::process_soft_refs`] falls back to
        // `last_observed_clock_ms` for "now": a soft reference discovered
        // through a path that does not also call
        // [`Self::touch_soft_reference`] would otherwise sit at timestamp `0`
        // and look infinitely idle against a wall-clock "now", so the very
        // first collection after the process had been up for
        // `SoftRefLRUPolicyMSPerMB * free_mb` would clear it even though the
        // application had just allocated it.
        //
        // `last_observed_clock_ms` is `0` until the first
        // `touch_soft_reference`, so this is a no-op for every caller that
        // never touches (all current unit and integration tests), and equals
        // "roughly now" for the live VM, where `SoftReference.<init>` touches.
        let creation_stamp = match ref_type {
            ReferenceType::Soft => self.last_observed_clock_ms,
            _ => 0,
        };
        let entry = ReferenceEntry {
            ref_type,
            reference_obj,
            referent,
            queue_addr: queue,
            enqueued: false,
            cleared: false,
            last_access_time_ms: creation_stamp,
            clear_emitted: false,
            action_emitted: false,
            runs_cleaner: false,
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
        // SOFT-POLICY FIX (2026-07-26): record the mutator's clock BEFORE any
        // early return, so the processor learns the real time base even from a
        // touch that resolves no entry or does not advance the timestamp. This
        // is the only clock the processor ever sees that is guaranteed to be on
        // the same scale as `last_access_time_ms`; `process_soft_refs` falls
        // back to it when the collector-side caller passes `0` (see the field
        // doc on `last_observed_clock_ms`). Monotone by construction so a
        // non-monotonic `SystemTime` (NTP step-back) cannot rewind the policy.
        if now_ms > self.last_observed_clock_ms {
            self.last_observed_clock_ms = now_ms;
        }
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

        // Augmented liveness predicate used for Phase 1 (soft): an object is
        // "live" if the collector already marked it OR it is reachable from a
        // to-be-finalized object. Phases 2-4 build on this with the additional
        // soft-reachable closure — see each phase's construction below.
        let soft_is_live =
            |addr: usize| -> bool { is_marked(addr) || finalizer_live.contains(&addr) };

        // SOFT-POLICY FIX (2026-07-26): "now" for the LRU comparison is the
        // later of the caller's clock and the mutator clock the processor has
        // observed through `touch_soft_reference`. Both production call sites
        // in `vm/src/runtime/interpreter.rs` pass a literal `0`, which made the
        // whole soft-ref policy unreachable; a caller with a real clock (ZGC's
        // backend, every test) already dominates and is unaffected. See the
        // `last_observed_clock_ms` field doc for the full analysis.
        let effective_now_ms = current_time_ms.max(self.last_observed_clock_ms);

        // Phase 1 (soft) honours the finalizer-reachable closure.
        self.process_soft_refs(&soft_is_live, free_heap_mb, effective_now_ms);

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

        // SPEC FIX (JLS/`java.lang.ref` reachability ordering, phases 3-4),
        // 2026-07-26. Phases 3 and 4 previously used the RAW `is_marked`
        // snapshot, with a comment claiming phantom reachability should be
        // "unaffected by the resurrection closure". That is the wrong way
        // round: the *whole reason* phantom is processed last is that an object
        // is phantom-reachable only once it is neither strongly, softly, nor
        // weakly reachable AND it has already been finalized. Using raw marking
        // meant:
        //
        //   * a `Cleaner`/`PhantomReference` fired for an object that Phase 1
        //     had just decided to RETAIN through a surviving SoftReference.
        //     For the JDK's most important cleaner — `DirectByteBuffer`'s —
        //     that means freeing the native allocation backing a buffer the
        //     application can still reach via `SoftReference.get()`:
        //     use-after-free, not merely a spec nit; and
        //   * `finalize()` was scheduled for an object that was still softly
        //     reachable, and a phantom was enqueued for an object whose
        //     `finalize()` had not run yet (so a resurrecting finalizer would
        //     race an already-delivered phantom notification).
        //
        // Both phases now fold in the closures Phases 1-2 already computed.
        // The split is deliberate:
        //
        //   * FINALIZER entries fold in `soft_live` ONLY. Soft reachability
        //     blocks finalization, but finalizer-reachability must NOT: when
        //     two mutually-reachable objects both override `finalize()`,
        //     HotSpot enqueues both in the same cycle. Folding `finalizer_live`
        //     here would make each block the other forever.
        //   * CLEANER and PHANTOM entries fold in BOTH. `Cleaner` is a
        //     `PhantomReference` subclass, so it takes the phantom rule, and
        //     "has been finalized" is part of that rule.
        //
        // This is strictly *less* clearing/enqueueing than before, so it can
        // only over-retain, never free early. Over-retention is bounded at one
        // collection for the finalizer closure: once Phase 3 flags an entry
        // `enqueued`, it stops contributing a root, and the next cycle fires
        // the phantom.
        let final_is_live = |addr: usize| -> bool { is_marked(addr) || soft_live.contains(&addr) };
        let phantom_is_live = |addr: usize| -> bool {
            is_marked(addr) || soft_live.contains(&addr) || finalizer_live.contains(&addr)
        };
        self.process_final_refs(&final_is_live, &phantom_is_live);
        self.process_phantom_refs(&phantom_is_live);

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
        // exclusion run; gaps/h2-testscript-segv-findings.md).
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
        // `jdk.internal.ref.Cleaner`s live in `phantom_refs` (see
        // `ReferenceEntry::runs_cleaner`) and become actions once their referent
        // is gone — `enqueued` is what `process_phantom_refs` sets for a phantom
        // whose referent died, and it is set exactly once.
        for entry in &mut self.phantom_refs {
            if entry.runs_cleaner && entry.enqueued && !entry.action_emitted {
                entry.action_emitted = true;
                cleaner_actions.push(entry.reference_obj);
            }
        }
        if dm_dbg_enabled() {
            use std::sync::atomic::Ordering::Relaxed;
            // A `runs_cleaner` phantom that is NOT yet enqueued is one whose
            // referent is STILL REACHABLE -- i.e. a direct buffer something in
            // Java is still holding. That count answers lead 3 directly.
            let retained = self
                .phantom_refs
                .iter()
                .filter(|e| e.runs_cleaner && !e.enqueued)
                .count();
            let emitted = cleaner_actions.len();
            let cum = DM_PC_EMITTED.fetch_add(emitted as u64, Relaxed) + emitted as u64;
            let g = DM_REFPROC_ROUNDS.fetch_add(1, Relaxed) + 1;
            if emitted > 0 || g % 50 == 0 {
                eprintln!(
                    "[dm] refproc round={} emitted={} cum_emitted={} discovered={} retained_live={} phantom_total={}",
                    g,
                    emitted,
                    cum,
                    DM_PC_DISCOVERED.load(Relaxed),
                    retained,
                    self.phantom_refs.len()
                );
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

        // ---- Entries condemned BEFORE the mark ---------------------------
        //
        // `condemn_idle_soft_refs` already applied the LRU rule to these, on
        // the pre-collection heap headroom, and the VM nulled their referent
        // slot so the marker could not keep the referent alive through the
        // `SoftReference` itself. They are settled here rather than by the
        // range scan below for two independent reasons:
        //
        // * re-deriving the verdict now would use POST-collection headroom,
        //   which is larger, which makes `threshold_ms` larger — so the
        //   re-derivation can only ever disagree in the direction of "keep".
        //   Keeping an entry whose referent this collection has already
        //   reclaimed leaves a dead address inside an entry that still reads
        //   as active;
        // * `candidate_indices` below is derived from that same larger
        //   threshold, so a condemned entry can fall outside the range and
        //   never be visited at all.
        //
        // `is_marked` still has the last word. A condemned referent that was
        // reachable on a strong path was marked anyway and is kept; the
        // post-GC restore pass writes its slot back
        // (`soft_pre_nulled_active_pairs`).
        if !self.soft_pre_nulled.is_empty() {
            // (index, reference_obj, queue_addr, last_access_time_ms) — read
            // out first so the mutation loop is not holding a borrow of
            // `self.soft_refs` while it touches `pending_queues` / the LRU
            // index, which are disjoint fields the borrow checker cannot see
            // through `self`.
            let mut condemned: Vec<(usize, usize, Option<usize>, u64)> = Vec::new();
            for (idx, entry) in self.soft_refs.iter().enumerate() {
                if entry.cleared
                    || entry.enqueued
                    || !self.soft_pre_nulled.contains(&entry.reference_obj)
                    || is_marked(entry.referent)
                {
                    continue;
                }
                condemned.push((
                    idx,
                    entry.reference_obj,
                    entry.queue_addr,
                    entry.last_access_time_ms,
                ));
            }
            for (idx, reference_obj, queue_addr, last_access) in condemned {
                self.soft_refs[idx].cleared = true;
                self.stats.soft_refs_cleared += 1;
                if let Some(q) = queue_addr {
                    self.pending_queues
                        .entry(q)
                        .or_default()
                        .push(reference_obj);
                    self.soft_refs[idx].enqueued = true;
                }
                // Same LRU-index hygiene the range scan below applies when it
                // clears an entry: a cleared soft ref is never a candidate
                // again, so its key would make every later range scan re-walk
                // it.
                self.soft_ref_lru_index.remove(&(last_access, idx));
            }
        }

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

    /// `finalizer_is_live` gates `finalize()` scheduling (raw marking + the
    /// soft-reachable closure); `cleaner_is_live` gates `Cleaner` actions
    /// (raw marking + soft-reachable + finalizer-reachable, because `Cleaner`
    /// is a `PhantomReference`). See the call site for why the two differ.
    fn process_final_refs(
        &mut self,
        finalizer_is_live: &dyn Fn(usize) -> bool,
        cleaner_is_live: &dyn Fn(usize) -> bool,
    ) {
        // Cleaners
        for entry in &mut self.cleaner_refs {
            if entry.cleared {
                continue;
            }
            if cleaner_is_live(entry.referent) {
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
            if finalizer_is_live(entry.referent) {
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
            // A `jdk.internal.ref.Cleaner` is never enqueued — its queue is a
            // private `dummyQueue` with no consumer. It is RUN, in
            // `process_references`'s gather step. See
            // `ReferenceEntry::runs_cleaner`.
            if entry.runs_cleaner {
                continue;
            }
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
    pub fn update_after_gc(&mut self, pointer_map: &cratonvm_types::PointerMap) {
        fn relocate_list(list: &mut [ReferenceEntry], map: &cratonvm_types::PointerMap) {
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

        // The stamp table is address-keyed too, and a compacting collector
        // moves the keys. Rebuilt rather than relocated in place: two entries
        // can swap addresses across one slide (a survivor slides onto the base
        // a dead object vacated), and an in-place rewrite of a map whose keys
        // are also its targets picks whichever insert lands second.
        if !self.identity_stamps.is_empty() {
            let mut moved: FxHashMap<usize, i32> =
                FxHashMap::with_capacity_and_hasher(self.identity_stamps.len(), Default::default());
            for (addr, hash) in self.identity_stamps.iter() {
                let now = match pointer_map.get(addr) {
                    Some(&new) if new != 0 => new,
                    _ => *addr,
                };
                moved.insert(now, *hash);
            }
            self.identity_stamps = moved;
        }

        // Same treatment, same reason, for the referent-class table: its keys
        // are `reference_obj` addresses and a slide moves them.
        if !self.referent_class_stamps.is_empty() {
            let mut moved: FxHashMap<usize, u32> = FxHashMap::with_capacity_and_hasher(
                self.referent_class_stamps.len(),
                Default::default(),
            );
            for (addr, cid) in self.referent_class_stamps.iter() {
                let now = match pointer_map.get(addr) {
                    Some(&new) if new != 0 => new,
                    _ => *addr,
                };
                moved.insert(now, *cid);
            }
            self.referent_class_stamps = moved;
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

    /// Drop weak/phantom entries whose `Reference` object no longer *looks
    /// like* a `Reference` — i.e. its memory has been reclaimed and recycled.
    ///
    /// HIB-WEAKREF-RECYCLE.1 (2026-07-31). [`Self::remove_collected`]'s
    /// survivor predicate ultimately bottoms out in `is_addr_live`, which for
    /// an old-generation address is a pure range check: freed old-gen memory
    /// still answers "live". A weak/phantom entry whose `Reference` object was
    /// reclaimed by an old-gen sweep therefore survives every prune, and both
    /// the pre-GC null pass and the post-GC restore pass keep writing slot 0 of
    /// whatever now occupies that address, forever. Declining the write at each
    /// of those sites stops the corruption but leaves the dangling entry (and
    /// its per-cycle cost) in place; this prunes it.
    ///
    /// Deliberately scoped to `weak_refs`/`phantom_refs`: their `reference_obj`
    /// is always a `java.lang.ref.Reference` subclass, so "has >= 2 instance
    /// fields" is a sound shape test. `finalizer_refs`/`cleaner_refs` are NOT
    /// safe to test this way — a finalizable object is an arbitrary class and
    /// may legitimately declare fewer than two fields.
    pub fn retain_shaped_weak_phantom(&mut self, still_a_reference: &dyn Fn(usize) -> bool) {
        self.weak_refs
            .retain(|e| still_a_reference(e.reference_obj));
        self.phantom_refs
            .retain(|e| still_a_reference(e.reference_obj));
    }

    /// Remove entries whose `Reference` object has itself been collected.
    ///
    /// `soft_refs` is indexed by position into [`Self::soft_ref_lru_index`]
    /// (keyed `(last_access_time_ms, idx) -> idx`); after `retain` shrinks
    /// the Vec the old indices are stale and would index out of bounds in
    /// `process_soft_refs`. We rebuild the LRU index from scratch by
    /// re-walking the surviving entries so the `(timestamp, idx)` keys and
    /// stored values reflect the new positions.
    /// Record the identity hash `reference_obj` carries, so every later write
    /// through its address can prove the object is still the one discovered.
    /// See [`Self::identity_stamps`].
    ///
    /// Called by the VM immediately after a `discover_*`, because minting the
    /// hash needs the heap and this crate has only addresses.
    pub fn stamp_reference(&mut self, reference_obj: usize, identity_hash: i32) {
        if identity_hash != 0 {
            self.identity_stamps.insert(reference_obj, identity_hash);
        }
    }

    /// Record the CLASS of the referent this entry currently points at, so the
    /// post-GC restore pass can refuse to install a stranger. See
    /// [`Self::referent_class_stamps`].
    ///
    /// Called by the VM from the pre-GC null pass, which is the one place that
    /// holds the referent and the heap at the same time.
    pub fn stamp_referent_class(&mut self, reference_obj: usize, class_id: u32) {
        if class_id != 0 {
            self.referent_class_stamps.insert(reference_obj, class_id);
        }
    }

    /// A copy of the referent-class table, for the same reason
    /// [`Self::identity_stamps_snapshot`] exists.
    pub fn referent_class_stamps_snapshot(&self) -> FxHashMap<usize, u32> {
        self.referent_class_stamps.clone()
    }

    /// The stamp for `reference_obj`, or `None` when it was never stamped.
    pub fn identity_stamp(&self, reference_obj: usize) -> Option<i32> {
        self.identity_stamps.get(&reference_obj).copied()
    }

    /// A copy of the whole stamp table, for a consumer that must check
    /// identities while it also mutates this processor (the post-GC passes
    /// take `&mut self` to drain the cleared list, so they cannot hold a
    /// borrow of it). One clone per collection, the same order as the passes
    /// themselves.
    pub fn identity_stamps_snapshot(&self) -> FxHashMap<usize, i32> {
        self.identity_stamps.clone()
    }

    /// [`Self::weak_phantom_active_pairs`] plus each entry's identity stamp
    /// (`0` = unstamped), so the pre-GC referent-null pass can check identity
    /// after it has dropped this processor's lock.
    pub fn weak_phantom_active_triples(&self) -> Vec<(usize, usize, i32)> {
        self.weak_phantom_active_pairs()
            .into_iter()
            .map(|(r, t)| (r, t, self.identity_stamp(r).unwrap_or(0)))
            .collect()
    }

    /// [`Self::soft_pre_nulled_active_pairs`] with the same stamp column.
    pub fn soft_pre_nulled_active_triples(&self) -> Vec<(usize, usize, i32)> {
        self.soft_pre_nulled_active_pairs()
            .into_iter()
            .map(|(r, t)| (r, t, self.identity_stamp(r).unwrap_or(0)))
            .collect()
    }

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
        // Drop stamps for entries that just left. A leaked stamp is not
        // dangerous (a later `discover_*` at the same address overwrites it,
        // which is the ABA case) but it is unbounded, and this walk is already
        // O(entries).
        self.prune_identity_stamps();
    }

    /// Keep only the stamps of entries this processor still holds.
    fn prune_identity_stamps(&mut self) {
        if self.identity_stamps.is_empty() && self.referent_class_stamps.is_empty() {
            return;
        }
        let mut live: FxHashSet<usize> = FxHashSet::default();
        for e in self
            .soft_refs
            .iter()
            .chain(self.weak_refs.iter())
            .chain(self.phantom_refs.iter())
            .chain(self.cleaner_refs.iter())
            .chain(self.finalizer_refs.iter())
        {
            live.insert(e.reference_obj);
        }
        self.identity_stamps.retain(|addr, _| live.contains(addr));
        self.referent_class_stamps
            .retain(|addr, _| live.contains(addr));
    }

    /// Retire the registry's bookkeeping entry for a `Reference` the
    /// application just enqueued *itself*, via an explicit `Reference.enqueue()`
    /// call (as opposed to the GC discovering the referent dead).
    ///
    /// PGJDBC-PHANTOM-GHOST (2026-08-07). `Reference.enqueue()` is a public,
    /// unconditional JDK API: it queues the `Reference` regardless of whether
    /// its referent is still reachable. Real code uses this — e.g. pgjdbc's
    /// `SimpleQuery.setCleanupRef`/`unprepare()` retire the *previous*
    /// `PhantomReference` wrapping a still-alive, about-to-be-reprepared
    /// `SimpleQuery` by calling `oldRef.clear(); oldRef.enqueue();` before
    /// installing a *new* `PhantomReference` around the SAME referent. Both
    /// the old and new `PhantomReference` objects are registered with this
    /// processor at construction time (`discover_reference`), and both share
    /// the same `referent` address.
    ///
    /// The native `Reference.enqueue()` path (`native_ref_enqueue` in
    /// `native-builtins/src/reference.rs`) physically links the Reference onto
    /// its `ReferenceQueue`'s Java-visible linked list directly (or, for a
    /// real-JDK-layout Reference, by invoking the JDK's own
    /// `ReferenceQueue.enqueue()` bytecode) — but until this fix, it never told
    /// *this* registry that the reference had been handled. The stale entry
    /// (`enqueued: false`) sat in `phantom_refs` (or `weak_refs`) until its own
    /// `Reference` object was later collected. If the shared referent (the
    /// `SimpleQuery`) outlived that window — entirely realistic for a
    /// long-lived, repeatedly-reprepared statement — the GC's own
    /// `process_phantom_refs` would eventually discover the referent dead and
    /// enqueue EVERY still-registered entry pointing at it, including the
    /// STALE one the application had already drained, removed from its own
    /// bookkeeping (e.g. `parsedQueryMap.remove(ref)`), and forgotten about. A
    /// second, unsolicited queue delivery of an already-fully-processed
    /// `Reference` is a ghost: `ReferenceQueue.poll()` legitimately returns
    /// non-null, but nothing recognises it any more — pgjdbc's
    /// `QueryExecutorImpl.processDeadParsedQueries()` observed exactly this as
    /// `parsedQueryMap.remove(ref)` returning `null`, feeding a `null`
    /// statement name into `sendCloseStatement` and NPEing on
    /// `statementName.getBytes(...)`.
    ///
    /// Marking the entry `cleared = true` and `enqueued = true` here (without
    /// touching `pending_queues` — the application already performed the real
    /// enqueue itself) makes every processing phase's re-entry guard
    /// (`process_weak_refs` gates on `cleared`; `process_phantom_refs` gates on
    /// `enqueued`) skip it on every subsequent cycle, so it is never
    /// rediscovered and delivered a second time. Returns whether a matching
    /// entry was found (informational only; a caller with no matching entry —
    /// e.g. `Reference.enqueue()` on a `Reference` this processor never saw
    /// `discover_reference`d, such as one constructed with a null referent —
    /// has nothing to retire and that is not an error).
    pub fn mark_manually_enqueued(&mut self, reference_obj: usize) -> bool {
        for list in [
            &mut self.weak_refs,
            &mut self.soft_refs,
            &mut self.phantom_refs,
            &mut self.cleaner_refs,
        ] {
            if let Some(entry) = list.iter_mut().find(|e| e.reference_obj == reference_obj) {
                entry.cleared = true;
                entry.enqueued = true;
                return true;
            }
        }
        false
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
    /// Apply the SoftReference LRU policy *before* the collection and report
    /// the entries it condemns, so the caller can null their referent slot
    /// ahead of the mark.
    ///
    /// # Why the decision has to happen here and not after the mark
    ///
    /// [`Self::process_soft_refs`] refuses to clear an entry whose referent
    /// `is_marked`, and CratonVM's markers trace a `Reference`'s slot 0 as an
    /// ordinary strong edge. A soft referent is therefore *always* marked
    /// through its own `SoftReference`, that check always wins, and the LRU
    /// policy underneath it is unreachable — soft references behaved exactly
    /// like strong ones on every collector. Measured 2026-08-15 with a
    /// four-arm probe in a 64 MiB heap: HotSpot cleared the soft reference and
    /// allocated 30 MiB past it; CratonVM threw `OutOfMemoryError` under both
    /// ZGC and G1 with the referent still reachable only softly.
    ///
    /// `weakref_null_referents_pre_gc` already solves precisely this problem
    /// for Weak and Phantom by nulling slot 0 before the mark, which is why
    /// those two work. Soft differs in exactly one respect: a weak referent
    /// dies whenever nothing else holds it, while a soft referent dies only
    /// when the LRU policy judges the heap tight enough. So the policy runs
    /// first and only its condemned set is nulled; an entry the policy wants
    /// to keep is left traced strongly and is retained bit-for-bit as before.
    ///
    /// `free_heap_mb` and `current_time_ms` carry the same meaning as in
    /// [`Self::process_references`], including the `0`-clock convention: the
    /// processor substitutes the mutator clock it has observed through
    /// [`Self::touch_soft_reference`] when the caller has none.
    ///
    /// Returns `(reference_obj, referent)` per condemned entry. Calling this
    /// also RESETS the condemned set, so it must be called once per
    /// collection, before the mark.
    ///
    /// # The clock argument is load-bearing here, unlike post-GC
    ///
    /// `current_time_ms == 0` makes "now" the last value a mutator handed
    /// [`Self::touch_soft_reference`] — i.e. the moment of the most recent
    /// `SoftReference.get()` anywhere in the process. For the reference that
    /// *made* that call the idle window is then exactly zero, forever, and a
    /// program in a tight allocation loop reading its own cache is precisely
    /// the program that keeps re-stamping it. The caller here is ordinary VM
    /// code on the mutator side of a safepoint, so it can and does supply a
    /// real `SystemTime` reading; `0` is accepted only so tests can pin the
    /// clock.
    pub fn condemn_idle_soft_refs(
        &mut self,
        free_heap_mb: usize,
        current_time_ms: u64,
    ) -> Vec<(usize, usize)> {
        let now_ms = current_time_ms.max(self.last_observed_clock_ms);
        let threshold_ms = self
            .soft_ref_lru_policy_ms_per_mb
            .saturating_mul(free_heap_mb as u64);
        self.condemn(|e| now_ms.saturating_sub(e.last_access_time_ms) > threshold_ms)
    }

    /// Condemn EVERY active soft reference, ignoring the LRU policy: the
    /// last-ditch rule the `java.lang.ref` specification states outright —
    /// "all soft references to softly-reachable objects are guaranteed to have
    /// been cleared before the virtual machine throws an
    /// `OutOfMemoryError`". HotSpot implements it as
    /// `SoftRefPolicy::should_clear_all_soft_refs`, armed for the full GC it
    /// runs when an allocation has already failed.
    ///
    /// This is a genuinely different rule from [`Self::condemn_idle_soft_refs`]
    /// and not a limiting case of it: the LRU policy asks how long ago the
    /// application last read the reference, and a program looping on its own
    /// soft-referenced cache re-stamps that clock on every iteration, so its
    /// idle window never opens however tight the heap gets. Measured: a
    /// 64 MiB heap where HotSpot cleared the reference and completed, and
    /// CratonVM threw `OutOfMemoryError` with a megabyte of softly-reachable
    /// garbage in hand.
    ///
    /// `is_marked` still decides the outcome, exactly as for the idle set: a
    /// condemned referent that is also strongly reachable was marked anyway
    /// and is kept, then restored. "Clear all soft references" means all the
    /// ones nothing else holds.
    pub fn condemn_all_soft_refs(&mut self) -> Vec<(usize, usize)> {
        self.condemn(|_| true)
    }

    /// Whether any soft entry is still live enough to be worth a last-ditch
    /// collection — so the escalation ladder can skip one full GC when the
    /// application uses no soft references at all.
    pub fn has_active_soft_refs(&self) -> bool {
        self.soft_refs.iter().any(|e| !e.cleared && !e.enqueued)
    }

    /// Shared body of the two condemnation rules: select from the active soft
    /// entries, publish the selection as this cycle's condemned set (replacing
    /// any previous cycle's), and hand the pairs back for the caller to null.
    fn condemn(&mut self, pick: impl Fn(&ReferenceEntry) -> bool) -> Vec<(usize, usize)> {
        let condemned: Vec<(usize, usize)> = self
            .soft_refs
            .iter()
            .filter(|e| !e.cleared && !e.enqueued)
            .filter(|e| pick(e))
            .map(|e| (e.reference_obj, e.referent))
            .collect();
        self.soft_pre_nulled.clear();
        self.soft_pre_nulled
            .extend(condemned.iter().map(|&(ref_obj, _)| ref_obj));
        condemned
    }

    /// The `(reference_obj, referent)` pairs [`Self::condemn_idle_soft_refs`]
    /// condemned this cycle whose entry is STILL active — the referent turned
    /// out to be strongly reachable, so `process_soft_refs` kept it and the
    /// slot 0 the pre-collection pass nulled has to be written back.
    ///
    /// The soft twin of [`Self::weak_phantom_active_pairs`], consumed by the
    /// same post-GC restore loop.
    pub fn soft_pre_nulled_active_pairs(&self) -> Vec<(usize, usize)> {
        self.soft_refs
            .iter()
            .filter(|e| {
                !e.cleared && !e.enqueued && self.soft_pre_nulled.contains(&e.reference_obj)
            })
            .map(|e| (e.reference_obj, e.referent))
            .collect()
    }

    pub fn weak_phantom_active_pairs(&self) -> Vec<(usize, usize)> {
        let mut v = Vec::with_capacity(self.weak_refs.len() + self.phantom_refs.len());
        for e in self.weak_refs.iter().chain(self.phantom_refs.iter()) {
            if !e.cleared && !e.enqueued {
                v.push((e.reference_obj, e.referent));
            }
        }
        v
    }

    /// Every heap address this processor holds a raw pointer to, across ALL
    /// reference kinds (soft / weak / phantom / cleaner / finalizer): the
    /// `Reference` object itself, its referent, and its `ReferenceQueue`.
    ///
    /// Published to `gc_quiescence::set_watched_referents` before each
    /// collection. Both young collectors and the old-gen collector emit an
    /// identity `pointer_map` entry for every watched address that SURVIVES
    /// but does not move, which is what makes "absent from the pointer map"
    /// an exact death proof for precisely the addresses post-GC reference
    /// processing writes through — see
    /// `VmHeap::watched_pre_gc_addr_survived`.
    ///
    /// Superset of [`Self::weak_phantom_active_pairs`] +
    /// [`Self::weak_phantom_active_queue_addrs`], which covered only the
    /// weak/phantom halves and only their non-cleared entries: the survival
    /// predicate is consulted for soft/cleaner/finalizer entries too (via
    /// `process_references` and `remove_collected`), so every one of them
    /// needs the same proof.
    pub fn all_tracked_addrs(&self) -> Vec<usize> {
        let n = self.soft_refs.len()
            + self.weak_refs.len()
            + self.phantom_refs.len()
            + self.cleaner_refs.len()
            + self.finalizer_refs.len();
        let mut v = Vec::with_capacity(n * 3);
        for e in self
            .soft_refs
            .iter()
            .chain(self.weak_refs.iter())
            .chain(self.phantom_refs.iter())
            .chain(self.cleaner_refs.iter())
            .chain(self.finalizer_refs.iter())
        {
            v.push(e.reference_obj);
            v.push(e.referent);
            if let Some(q) = e.queue_addr {
                v.push(q);
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
    pub fn update_after_gc(&self, pointer_map: &cratonvm_types::PointerMap) {
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
    pub fn update_after_gc(&self, pointer_map: &cratonvm_types::PointerMap) {
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

    /// Addresses of every object still waiting for its `finalize()` to run.
    ///
    /// These MUST be handed to the collector as finalizable roots. Once
    /// `ReferenceProcessor::mark_finalizer_enqueued` flags an entry,
    /// `finalizer_referent_addresses` stops reporting it (that flag exists to
    /// stop the resurrection channel re-enqueuing the same object on every
    /// later cycle), so from that moment the only thing still referring to the
    /// object is this queue — and it holds a *raw address*, which no marker
    /// can see. `update_after_gc` already covers a moving collection, but a
    /// non-moving young mark-sweep just frees the now-unreachable object and
    /// leaves the queued address dangling; `run_finalizers` then dereferences
    /// it (`heap.class_id_of`) and faults. Keeping them alive is what JLS
    /// §12.6 requires anyway: `finalize()` runs *on* the object and may even
    /// resurrect it.
    pub fn pending_addresses(&self) -> Vec<usize> {
        self.finalization_queue.lock().iter().copied().collect()
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
        let mut map = cratonvm_types::PointerMap::default();
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
    fn all_tracked_addrs_covers_every_reference_kind() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Soft, 100, 200, None);
        proc.discover_reference(ReferenceType::Weak, 101, 201, Some(301));
        proc.discover_reference(ReferenceType::Phantom, 102, 202, None);
        proc.discover_reference(ReferenceType::Cleaner, 103, 203, None);
        proc.discover_reference(ReferenceType::Finalizer, 104, 204, None);

        let addrs = proc.all_tracked_addrs();
        // Soft / cleaner / finalizer entries were NOT published before the
        // old-gen-reclamation fix, so the collector emitted no survival proof
        // for them and the exact post-GC predicate could not be used.
        for a in [100, 200, 101, 201, 301, 102, 202, 103, 203, 104, 204] {
            assert!(
                addrs.contains(&a),
                "address {a} must be published as watched"
            );
        }
    }

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

    // 19b. HIB-WEAKREF-RECYCLE.1 — the shape prune drops weak/phantom entries
    //      whose Reference object has been recycled, and leaves the
    //      finalizer/cleaner lists (whose reference_obj is an arbitrary object)
    //      completely alone.
    #[test]
    fn retain_shaped_weak_phantom_prunes_recycled_and_spares_finalizers() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 100, 200, None);
        proc.discover_reference(ReferenceType::Weak, 101, 201, None);
        proc.discover_reference(ReferenceType::Phantom, 102, 202, Some(302));
        // A finalizable object with a shape the weak/phantom test would reject.
        proc.discover_reference(ReferenceType::Finalizer, 103, 203, None);
        proc.discover_reference(ReferenceType::Cleaner, 104, 204, None);

        // 100 and 102 have been reclaimed and their memory recycled: whatever
        // occupies those addresses no longer has >= 2 instance fields.
        let still_a_reference = [101usize];
        proc.retain_shaped_weak_phantom(&live_set(&still_a_reference));

        assert_eq!(proc.weak_refs.len(), 1);
        assert_eq!(proc.weak_refs[0].reference_obj, 101);
        assert!(proc.phantom_refs.is_empty());
        // Untouched — pruning these on a field-count test would drop live
        // finalizable objects that legitimately declare fewer than two fields.
        assert_eq!(proc.finalizer_refs.len(), 1);
        assert_eq!(proc.cleaner_refs.len(), 1);
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

    // 28b. SOFT-CLEAR GAP (2026-08-15) -------------------------------------
    //
    // These five pin the fix for the defect the retired
    // `zgc-resourceleakdetector-corpse-read` write-up left as an open
    // question. `process_soft_refs` skips any entry whose referent
    // `is_marked`, and the VM's markers trace a `Reference`'s slot 0 as an
    // ordinary strong edge -- so a soft referent was always marked through its
    // own `SoftReference` and the LRU policy underneath was unreachable on
    // every collector. `condemn_idle_soft_refs` moves the decision ahead of
    // the mark, where the VM can null the slot the way it already does for
    // weak and phantom.

    /// The policy picks out the idle entries and leaves the freshly-read one.
    #[test]
    fn condemn_idle_soft_refs_selects_only_the_idle_entries() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 10, 100, Some(900));
        proc.discover_reference(ReferenceType::Soft, 20, 200, None);
        proc.touch_soft_reference(10, 1_000);
        proc.touch_soft_reference(20, 61_000);
        // 1 MB allocatable => a 1000 ms idle threshold; "now" is the mutator
        // clock the processor observed (61_000), so entry 10 is 60 s idle and
        // entry 20 is 0 s idle.
        assert_eq!(proc.condemn_idle_soft_refs(1, 0), vec![(10, 100)]);
    }

    /// A roomy heap condemns nothing -- the LRU threshold scales with free
    /// megabytes, so the same 60 s idle window is nowhere near it.
    #[test]
    fn condemn_idle_soft_refs_condemns_nothing_when_the_heap_is_roomy() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 10, 100, None);
        proc.discover_reference(ReferenceType::Soft, 20, 200, None);
        proc.touch_soft_reference(10, 1_000);
        proc.touch_soft_reference(20, 61_000);
        assert!(proc.condemn_idle_soft_refs(1024, 0).is_empty());
        assert!(proc.soft_pre_nulled_active_pairs().is_empty());
    }

    /// The load-bearing one. The pre-mark verdict has to STAND even though
    /// re-deriving it after the collection would say "keep": the collection
    /// freed memory, so the post-GC threshold is larger, and the BTreeMap
    /// range the ordinary scan walks is derived from that same larger
    /// threshold -- here it selects no candidates at all. Without the
    /// pre-condemned pass the referent is gone (its slot was nulled before the
    /// mark) while the entry still reads as active, holding a dead address.
    #[test]
    fn a_pre_condemned_soft_ref_is_cleared_even_when_the_post_gc_threshold_would_keep_it() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 10, 100, Some(900));
        proc.discover_reference(ReferenceType::Soft, 20, 200, None);
        proc.touch_soft_reference(10, 1_000);
        proc.touch_soft_reference(20, 61_000);
        assert_eq!(proc.condemn_idle_soft_refs(1, 0), vec![(10, 100)]);

        // 10_000 MB free after the collection => a 10_000_000 ms threshold, so
        // the range scan's cutoff is 0 and it visits nothing.
        let result = proc.process_references(&always_dead, 10_000, 0);
        assert!(
            proc.soft_refs[0].cleared,
            "the pre-mark verdict must stand: this referent is already gone"
        );
        assert!(proc.soft_refs[0].enqueued);
        assert!(result.to_enqueue.contains(&(10, 900)));
        // The entry the policy KEPT is untouched, dead referent or not --
        // nothing nulled its slot, so the marker kept its referent alive and
        // `always_dead` is a fiction for it.
        assert!(!proc.soft_refs[1].cleared);
    }

    /// A condemned entry whose referent turned out to be strongly reachable is
    /// kept, and is reported for the post-GC slot-0 restore.
    #[test]
    fn a_condemned_soft_ref_whose_referent_survived_is_offered_for_restore() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 10, 100, None);
        proc.discover_reference(ReferenceType::Soft, 20, 200, None);
        proc.touch_soft_reference(10, 1_000);
        proc.touch_soft_reference(20, 61_000);
        assert_eq!(proc.condemn_idle_soft_refs(1, 0), vec![(10, 100)]);

        let live = [100usize];
        proc.process_references(&live_set(&live), 1, 0);
        assert!(!proc.soft_refs[0].cleared);
        assert_eq!(proc.soft_pre_nulled_active_pairs(), vec![(10, 100)]);
    }

    /// The condemned set describes ONE collection. A later cycle that condemns
    /// nothing must not inherit the previous cycle's verdict -- otherwise an
    /// entry the policy has since decided to keep would be cleared the moment
    /// its referent looked unmarked for any other reason.
    #[test]
    fn condemn_idle_soft_refs_resets_the_condemned_set_each_cycle() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 10, 100, None);
        proc.discover_reference(ReferenceType::Soft, 20, 200, None);
        proc.touch_soft_reference(10, 1_000);
        proc.touch_soft_reference(20, 61_000);
        assert_eq!(proc.condemn_idle_soft_refs(1, 0), vec![(10, 100)]);
        // Second cycle, roomy heap: nothing is condemned, so nothing carries
        // over.
        assert!(proc.condemn_idle_soft_refs(1024, 0).is_empty());
        assert!(proc.soft_pre_nulled_active_pairs().is_empty());
        proc.process_references(&always_dead, 1024, 0);
        assert!(
            !proc.soft_refs[0].cleared,
            "no slot was nulled this cycle, so no entry may be force-cleared"
        );
    }

    /// The last-ditch rule clears a reference the LRU policy would keep, which
    /// is the whole point of having it: a program looping on its own
    /// soft-referenced cache re-stamps the LRU clock every iteration, so its
    /// idle window never opens however tight the heap becomes.
    #[test]
    fn the_last_ditch_rule_condemns_a_soft_ref_the_lru_policy_would_keep() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 10, 100, Some(900));
        // Read just now: zero idle time, so the LRU policy keeps it even with
        // the heap reporting no free megabytes at all.
        proc.touch_soft_reference(10, 61_000);
        assert!(proc.condemn_idle_soft_refs(0, 61_000).is_empty());

        assert_eq!(proc.condemn_all_soft_refs(), vec![(10, 100)]);
        let result = proc.process_references(&always_dead, 0, 61_000);
        assert!(proc.soft_refs[0].cleared);
        assert!(result.to_enqueue.contains(&(10, 900)));
    }

    /// "Clear all soft references" means all the ones nothing else holds. A
    /// condemned referent that is still strongly reachable was marked anyway
    /// and must survive — otherwise the last-ditch collection would hand the
    /// application a null for an object it can still reach by a strong path.
    #[test]
    fn the_last_ditch_rule_still_keeps_a_strongly_reachable_referent() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 10, 100, None);
        proc.discover_reference(ReferenceType::Soft, 20, 200, None);
        proc.touch_soft_reference(10, 61_000);
        proc.touch_soft_reference(20, 61_000);
        assert_eq!(proc.condemn_all_soft_refs(), vec![(10, 100), (20, 200)]);
        let live = [100usize];
        proc.process_references(&live_set(&live), 0, 61_000);
        assert!(
            !proc.soft_refs[0].cleared,
            "referent 100 is strongly reachable"
        );
        assert!(
            proc.soft_refs[1].cleared,
            "referent 200 is only softly reachable"
        );
        assert_eq!(proc.soft_pre_nulled_active_pairs(), vec![(10, 100)]);
    }

    /// The ladder skips its extra collection when there is nothing to clear.
    #[test]
    fn has_active_soft_refs_tracks_the_uncleared_population() {
        let mut proc = ReferenceProcessor::new();
        assert!(!proc.has_active_soft_refs());
        proc.discover_reference(ReferenceType::Weak, 1, 2, None);
        assert!(!proc.has_active_soft_refs(), "a weak ref is not a soft ref");
        proc.discover_reference(ReferenceType::Soft, 10, 100, None);
        assert!(proc.has_active_soft_refs());
        proc.condemn_all_soft_refs();
        proc.process_references(&always_dead, 0, 1);
        assert!(!proc.has_active_soft_refs());
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
        let mut map = cratonvm_types::PointerMap::default();
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
        let mut map = cratonvm_types::PointerMap::default();
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
        let mut map = cratonvm_types::PointerMap::default();
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

        let mut map = cratonvm_types::PointerMap::default();
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
        let mut map = cratonvm_types::PointerMap::default();
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

    // ======================================================================
    // SOFT-POLICY FIX (2026-07-26): the LRU must survive a caller that has no
    // clock. Both production call sites in `vm/src/runtime/interpreter.rs`
    // pass `current_time_ms == 0`, which used to make `process_soft_refs`
    // structurally incapable of clearing anything. See the
    // `last_observed_clock_ms` field doc.
    // ======================================================================

    // 66. With a `0` caller clock, the mutator clock observed through
    //     `touch_soft_reference` is used instead, so an idle soft ref clears.
    #[test]
    fn soft_policy_uses_mutator_clock_when_caller_passes_zero() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, Some(0x180));
        // `SoftReference.get()` stamps a wall-clock time on the entry.
        proc.touch_soft_reference(0x100, 1_000_000);
        // Time passes; another (unrelated / unknown) touch advances only the
        // processor's clock. This also pins that the clock is recorded BEFORE
        // the unknown-address early return.
        proc.touch_soft_reference(0xDEAD, 1_100_000);

        // Exactly what the interpreter passes today.
        let result = proc.process_references(&always_dead, 64, 0);

        // threshold = 1000 ms/MB * 64 MB = 64_000; idle = 100_000 > 64_000.
        assert_eq!(
            result.stats.soft_refs_cleared, 1,
            "soft-ref LRU must run even when the collector supplies no clock"
        );
        assert!(proc.soft_refs[0].cleared);
    }

    // 67. A soft ref created after the clock is known carries a *creation*
    //     timestamp (like `SoftReference`'s constructor), so it is not
    //     instantly stale against a wall-clock "now". Non-soft types keep
    //     stamping 0 — the field is meaningless for them.
    #[test]
    fn soft_policy_creation_stamp_protects_fresh_ref() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        // Establish the mutator clock first (some earlier SoftReference.get()).
        proc.touch_soft_reference(0xDEAD, 5_000_000);

        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
        assert_eq!(
            proc.soft_refs[0].last_access_time_ms, 5_000_000,
            "a freshly discovered soft ref must be stamped at creation"
        );
        proc.discover_reference(ReferenceType::Weak, 0x300, 0x400, None);
        assert_eq!(
            proc.weak_refs[0].last_access_time_ms, 0,
            "only soft refs carry an LRU timestamp"
        );

        // Production (64, 0): idle == 0, so the fresh ref must survive.
        let result = proc.process_references(&always_dead, 64, 0);
        assert_eq!(result.stats.soft_refs_cleared, 0);
        assert!(!proc.soft_refs[0].cleared);
    }

    // 68. A caller that DOES supply a coherent clock keeps full control: the
    //     larger of the two wins, so ZGC's backend and every test behave
    //     exactly as before the fix.
    #[test]
    fn soft_policy_caller_clock_dominates_observed_clock() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
        proc.touch_soft_reference(0x100, 1_000);

        // threshold = 1000 * 1 = 1000; idle = 1_000_000 - 1_000 > 1000.
        let result = proc.process_references(&always_dead, 1, 1_000_000);
        assert_eq!(result.stats.soft_refs_cleared, 1);
    }

    // ======================================================================
    // SPEC FIX (2026-07-26): phases 3-4 honour the soft/finalizer closures.
    // ======================================================================

    // 69. A `Cleaner` must NOT fire while its referent is still softly
    //     reachable. The live case is `DirectByteBuffer`: firing here frees
    //     native memory still reachable through `SoftReference.get()`.
    #[test]
    fn cleaner_not_fired_while_referent_softly_reachable() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        // Soft ref retains 0x200 (recently touched, ample headroom).
        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
        proc.touch_soft_reference(0x100, 5_000);
        // A Cleaner registered against that very object.
        proc.discover_reference(ReferenceType::Cleaner, 0x300, 0x200, None);

        let result = proc.process_references(&always_dead, 64, 5_000);

        assert_eq!(result.stats.soft_refs_cleared, 0, "soft ref must survive");
        assert!(
            result.cleaner_actions.is_empty(),
            "cleaner fired for a still-soft-reachable referent"
        );
        assert!(!proc.cleaner_refs[0].cleared);
    }

    // 70. `finalize()` must NOT be scheduled while the object is still softly
    //     reachable.
    #[test]
    fn finalizer_not_enqueued_while_referent_softly_reachable() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
        proc.touch_soft_reference(0x100, 5_000);
        proc.discover_reference(ReferenceType::Finalizer, 0x300, 0x200, None);

        let result = proc.process_references(&always_dead, 64, 5_000);

        assert_eq!(result.stats.soft_refs_cleared, 0);
        assert_eq!(result.stats.finalizer_refs_enqueued, 0);
        assert!(result.to_finalize.is_empty());
    }

    // 71. A phantom must wait until the referent has been finalized, then fire
    //     on the following cycle (bounded one-cycle delay, never permanent).
    #[test]
    fn phantom_waits_one_cycle_for_finalization() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x200, None);
        proc.discover_reference(ReferenceType::Phantom, 0x300, 0x200, Some(0x400));

        let r1 = proc.process_references(&always_dead, 64, 0);
        assert_eq!(r1.stats.finalizer_refs_enqueued, 1);
        assert_eq!(
            r1.stats.phantom_refs_enqueued, 0,
            "phantom must not fire before finalization"
        );

        // The finalizer entry is now flagged enqueued, so it stops rooting the
        // object and the phantom is delivered.
        let r2 = proc.process_references(&always_dead, 64, 0);
        assert_eq!(r2.stats.phantom_refs_enqueued, 1);
        assert!(r2.to_enqueue.iter().any(|&(r, q)| r == 0x300 && q == 0x400));
    }

    // 72. Mutually-reachable finalizable objects must BOTH be enqueued in the
    //     same cycle. This pins the deliberate asymmetry: the finalizer phase
    //     folds in the soft closure but NOT the finalizer closure, otherwise
    //     each object would block the other forever.
    #[test]
    fn mutually_reachable_finalizers_both_enqueued() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x200, None);
        proc.discover_reference(ReferenceType::Finalizer, 0x300, 0x400, None);

        // 0x200 and 0x400 reference each other.
        let trace = |roots: &[usize]| -> Vec<usize> {
            let mut out = roots.to_vec();
            if roots.contains(&0x200) {
                out.push(0x400);
            }
            if roots.contains(&0x400) {
                out.push(0x200);
            }
            out
        };

        let result =
            proc.process_references_with_finalizer_trace(&always_dead, Some(&trace), 64, 0);
        assert_eq!(
            result.stats.finalizer_refs_enqueued, 2,
            "finalizer-reachability must not block finalization"
        );
    }

    // 73. No over-retention: an unrelated dead cleaner/phantom referent still
    //     fires while a soft reference survives elsewhere.
    #[test]
    fn unrelated_cleaner_and_phantom_still_fire_with_surviving_soft_ref() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
        proc.touch_soft_reference(0x100, 5_000);
        // Unrelated dead referents.
        proc.discover_reference(ReferenceType::Cleaner, 0x300, 0x999, None);
        proc.discover_reference(ReferenceType::Phantom, 0x500, 0x888, Some(0x600));

        let result = proc.process_references(&always_dead, 64, 5_000);

        assert_eq!(result.stats.soft_refs_cleared, 0);
        assert!(result.cleaner_actions.contains(&0x300));
        assert_eq!(result.stats.phantom_refs_enqueued, 1);
    }
}

/// DBG (CRATONVM_DBG_DM) -- see `gc_and_alloc::dm_dbg_enabled`. Cached gate.
fn dm_dbg_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DM").is_some())
}

/// `jdk.internal.ref.Cleaner`s discovered as run-instead-of-enqueue phantoms.
static DM_PC_DISCOVERED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Cleaner actions emitted by reference processing.
static DM_PC_EMITTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Reference-processing rounds, for the periodic tally.
static DM_REFPROC_ROUNDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
