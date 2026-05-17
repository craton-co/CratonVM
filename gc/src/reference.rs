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
        };
        match ref_type {
            ReferenceType::Soft => {
                let idx = self.soft_refs.len();
                self.soft_ref_lru_index
                    .insert((entry.last_access_time_ms, idx), idx);
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
    /// Cost is O(log n) for the BTreeMap re-key plus O(1) for the entry
    /// update; no global lock beyond the caller's existing
    /// `Mutex<ReferenceProcessor>` is taken.
    pub fn touch_soft_reference(&mut self, reference_obj: usize, now_ms: u64) {
        // Linear search is acceptable here: this is called only on the
        // SoftReference.get() path (rare compared to ordinary loads) and
        // soft-ref populations are typically in the hundreds for caches.
        // If profiling reveals this as a bottleneck the index can be
        // augmented with a `FxHashMap<usize, usize>` from reference_obj
        // to soft_refs index.
        for (idx, entry) in self.soft_refs.iter_mut().enumerate() {
            if entry.reference_obj == reference_obj {
                let old_key = (entry.last_access_time_ms, idx);
                // Only re-insert when the timestamp actually advances;
                // many caches `get()` faster than the timer resolution.
                if now_ms == entry.last_access_time_ms {
                    return;
                }
                self.soft_ref_lru_index.remove(&old_key);
                entry.last_access_time_ms = now_ms;
                self.soft_ref_lru_index.insert((now_ms, idx), idx);
                return;
            }
        }
    }

    // -- Main entry point ---------------------------------------------------

    /// Process all reference types in HotSpot order.
    pub fn process_references(
        &mut self,
        is_marked: &dyn Fn(usize) -> bool,
        free_heap_mb: usize,
        current_time_ms: u64,
    ) -> ReferenceProcessingResult {
        self.stats = ReferenceProcessingStats::default();

        self.stats.soft_refs_discovered = self.soft_refs.len();
        self.stats.weak_refs_discovered = self.weak_refs.len();
        self.stats.phantom_refs_discovered = self.phantom_refs.len();
        self.stats.finalizer_refs_discovered = self.finalizer_refs.len();

        // Phase 1-4
        self.process_soft_refs(is_marked, free_heap_mb, current_time_ms);
        self.process_weak_refs(is_marked);
        self.process_final_refs(is_marked);
        self.process_phantom_refs(is_marked);

        // Build result
        let mut to_enqueue = Vec::new();
        let mut to_finalize = Vec::new();
        let mut cleaner_actions = Vec::new();

        // Gather enqueue pairs from pending_queues
        for (&queue_addr, refs) in &self.pending_queues {
            for &ref_obj in refs {
                to_enqueue.push((ref_obj, queue_addr));
            }
        }

        to_finalize.extend(self.finalization_queue.iter().copied());

        // Cleaner actions are the reference_obj addresses of cleared cleaners
        for entry in &self.cleaner_refs {
            if entry.cleared {
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
        let threshold_ms =
            self.soft_ref_lru_policy_ms_per_mb.saturating_mul(free_heap_mb as u64);

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
            let entry = &mut self.soft_refs[idx];
            if entry.cleared {
                continue;
            }
            if is_marked(entry.referent) {
                continue; // referent still live
            }
            // Double-check LRU policy (the BTreeMap range is an approximation
            // since we key on insertion time; re-verify the exact idle window).
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
            }
        }
    }

    // -- Phase 2: WeakReferences -------------------------------------------

    fn process_weak_refs(&mut self, is_marked: &dyn Fn(usize) -> bool) {
        for entry in &mut self.weak_refs {
            if entry.cleared {
                continue;
            }
            if is_marked(entry.referent) {
                continue;
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
                            tracing::warn!(
                                "update_after_gc: null target for queue 0x{:x}",
                                q
                            );
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
    pub fn remove_collected(&mut self, is_live: &dyn Fn(usize) -> bool) {
        self.soft_refs.retain(|e| is_live(e.reference_obj));
        self.weak_refs.retain(|e| is_live(e.reference_obj));
        self.phantom_refs.retain(|e| is_live(e.reference_obj));
        self.cleaner_refs.retain(|e| is_live(e.reference_obj));
        self.finalizer_refs.retain(|e| is_live(e.reference_obj));
    }

    /// Return reference_obj addresses of all entries whose referent was cleared.
    /// The caller should null the referent field (field 0) on each of these objects.
    pub fn cleared_ref_objects(&self) -> Vec<usize> {
        let mut result = Vec::new();
        for e in &self.soft_refs {
            if e.cleared { result.push(e.reference_obj); }
        }
        for e in &self.weak_refs {
            if e.cleared { result.push(e.reference_obj); }
        }
        // Phantom refs: Java 9+ does NOT clear the referent, but we enqueue them.
        // Cleaners: cleared flag used for cleaner actions, already handled.
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

    /// Return the referent addresses of all registered (non-cleared, non-enqueued)
    /// finalizer references.  Used before GC to add them as resurrection roots.
    pub fn finalizer_referent_addresses(&self) -> Vec<usize> {
        self.finalizer_refs
            .iter()
            .filter(|e| !e.cleared && !e.enqueued)
            .map(|e| e.referent)
            .collect()
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
        let _result = proc.process_references(&always_dead, 100, 0);
        assert_eq!(proc.dequeue_for_finalization(), Some(100));
        assert_eq!(proc.dequeue_for_finalization(), Some(200));
        assert_eq!(proc.dequeue_for_finalization(), Some(300));
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
        proc.discover_reference(ReferenceType::Finalizer, 1, 100, None);
        let _result = proc.process_references(&always_dead, 100, 0);
        assert_eq!(proc.finalization_queue, vec![100]);
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
        assert!(ft.was_finalized(200));  // still tracked
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
        assert!(start.elapsed().as_millis() < 100, "should return immediately");
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
        assert!(elapsed >= 40, "should wait close to timeout duration: {elapsed}ms");
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
        proc.discover_reference(ReferenceType::Finalizer, 1, 0xBEEF, None);
        let _result = proc.process_references(&always_dead, 100, 0);
        assert_eq!(proc.finalization_queue, vec![0xBEEF]);
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
}
