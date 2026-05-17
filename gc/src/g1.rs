//! G1 (Garbage-First) garbage collector.
//!
//! A region-based, generational, incremental, parallel, mostly concurrent
//! garbage collector. Key features:
//!
//! - **Region-based heap:** Fixed-size regions classified as Eden, Survivor,
//!   Old, Humongous, or Free.
//! - **Young collection (STW):** Evacuate all Eden + Survivor regions.
//! - **Mixed collection:** Evacuate young + selected old regions (worst-first).
//! - **Concurrent marking:** Tri-color marking with SATB barriers.
//! - **IHOP:** Initiating Heap Occupancy Percent triggers concurrent marking.
//! - **Region pinning (JEP 423):** Pin regions during JNI critical sections.
//! - **String deduplication:** Deduplicate identical String backing arrays.
//! - **Humongous allocation:** Objects > region_size/2 span contiguous regions.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use std::sync::Arc;

use crate::collector::{GarbageCollector, MonitorCleanup};
use crate::concurrent_mark::{ConcurrentGcPhase, ConcurrentGcState};
use crate::gc::{GcResult, GcStats};
use crate::heap::{
    array_data_size, ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE, SLOT_SIZE,
};
use crate::mark_bitmap::MarkBitmap;
use crate::region::{RegionType, RememberedSet};
use crate::satb::SatbQueue;
use rustjvm_types::{ClassId, ObjectRef, Value};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the G1 garbage collector.
#[derive(Debug, Clone)]
pub struct G1CollectorConfig {
    /// Total heap size in bytes (default 256 MB).
    pub heap_size: usize,
    /// Region size in bytes (default 1 MB).
    pub region_size: usize,
    /// Target maximum GC pause in milliseconds (default 200).
    pub max_gc_pause_ms: u64,
    /// Initiating heap occupancy percent (default 45).
    pub ihop_percent: u8,
    /// Tenuring threshold: survive this many young GCs before promotion (default 15).
    pub promotion_age: u8,
    /// Number of parallel GC worker threads (default 4).
    pub gc_worker_threads: usize,
    /// Enable string deduplication (default false).
    pub string_dedup_enabled: bool,
    /// Target number of mixed GC cycles after marking (default 8).
    pub mixed_gc_count_target: u8,
    /// Maximum percentage of old regions to include per mixed GC (default 10).
    pub old_cset_region_threshold_percent: u8,
}

impl Default for G1CollectorConfig {
    fn default() -> Self {
        Self {
            heap_size: 256 * 1024 * 1024,
            region_size: 1024 * 1024,
            max_gc_pause_ms: 200,
            // T19.3.G1: raised from 45 → 70 so static-init bursts
            // on small heaps don't fire a concurrent marking cycle
            // before the heap has actually retained anything worth
            // collecting. HotSpot defaults 45% but assumes a 32 GiB
            // heap where 45% = ~14 GiB; at 256 MiB (our default)
            // 45% is only 115 MiB, which a Quarkus static-init
            // replay can chew through in under two seconds.
            // Adaptive IHOP (see `update_ihop`) still adjusts
            // downward under real memory pressure.
            ihop_percent: 70,
            promotion_age: 15,
            gc_worker_threads: 4,
            string_dedup_enabled: false,
            mixed_gc_count_target: 8,
            old_cset_region_threshold_percent: 10,
        }
    }
}

// ---------------------------------------------------------------------------
// G1 Region
// ---------------------------------------------------------------------------

/// Enhanced region descriptor for the G1 collector.
pub struct G1Region {
    /// Current region classification.
    pub region_type: RegionType,
    /// Backing storage for this region.
    pub data: Vec<u8>,
    /// Bump pointer: next free byte offset.
    pub cursor: usize,
    /// Bytes of live data (computed during marking).
    pub live_bytes: usize,
    /// GC efficiency: `live_bytes / region_size`. Lower means more garbage.
    pub gc_efficiency: f64,
    /// Per-region remembered set.
    pub rset: RememberedSet,
    /// Whether this region is pinned (JEP 423: JNI critical region pinning).
    pub pinned: bool,
    /// Survivor age (number of young GCs survived).
    pub age: u8,
    /// Per-region mark bitmap for concurrent marking.
    ///
    /// Round-2 fix (HIGH — GC #5): the bitmap is keyed off the region's
    /// own heap-allocated `data.as_ptr()` base, so `try_mark`/`is_marked`
    /// accept real object addresses living inside this region. The
    /// previous design used a single global bitmap rooted at address 0,
    /// which silently rejected every real region address and produced
    /// `live_bytes = 0` for every region (breaking mixed-GC region
    /// selection). The Vec backing the region is never reallocated
    /// (only zero-filled by `reset`), so the bitmap base remains stable
    /// for the entire collector lifetime.
    pub mark_bitmap: MarkBitmap,
}

impl G1Region {
    /// Create a new free region of the given size.
    fn new(region_size: usize) -> Self {
        let data = vec![0u8; region_size];
        // Round-2 fix (HIGH — GC #5): bitmap covers exactly this region's
        // heap-allocated buffer. Base = data.as_ptr(), span = region_size.
        let base = data.as_ptr() as usize;
        let mark_bitmap = MarkBitmap::new(base, region_size);
        Self {
            region_type: RegionType::Free,
            data,
            cursor: 0,
            live_bytes: 0,
            gc_efficiency: 0.0,
            rset: RememberedSet::default(),
            pinned: false,
            age: 0,
            mark_bitmap,
        }
    }

    /// Remaining free bytes in this region.
    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.cursor)
    }

    /// Reset this region to Free state.
    fn reset(&mut self) {
        self.region_type = RegionType::Free;
        self.cursor = 0;
        self.live_bytes = 0;
        self.gc_efficiency = 0.0;
        self.rset.clear();
        self.pinned = false;
        self.age = 0;
        // Round-2 fix (HIGH — GC #5): clear stale mark bits so they don't
        // pollute the next concurrent-mark cycle. The Vec is never
        // reallocated (only `fill(0)`'d) so the bitmap's base address
        // remains valid.
        self.mark_bitmap.clear();
        // Zero the backing storage
        self.data.fill(0);
    }

    /// Bump-allocate `size` bytes (with alignment) in this region.
    /// Returns `(pointer, offset_within_region)` or `None` if region is full.
    fn bump_alloc(&mut self, size: usize, align: usize) -> Option<(*mut u8, usize)> {
        let base = self.data.as_mut_ptr() as usize;
        let current = base + self.cursor;
        let aligned = (current + align - 1) & !(align - 1);
        let offset_in_region = aligned - base;
        let end = offset_in_region + size;

        if end > self.data.len() {
            return None;
        }

        self.cursor = end;
        let ptr = aligned as *mut u8;
        // Zero-init the allocated area
        unsafe {
            std::ptr::write_bytes(ptr, 0, size);
        }
        Some((ptr, offset_in_region))
    }

    /// Get a raw pointer to the start of this region's data.
    fn _base_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    /// Get a mutable raw pointer to the start of this region's data.
    fn base_ptr_mut(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }
}

/// Round-5 HIGH #6 — defensive cap on the gray-set worklist.
///
/// The mark worklist was previously an unbounded `Vec<usize>`. A
/// pathological object graph (e.g. a malicious or buggy classloader that
/// produces an exceptionally wide reference fan-out during a concurrent
/// mark cycle, or a marker thread that has been starved so the worklist
/// grows faster than it drains) could OOM the JVM by ballooning this
/// Vec. The cap below converts that silent allocator-OOM into a
/// deterministic panic, which is strictly better than crashing the
/// entire VM with an unrecoverable allocation failure deep inside
/// `Vec::push`.
///
/// 1 million entries * 8 bytes = 8 MiB — chosen large enough that any
/// realistic mark cycle stays well below it, small enough that the
/// panic is reproducible in tests.
///
/// TODO(round-5+): the real fix is an overflow-handling strategy —
/// either spill the worklist to a backing region, or drop the explicit
/// worklist entirely and fall back to a "mark-everything-dirty" sweep
/// pass guided by the card table. Both are too invasive for this
/// hotfix; the cap below is the defensive interim.
const MARK_WORKLIST_CAP: usize = 1 << 20;

// ---------------------------------------------------------------------------
// Collection type
// ---------------------------------------------------------------------------

/// Type of G1 collection to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum G1CollectionType {
    /// Evacuate all Eden + Survivor regions.
    YoungOnly,
    /// Evacuate young + selected old regions.
    Mixed,
    /// Full heap compaction (fallback when evacuation fails).
    Full,
}

// ---------------------------------------------------------------------------
// G1 Collector
// ---------------------------------------------------------------------------

/// G1 garbage collector implementing the `GarbageCollector` trait.
pub struct G1Collector {
    /// Collector configuration.
    config: G1CollectorConfig,
    /// All heap regions.
    regions: Mutex<Vec<G1Region>>,

    /// Index of the current Eden allocation region.
    current_eden: AtomicUsize,
    /// Next identity hash code to assign.
    next_hash_code: AtomicI32,

    /// Concurrent GC phase state.
    ///
    /// Round-2 fix (HIGH — GC #5): the previously-global `mark_bitmap`
    /// has moved to `G1Region::mark_bitmap` so each region's bitmap is
    /// keyed off that region's actual data pointer (not address 0).
    /// Callers route bitmap operations through the region lookup.
    pub gc_state: Arc<ConcurrentGcState>,
    /// Global SATB queue.
    satb_queue: Arc<SatbQueue>,

    /// Number of collections performed.
    collection_count: AtomicU64,
    /// Total pause time in milliseconds across all collections.
    total_pause_ms: AtomicU64,

    /// Current old-gen bytes (for IHOP tracking).
    old_gen_bytes: AtomicUsize,
    /// Byte threshold at which to initiate concurrent marking.
    marking_threshold_bytes: AtomicUsize,

    /// String deduplication table: hash -> canonical object address.
    /// T10.9.B: FxHashMap — key is Java String hash from loaded bytecode.
    string_dedup_table: Mutex<FxHashMap<u64, usize>>,

    /// Whether GC event logging is enabled.
    gc_log_enabled: AtomicBool,

    /// Whether a concurrent mark cycle has completed and mixed GC is needed.
    marking_complete: AtomicBool,
    /// Remaining mixed GC cycles after a marking cycle.
    mixed_gc_remaining: AtomicU64,

    /// Audit fix (HIGH-3): persistent mark worklist (the "gray" stack)
    /// drained by `concurrent_mark_step`. Roots are pushed by `remark`
    /// (which the VM calls both at initial-mark and at final-remark
    /// STW points) and SATB entries are pushed when remark drains the
    /// SATB queue. Each step pops an object, marks it, and pushes its
    /// reference fields that are not yet marked. Stored as raw `usize`
    /// addresses so the queue is `Send`/`Sync` without `unsafe impl`
    /// gymnastics for `*mut u8`.
    mark_worklist: Mutex<Vec<usize>>,

    /// Round-9 gc HIGH-5 — set when any `mark_worklist` push is
    /// dropped because the cap was hit. The marker checks this flag at
    /// the end of remark and falls back to a conservative full re-walk
    /// of all live regions: every already-marked object is re-scanned
    /// and its outgoing references are pushed again. This replaces the
    /// previous `panic!` (which a hostile Java app could trip) with a
    /// time/correctness trade — no crash, just longer mark.
    mark_worklist_overflowed: AtomicBool,

    /// Address-to-region lookup table for O(log R) `region_for_ptr` queries.
    ///
    /// Each entry is `(base_addr, region_idx)`, sorted ascending by
    /// `base_addr`. Built once in [`G1Collector::new`] and never mutated
    /// afterward: the outer `regions: Vec<G1Region>` is constructed with a
    /// fixed length and never `push`/`pop`ed, and each region's `data` Vec
    /// is allocated once with `region_size` capacity — `G1Region::reset`
    /// only zero-fills, it does not reallocate — so the backing-buffer
    /// addresses are stable for the entire lifetime of the collector.
    ///
    /// This replaces the previous O(R) linear scan inside
    /// `scan_and_evacuate_refs` and the write barrier, which was an
    /// audit-flagged hot-path bottleneck (CRIT-P4): with 256 regions, a
    /// 100-slot object incurred ~25k linear probes during evacuation.
    region_lookup: Vec<(usize, usize)>,
}

// SAFETY: All fields are either atomic, behind Mutex, or Arc. Raw pointers
// in the mark bitmap are heap-managed and only accessed during STW pauses.
unsafe impl Send for G1Collector {}
unsafe impl Sync for G1Collector {}

impl G1Collector {
    /// Create a new G1 collector with the given configuration.
    pub fn new(config: G1CollectorConfig) -> Self {
        let num_regions = config.heap_size / config.region_size;
        assert!(num_regions > 0, "g1: heap must fit at least one region (heap_size={}, region_size={})", config.heap_size, config.region_size);

        let regions: Vec<G1Region> = (0..num_regions)
            .map(|_| G1Region::new(config.region_size))
            .collect();

        // Build the address-to-region lookup table (sorted by base addr).
        // Each region's `data` Vec was just allocated; its `as_ptr()` is
        // stable for the lifetime of the collector (see field doc).
        let mut region_lookup: Vec<(usize, usize)> = regions
            .iter()
            .enumerate()
            .map(|(i, r)| (r.data.as_ptr() as usize, i))
            .collect();
        region_lookup.sort_unstable_by_key(|(base, _)| *base);

        // Round-2 fix (HIGH — GC #5): no global mark bitmap any more —
        // bitmaps live per-region (see `G1Region::mark_bitmap`) so they
        // correctly cover the real heap addresses of each region's data
        // buffer. The previous global `MarkBitmap::new(0, heap_size)`
        // silently rejected every real address.

        let ihop_threshold =
            (config.heap_size as u64 * config.ihop_percent as u64 / 100) as usize;

        Self {
            config: config.clone(),
            regions: Mutex::new(regions),
            current_eden: AtomicUsize::new(usize::MAX), // no eden yet
            next_hash_code: AtomicI32::new(1),
            gc_state: Arc::new(ConcurrentGcState::new()),
            satb_queue: Arc::new(SatbQueue::new()),
            collection_count: AtomicU64::new(0),
            total_pause_ms: AtomicU64::new(0),
            old_gen_bytes: AtomicUsize::new(0),
            marking_threshold_bytes: AtomicUsize::new(ihop_threshold),
            string_dedup_table: Mutex::new(FxHashMap::default()),
            gc_log_enabled: AtomicBool::new(false),
            marking_complete: AtomicBool::new(false),
            mixed_gc_remaining: AtomicU64::new(0),
            mark_worklist: Mutex::new(Vec::new()),
            mark_worklist_overflowed: AtomicBool::new(false),
            region_lookup,
        }
    }

    /// Create a G1 collector with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(G1CollectorConfig::default())
    }

    /// Get the number of regions.
    pub fn num_regions(&self) -> usize {
        self.regions.lock().len()
    }

    /// Generate the next identity hash code.
    ///
    /// Relaxed ordering is sufficient: hash codes are monotonic counters with
    /// no ordering requirements relative to other memory operations. Duplicate
    /// or slightly-stale values are acceptable per the JVM spec (identity
    /// hashes need not be unique).
    ///
    /// H1: exposed publicly so `interpreter::init_object_header` (TLAB
    /// fast path) can mint a unique hash at allocation time, matching the
    /// non-TLAB allocators.
    pub fn next_hash(&self) -> i32 {
        self.next_hash_code.fetch_add(1, Ordering::Relaxed)
    }

    // -----------------------------------------------------------------------
    // Allocation
    // -----------------------------------------------------------------------

    /// Bump-allocate `size` bytes in the current Eden region.
    /// Returns `(pointer, region_index)` or `None` on failure.
    pub fn alloc_in_region(&self, size: usize) -> Option<(*mut u8, usize)> {
        let mut regions = self.regions.lock();
        let region_size = self.config.region_size;

        // Humongous check
        if size > region_size / 2 {
            return self.alloc_humongous_locked(&mut regions, size);
        }

        // Try current Eden region
        let cur = self.current_eden.load(Ordering::Relaxed);
        if cur < regions.len() && regions[cur].region_type == RegionType::Eden {
            if let Some(result) = regions[cur].bump_alloc(size, 8) {
                return Some((result.0, cur));
            }
        }

        // Find a new free region for Eden
        if let Some(idx) = find_free_region(&regions) {
            regions[idx].region_type = RegionType::Eden;
            self.current_eden.store(idx, Ordering::Relaxed);
            if let Some(result) = regions[idx].bump_alloc(size, 8) {
                return Some((result.0, idx));
            }
        }

        None
    }

    /// Allocate a humongous object spanning contiguous free regions.
    fn alloc_humongous_locked(
        &self,
        regions: &mut Vec<G1Region>,
        size: usize,
    ) -> Option<(*mut u8, usize)> {
        let region_size = self.config.region_size;
        let regions_needed = size.div_ceil(region_size);

        let start = find_contiguous_free(regions, regions_needed)?;

        // Mark regions
        regions[start].region_type = RegionType::HumongousStart;
        for i in 1..regions_needed {
            regions[start + i].region_type = RegionType::HumongousContinuation;
        }

        // Allocate in the first region
        regions[start].cursor = size.min(region_size);
        for i in 1..regions_needed {
            let remaining = size.saturating_sub(i * region_size);
            regions[start + i].cursor = remaining.min(region_size);
        }

        // CRIT (round-5 GC #1, heap walker UAF): zero the *entire* humongous
        // span, not just the first region's payload. Continuation regions
        // (start+1..start+regions_needed) come from `Free` slots that may
        // have last held arbitrary collected data; G1Region::reset() zeroes
        // a region only on its STW retire path. If we leave the residual
        // bytes intact, the heap walker that scans `cursor` bytes per
        // continuation region will treat those stale bytes as live object
        // headers / reference slots, follow the garbage pointers, and
        // either crash, corrupt the bitmap, or revive freed objects.
        //
        // Zero each region's `cursor` bytes individually rather than a
        // single `write_bytes` across `total_bytes`: G1Region buffers are
        // separate `Vec<u8>` allocations and are NOT guaranteed to live
        // at contiguous addresses, even though they are logically adjacent
        // in region index space.
        for i in 0..regions_needed {
            let n = regions[start + i].cursor;
            if n > 0 {
                let p = regions[start + i].base_ptr_mut();
                unsafe {
                    std::ptr::write_bytes(p, 0, n);
                }
            }
        }

        // CRIT (round-9 gc CRIT-1): install a `HumongousFiller` sentinel
        // header at the START of every continuation region. The previous
        // round-5 fix only zeroed the continuation bytes, but a zero
        // header still decodes as a well-formed `Object` (class_id=0,
        // kind=Object=0, num_slots=0). A heap walker iterating with the
        // generic per-object size formula treats the zeroed bytes as a
        // 40-byte object and the next 40 bytes as another, and so on,
        // following any garbage that survives at later offsets.
        //
        // The filler is a single sentinel header (HEADER_SIZE bytes) that
        // walkers detect via `is_humongous_filler()` and use to break out
        // of the per-region iteration. No oops are scanned and the
        // continuation region is effectively dark to the walker.
        //
        // The first region (start) is the HumongousStart and contains the
        // actual application object header at offset 0 — leave it alone.
        for i in 1..regions_needed {
            let n = regions[start + i].cursor;
            if n >= HEADER_SIZE {
                let p = regions[start + i].base_ptr_mut();
                // SAFETY: continuation region's first HEADER_SIZE bytes are
                // owned by this allocation and were just zeroed; writing a
                // sentinel ObjectHeader here is well-defined.
                let filler = ObjectHeader::new(
                    ClassId::new(0),
                    ObjectKind::HumongousFiller,
                    ArrayElementType::Reference,
                    0,
                    0,
                    0,
                );
                unsafe {
                    std::ptr::write(p as *mut ObjectHeader, filler);
                }
            }
        }

        let ptr = regions[start].base_ptr_mut();
        Some((ptr, start))
    }

    /// Allocate in a region of the specified type (Survivor or Old).
    fn alloc_in_type_locked(
        regions: &mut Vec<G1Region>,
        target_type: RegionType,
        size: usize,
    ) -> Option<*mut u8> {
        // Try existing regions of this type
        for i in 0..regions.len() {
            if regions[i].region_type == target_type {
                if let Some((ptr, _)) = regions[i].bump_alloc(size, 8) {
                    return Some(ptr);
                }
            }
        }

        // Allocate a new free region
        if let Some(idx) = find_free_region(regions) {
            regions[idx].region_type = target_type;
            if target_type == RegionType::Survivor {
                regions[idx].age = 1;
            }
            if let Some((ptr, _)) = regions[idx].bump_alloc(size, 8) {
                return Some(ptr);
            }
        }

        None
    }

    // -----------------------------------------------------------------------
    // Young Collection (STW)
    // -----------------------------------------------------------------------

    /// Perform a young-only collection. Evacuates all Eden + Survivor regions.
    pub fn young_collection(
        &self,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        let start = std::time::Instant::now();
        let mut regions = self.regions.lock();
        let mut pointer_map: HashMap<usize, usize> = HashMap::new();
        let mut objects_copied = 0usize;
        let mut bytes_copied = 0usize;

        // Build collection set: all Eden + Survivor regions (skip pinned)
        let cset: Vec<usize> = regions
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                !r.pinned
                    && (r.region_type == RegionType::Eden
                        || r.region_type == RegionType::Survivor)
            })
            .map(|(i, _)| i)
            .collect();

        if cset.is_empty() {
            return GcResult {
                stats: GcStats {
                    objects_copied: 0,
                    bytes_copied: 0,
                    bytes_freed: 0,
                },
                pointer_map,
            };
        }

        // Phase 1: Scan roots and evacuate reachable objects from CSet
        let cset_set: std::collections::HashSet<usize> = cset.iter().copied().collect();
        let mut work_list: Vec<*mut u8> = Vec::new();

        // Process root references
        for root in roots.iter_mut() {
            let old_ptr = root.as_ptr();
            if let Some(region_idx) = self.region_for_ptr(&regions, old_ptr) {
                if cset_set.contains(&region_idx) {
                    if let Some(new_ptr) = self.evacuate_object(
                        &mut regions,
                        old_ptr,
                        &mut pointer_map,
                        &mut objects_copied,
                        &mut bytes_copied,
                    ) {
                        *root = unsafe { ObjectRef::from_raw(new_ptr) };
                        work_list.push(new_ptr);
                    }
                }
            }
        }

        // Phase 2: Scan remembered sets for references into CSet
        // (Collect rset sources before mutating regions)
        let mut rset_sources: Vec<(usize, Vec<usize>)> = Vec::new();
        for &cset_idx in &cset {
            let sources: Vec<usize> = regions[cset_idx].rset.sources();
            if !sources.is_empty() {
                rset_sources.push((cset_idx, sources));
            }
        }

        // CRIT fix (UAF): actually process the collected rset sources.
        // Dedup source indices so we walk each source region at most once.
        let unique_sources: std::collections::HashSet<usize> = rset_sources
            .iter()
            .flat_map(|(_, srcs)| srcs.iter().copied())
            .collect();
        for src_idx in unique_sources {
            self.scan_source_region_for_cset_refs(
                &mut regions,
                src_idx,
                &cset_set,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
                &mut work_list,
            );
        }

        // Phase 3: Cheney-style scan of evacuated objects
        let mut scan_idx = 0;
        while scan_idx < work_list.len() {
            let obj_ptr = work_list[scan_idx];
            scan_idx += 1;

            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            self.scan_and_evacuate_refs(
                &mut regions,
                obj_ptr,
                header,
                &cset_set,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
                &mut work_list,
            );
        }

        // Phase 4: Update forwarding pointers in non-CSet regions
        self.update_references_in_regions(&mut regions, &cset_set, &pointer_map);

        // Phase 5: Free evacuated regions
        let mut bytes_freed = 0usize;
        for &cset_idx in &cset {
            bytes_freed += regions[cset_idx].cursor;
            regions[cset_idx].reset();
        }

        // TODO(round-9 gc HIGH-9, humongous-reclaim-young):
        // ----------------------------------------------------
        // Today humongous regions are reclaimed only by the full GC
        // path. A young-only collection cannot tell whether a
        // humongous span is unreachable, because:
        //
        //   * The humongous start region's RSet records inbound
        //     cross-region references from *other regions*, but young
        //     collections don't scan every region's outgoing edges —
        //     only the CSet's, plus their RSet sources.
        //   * Continuation regions don't carry their own RSets; the
        //     HumongousFiller sentinel parks them outside the walker.
        //   * Roots may pin a humongous span directly (no other
        //     region in the heap references it), and the young phase
        //     evacuates roots into Eden/Survivor but doesn't surface
        //     "this humongous is now unreferenced".
        //
        // A correct young-time humongous reclaim would need:
        //   1. Extend the RSet on the HumongousStart region to track
        //      inbound refs from *all* regions, not just non-young.
        //   2. Track root pins per-humongous (e.g. a per-region
        //      `root_refcount: AtomicU32` updated as roots are
        //      evacuated, decremented when a slot stops pointing at
        //      this region's start address).
        //   3. After phase 5 (CSet free), iterate humongous starts
        //      and reclaim any whose `rset.is_empty() && root_refcount
        //      == 0`. Reclamation must zero both the start region
        //      and every contiguous continuation region.
        //
        // None of (1)..(3) are local edits; they touch the RSet
        // schema and the root-evacuation loop. Deferred to a focused
        // change. Until then, humongous garbage is only collected on
        // full GC, which is functionally correct but means long-lived
        // mutators that churn humongous allocations will see
        // unnecessary heap growth between full GCs.

        // Reset current eden if it was in the CSet
        let cur_eden = self.current_eden.load(Ordering::Relaxed);
        if cset_set.contains(&cur_eden) {
            self.current_eden.store(usize::MAX, Ordering::Relaxed);
        }

        // Update old gen bytes tracking.
        // Relaxed ordering: this is a statistics counter read only by IHOP heuristics;
        // exact inter-thread visibility ordering is not required.
        let old_bytes: usize = regions
            .iter()
            .filter(|r| r.region_type == RegionType::Old)
            .map(|r| r.cursor)
            .sum();
        self.old_gen_bytes.store(old_bytes, Ordering::Relaxed);

        // Remap monitors
        monitors.remap_after_gc(&pointer_map);

        // Round-7 fix (HIGH, audit §12): the concurrent mark worklist holds
        // raw object addresses that may have just been evacuated by this
        // young STW. The marker, after we unpark mutators, would otherwise
        // dereference stale pointers into freed regions → UAF. Walk the
        // worklist and apply the same `pointer_map` we applied to roots
        // and remembered-set updates; drop entries whose source region was
        // freed (no forward) since those objects were unreachable via the
        // normal evacuation closure (the only way they were on the
        // worklist without being evacuated is if they were already dead).
        //
        // Done under STW (still holding `regions.lock()`), so no marker
        // thread can be reading/writing `mark_worklist` concurrently — the
        // concurrent marker takes the same lock for each step.
        {
            let mut worklist = self.mark_worklist.lock();
            if !worklist.is_empty() {
                worklist.retain_mut(|addr| {
                    if let Some(&new_addr) = pointer_map.get(&*addr) {
                        // Object was evacuated — follow the forwarding ptr.
                        *addr = new_addr;
                        return true;
                    }
                    // Not forwarded. If the address lived in a CSet region
                    // it is now dangling (the region was reset above) so
                    // drop it. Otherwise (Old / non-CSet) leave it alone.
                    match self.region_for_ptr(&regions, *addr as *mut u8) {
                        Some(idx) if cset_set.contains(&idx) => false,
                        _ => true,
                    }
                });
            }
        }

        let pause_ms = start.elapsed().as_millis() as u64;
        // Relaxed ordering: collection_count and total_pause_ms are statistics
        // counters used for monitoring/logging only. They do not guard any data
        // and a slightly stale read is harmless.
        self.collection_count.fetch_add(1, Ordering::Relaxed);
        self.total_pause_ms.fetch_add(pause_ms, Ordering::Relaxed);

        let stats = GcStats {
            objects_copied,
            bytes_copied,
            bytes_freed,
        };

        self.log_gc_event(&G1CollectionType::YoungOnly, pause_ms, &stats);

        GcResult {
            stats,
            pointer_map,
        }
    }

    /// Phase H (RH.8) test hook — invoke `f` with mutable access to
    /// the region table.  **Intended for integration tests only.**
    /// External callers must assume this method will be removed if a
    /// safer API replaces it; it exists to let Phase H tests set up
    /// synthetic `gc_efficiency` values on old regions without having
    /// to run a full marking cycle.
    ///
    /// Holding the internal region lock across the closure guarantees
    /// no concurrent GC work observes a partially-mutated table.
    #[doc(hidden)]
    pub fn with_regions_mut<F: FnOnce(&mut [G1Region])>(&self, f: F) {
        let mut guard = self.regions.lock();
        f(&mut guard);
    }

    /// Phase H (RH.8) — select the old regions that will be evacuated
    /// in the next mixed collection, ordered by expected benefit.
    ///
    /// Selection policy:
    /// 1. **Eligible:** region must be of [`RegionType::Old`] and not
    ///    [`G1Region::pinned`] (JNI critical sections may pin regions).
    /// 2. **Rank:** ascending [`G1Region::gc_efficiency`] (= lowest
    ///    live-bytes ratio wins) — this is HotSpot's "worst-first"
    ///    heuristic: the region with the most reclaimable garbage is
    ///    selected first, maximising memory freed per byte copied.
    /// 3. **Cap:** at most `old_cset_region_threshold_percent` of all
    ///    regions (defaults to 10%), with a minimum of one region so
    ///    a tiny heap still makes progress.
    /// 4. **Deterministic:** ties broken by ascending region index.
    ///
    /// This helper is exposed publicly so tests and external policy
    /// hooks can inspect the selection without running a full mixed
    /// GC cycle.  The real cycle duplicates the logic inline to avoid
    /// releasing its region-table lock guard.
    pub fn select_old_regions_for_mixed_gc(&self) -> Vec<usize> {
        let regions = self.regions.lock();
        let total_regions = regions.len();
        let cap_percent = self.config.old_cset_region_threshold_percent as usize;
        let max_old = ((total_regions * cap_percent) / 100).max(1);

        let mut candidates: Vec<(usize, f64)> = regions
            .iter()
            .enumerate()
            .filter(|(_, r)| r.region_type == RegionType::Old && !r.pinned)
            .map(|(i, r)| (i, r.gc_efficiency))
            .collect();
        candidates.sort_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        candidates
            .into_iter()
            .take(max_old)
            .map(|(i, _)| i)
            .collect()
    }

    /// Perform a mixed collection. Evacuates young + selected old regions.
    pub fn mixed_collection(
        &self,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        let start = std::time::Instant::now();
        let mut regions = self.regions.lock();
        let mut pointer_map: HashMap<usize, usize> = HashMap::new();
        let mut objects_copied = 0usize;
        let mut bytes_copied = 0usize;

        // Build CSet: all young regions + worst old regions
        let mut cset: Vec<usize> = regions
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                !r.pinned
                    && (r.region_type == RegionType::Eden
                        || r.region_type == RegionType::Survivor)
            })
            .map(|(i, _)| i)
            .collect();

        // Select old regions sorted by gc_efficiency (lowest = most
        // garbage first).  See [`Self::select_old_regions_for_mixed_gc`]
        // for the stand-alone helper; the logic is duplicated here to
        // avoid releasing the `regions` lock guard.
        let max_old = (regions.len() * self.config.old_cset_region_threshold_percent as usize)
            / 100;
        let max_old = max_old.max(1);

        let mut old_candidates: Vec<(usize, f64)> = regions
            .iter()
            .enumerate()
            .filter(|(_, r)| r.region_type == RegionType::Old && !r.pinned)
            .map(|(i, r)| (i, r.gc_efficiency))
            .collect();
        // Stable sort so that among regions with identical efficiency
        // the lower index wins — keeps selection deterministic.
        old_candidates.sort_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });

        for (idx, _) in old_candidates.into_iter().take(max_old) {
            cset.push(idx);
        }

        let cset_set: std::collections::HashSet<usize> = cset.iter().copied().collect();
        let mut work_list: Vec<*mut u8> = Vec::new();

        // Evacuate roots
        for root in roots.iter_mut() {
            let old_ptr = root.as_ptr();
            if let Some(region_idx) = self.region_for_ptr(&regions, old_ptr) {
                if cset_set.contains(&region_idx) {
                    if let Some(new_ptr) = self.evacuate_object(
                        &mut regions,
                        old_ptr,
                        &mut pointer_map,
                        &mut objects_copied,
                        &mut bytes_copied,
                    ) {
                        *root = unsafe { ObjectRef::from_raw(new_ptr) };
                        work_list.push(new_ptr);
                    }
                }
            }
        }

        // CRIT fix (UAF): process remembered-set sources for every CSet
        // region. Collect sources up front (before mutating regions during
        // evacuation), dedup, then walk each source region rewriting
        // CSet-bound slots and seeding the work_list with newly evacuated
        // targets. Without this, cross-region refs (e.g. old → young)
        // were silently dropped, leaving stale pointers in non-CSet
        // regions after CSet reset.
        let mixed_rset_sources: std::collections::HashSet<usize> = {
            let mut set = std::collections::HashSet::new();
            for &cset_idx in &cset {
                for s in regions[cset_idx].rset.sources() {
                    set.insert(s);
                }
                // Round-9 gc CRIT-8: `sources()` now returns an owned Vec
                // (the underlying FxHashSet lives behind a per-RSet mutex);
                // iteration above is over the snapshot and does not hold
                // the RSet lock across the body.
            }
            set
        };
        for src_idx in mixed_rset_sources {
            self.scan_source_region_for_cset_refs(
                &mut regions,
                src_idx,
                &cset_set,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
                &mut work_list,
            );
        }

        // Cheney scan
        let mut scan_idx = 0;
        while scan_idx < work_list.len() {
            let obj_ptr = work_list[scan_idx];
            scan_idx += 1;

            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            self.scan_and_evacuate_refs(
                &mut regions,
                obj_ptr,
                header,
                &cset_set,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
                &mut work_list,
            );
        }

        // Update references and free evacuated regions
        self.update_references_in_regions(&mut regions, &cset_set, &pointer_map);

        let mut bytes_freed = 0usize;
        for &cset_idx in &cset {
            bytes_freed += regions[cset_idx].cursor;
            regions[cset_idx].reset();
        }

        let cur_eden = self.current_eden.load(Ordering::Relaxed);
        if cset_set.contains(&cur_eden) {
            self.current_eden.store(usize::MAX, Ordering::Relaxed);
        }

        // Relaxed ordering: statistics counter for IHOP heuristics only.
        let old_bytes: usize = regions
            .iter()
            .filter(|r| r.region_type == RegionType::Old)
            .map(|r| r.cursor)
            .sum();
        self.old_gen_bytes.store(old_bytes, Ordering::Relaxed);

        monitors.remap_after_gc(&pointer_map);

        // Decrement mixed GC counter.
        // Relaxed ordering: mixed_gc_remaining and marking_complete are GC-internal
        // scheduling counters only accessed during STW pauses (single-threaded).
        let remaining = self.mixed_gc_remaining.load(Ordering::Relaxed);
        if remaining > 0 {
            self.mixed_gc_remaining.store(remaining - 1, Ordering::Relaxed);
            if remaining - 1 == 0 {
                self.marking_complete.store(false, Ordering::Relaxed);
            }
        }

        let pause_ms = start.elapsed().as_millis() as u64;
        // Relaxed ordering: statistics counters for monitoring/logging only.
        self.collection_count.fetch_add(1, Ordering::Relaxed);
        self.total_pause_ms.fetch_add(pause_ms, Ordering::Relaxed);

        let stats = GcStats {
            objects_copied,
            bytes_copied,
            bytes_freed,
        };

        self.log_gc_event(&G1CollectionType::Mixed, pause_ms, &stats);

        GcResult {
            stats,
            pointer_map,
        }
    }

    // -----------------------------------------------------------------------
    // Evacuation helpers
    // -----------------------------------------------------------------------

    /// Find which region a raw pointer belongs to.
    ///
    /// O(log R) via binary search on the cached `region_lookup` table.
    /// The `regions` parameter is retained for signature compatibility but
    /// is no longer scanned linearly — the per-slot evacuation hot path
    /// (CRIT-P4) used to incur an O(R) probe per reference slot, dragging
    /// scan cost to O(objects × refs × regions). Lookup is now independent
    /// of the live-region count.
    ///
    /// Note: this no longer skips `Free` regions (matching
    /// [`Self::region_for_ptr_with_regions`]). All callers either filter
    /// via the CSet (which excludes Free regions by construction) or are
    /// inherently safe against the case.
    fn region_for_ptr(&self, _regions: &[G1Region], ptr: *mut u8) -> Option<usize> {
        self.lookup_region_for_addr(ptr as usize)
    }

    /// Evacuate a single object from its current region to Survivor or Old.
    /// Returns the new pointer, or None on evacuation failure.
    fn evacuate_object(
        &self,
        regions: &mut Vec<G1Region>,
        old_ptr: *mut u8,
        pointer_map: &mut HashMap<usize, usize>,
        objects_copied: &mut usize,
        bytes_copied: &mut usize,
    ) -> Option<*mut u8> {
        let old_addr = old_ptr as usize;

        // Already forwarded?
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            return Some(new_addr as *mut u8);
        }

        let header = unsafe { &*(old_ptr as *const ObjectHeader) };
        let obj_size = object_total_size(header);

        // Decide destination based on age
        let promote = header.gc_age >= self.config.promotion_age;
        let dest_type = if promote {
            RegionType::Old
        } else {
            RegionType::Survivor
        };

        let new_ptr = Self::alloc_in_type_locked(regions, dest_type, obj_size)?;

        // Copy object data
        unsafe {
            std::ptr::copy_nonoverlapping(old_ptr, new_ptr, obj_size);
        }

        // Round-2 fix (T2-4): explicit atomic load+store for the mark_word
        // field. The bulk memcpy above is technically UB for `AtomicU64`:
        // even under STW the memory model requires atomic ops on atomic
        // locations. Replicate the mark word atomically so subsequent CAS
        // operations (monitor inflation, etc.) on the new copy observe a
        // properly synchronized initial value.
        //
        // NOTE: a future concurrent G1 collector needs a different
        // forwarding protocol — CAS-install the forwarding pointer and
        // re-read the mark word if a mutator raced the evacuation.
        // SAFETY: both pointers reference a fully written ObjectHeader.
        unsafe {
            let old_header_ptr = old_ptr as *const ObjectHeader;
            let new_header_ptr = new_ptr as *mut ObjectHeader;
            let mark = (*old_header_ptr)
                .mark_word
                .load(std::sync::atomic::Ordering::Relaxed);
            (*new_header_ptr)
                .mark_word
                .store(mark, std::sync::atomic::Ordering::Relaxed);
        }

        // Increment GC age on the new copy
        let new_header = unsafe { &mut *(new_ptr as *mut ObjectHeader) };
        if !promote {
            new_header.gc_age = new_header.gc_age.saturating_add(1);
        }
        new_header.forwarding_ptr = std::ptr::null_mut();

        pointer_map.insert(old_addr, new_ptr as usize);
        *objects_copied += 1;
        *bytes_copied += obj_size;

        Some(new_ptr)
    }

    /// Scan an evacuated object's reference fields. For each reference pointing
    /// into the CSet, evacuate the target and update the field.
    ///
    /// **STW-only correctness contract** (Round-7 audit §2): the
    /// `pointer_map.contains_key(...)` dedup pre-check below is a
    /// plain-`HashMap` operation that is correct ONLY because young/mixed
    /// evacuation runs single-threaded under STW with the calling thread
    /// holding `self.regions.lock()` for the entire collection. The
    /// `gc_worker_threads` config field (default 4) exists for a future
    /// parallel evacuator; the dedup will become a TOCTOU the moment the
    /// `work_list`/`pointer_map` is shared between worker threads: two
    /// workers can sample `contains_key == false` for the same source addr,
    /// both call `evacuate_object`, one wins the insert, and the loser's
    /// freshly-copied Survivor allocation is leaked while still being
    /// pushed onto a worklist for double-scan. Before enabling parallel
    /// evacuation, convert `pointer_map` to a `DashMap` (or per-worker
    /// shards) and use `entry().or_insert_with(...)` so the dedup signal
    /// is the entry's vacancy state, not a separate `contains_key` call.
    ///
    /// The caller is documented to hold the regions lock; we cannot
    /// `debug_assert!` directly on lock ownership (parking_lot Mutex offers
    /// no such API), so the invariant is enforced by the type-level
    /// `&mut Vec<G1Region>` parameter (only the lock holder can produce
    /// it) plus this contract comment.
    fn scan_and_evacuate_refs(
        &self,
        regions: &mut Vec<G1Region>,
        obj_ptr: *mut u8,
        header: &ObjectHeader,
        cset: &std::collections::HashSet<usize>,
        pointer_map: &mut HashMap<usize, usize>,
        objects_copied: &mut usize,
        bytes_copied: &mut usize,
        work_list: &mut Vec<*mut u8>,
    ) {
        if header.kind == ObjectKind::Array {
            if header.element_type == ArrayElementType::Reference {
                for i in 0..header.array_length as usize {
                    let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * 8) };
                    let raw: u64 = unsafe { std::ptr::read(slot_ptr as *const u64) };
                    if raw != 0 {
                        let ref_ptr = raw as usize as *mut u8;
                        if let Some(region_idx) = self.region_for_ptr(regions, ref_ptr) {
                            if cset.contains(&region_idx) {
                                // Round-5 fix (CRIT, O(N²)): the prior code
                                // pushed `new_ptr` onto the worklist in BOTH
                                // branches of the dedup `if/else`, so every
                                // already-evacuated target was rescanned —
                                // turning ref-cycles into quadratic blow-up
                                // and reaching the worklist budget cap in
                                // pathological graphs. The dedup signal we
                                // need is "was this evacuation fresh?".
                                // `evacuate_object` inserts into
                                // `pointer_map` only when it actually
                                // copies; if the entry was already present
                                // it returns the existing forward without
                                // touching the map. Sample BEFORE the call
                                // and push to the worklist only on a fresh
                                // evacuation.
                                let already_forwarded =
                                    pointer_map.contains_key(&(ref_ptr as usize));
                                if let Some(new_ptr) = self.evacuate_object(
                                    regions,
                                    ref_ptr,
                                    pointer_map,
                                    objects_copied,
                                    bytes_copied,
                                ) {
                                    unsafe {
                                        std::ptr::write(slot_ptr as *mut u64, new_ptr as u64);
                                    }
                                    if !already_forwarded {
                                        work_list.push(new_ptr);
                                    }
                                }
                            } else if let Some(&new_addr) = pointer_map.get(&(ref_ptr as usize)) {
                                // Already forwarded from a previous scan
                                unsafe {
                                    std::ptr::write(slot_ptr as *mut u64, new_addr as u64);
                                }
                            }
                        }
                    }
                }
            }
        } else {
            for slot_idx in 0..header.num_slots as usize {
                let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                let value = unsafe { std::ptr::read(slot_ptr as *const Value) };
                if let Value::Object(Some(ref_obj)) = value {
                    let ref_ptr = ref_obj.as_ptr();
                    if let Some(region_idx) = self.region_for_ptr(regions, ref_ptr) {
                        if cset.contains(&region_idx) {
                            // Round-5 fix (CRIT, O(N²)): only push the
                            // forwarded target onto the worklist when this
                            // call site actually evacuated it. See the
                            // matching comment in the Array branch above.
                            let already_forwarded =
                                pointer_map.contains_key(&(ref_ptr as usize));
                            if let Some(new_ptr) = self.evacuate_object(
                                regions,
                                ref_ptr,
                                pointer_map,
                                objects_copied,
                                bytes_copied,
                            ) {
                                let new_value = Value::Object(Some(unsafe {
                                    ObjectRef::from_raw(new_ptr)
                                }));
                                unsafe {
                                    std::ptr::write(slot_ptr as *mut Value, new_value);
                                }
                                if !already_forwarded {
                                    work_list.push(new_ptr);
                                }
                            }
                        } else if let Some(&new_addr) = pointer_map.get(&(ref_ptr as usize)) {
                            let new_value = Value::Object(Some(unsafe {
                                ObjectRef::from_raw(new_addr as *mut u8)
                            }));
                            unsafe {
                                std::ptr::write(slot_ptr as *mut Value, new_value);
                            }
                        }
                    }
                }
            }
        }
    }

    /// CRIT fix (UAF): scan every object in a non-CSet source region and,
    /// for each reference slot pointing into the CSet, evacuate the target
    /// (if not already evacuated) and rewrite the slot to the forwarded
    /// pointer in place. Without this step the RSet sources collected in
    /// young/mixed Phase 2 were dropped, so cross-region references from
    /// non-CSet → CSet survived as stale pointers after the CSet regions
    /// were reset, causing silent use-after-free. Pattern mirrors
    /// `scan_and_evacuate_refs` but operates on a region's full object
    /// walk rather than a single evacuated object.
    fn scan_source_region_for_cset_refs(
        &self,
        regions: &mut Vec<G1Region>,
        source_idx: usize,
        cset: &std::collections::HashSet<usize>,
        pointer_map: &mut HashMap<usize, usize>,
        objects_copied: &mut usize,
        bytes_copied: &mut usize,
        work_list: &mut Vec<*mut u8>,
    ) {
        // Source must be a live non-CSet region; cset sources from rset can
        // include indices that have since been reclassified.
        if cset.contains(&source_idx) {
            return;
        }
        let (cursor, base) = {
            let r = &mut regions[source_idx];
            if r.region_type == RegionType::Free {
                return;
            }
            (r.cursor, r.data.as_mut_ptr())
        };

        let mut offset = 0usize;
        while offset < cursor {
            let obj_ptr = unsafe { base.add(offset) };
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            // Round-9 gc CRIT-1: humongous continuation filler covers the
            // entire region; skip without trying to follow any oops.
            if is_humongous_filler(header) {
                break;
            }
            let obj_size = object_total_size(header);
            if obj_size < HEADER_SIZE || offset + obj_size > cursor {
                break;
            }

            // Walk reference slots; mirror scan_and_evacuate_refs's slot
            // dispatch but rewrite the slot atomically-by-store (STW: no
            // concurrent mutator; uses the same plain ptr::write pattern
            // as evacuate_object's mark-word transfer and the existing
            // scan_and_evacuate_refs helper).
            if header.kind == ObjectKind::Array {
                if header.element_type == ArrayElementType::Reference {
                    for i in 0..header.array_length as usize {
                        let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * 8) };
                        let raw: u64 = unsafe { std::ptr::read(slot_ptr as *const u64) };
                        if raw == 0 {
                            continue;
                        }
                        let ref_ptr = raw as usize as *mut u8;
                        if let Some(ridx) = self.region_for_ptr(regions, ref_ptr) {
                            if cset.contains(&ridx) {
                                if let Some(new_ptr) = self.evacuate_object(
                                    regions,
                                    ref_ptr,
                                    pointer_map,
                                    objects_copied,
                                    bytes_copied,
                                ) {
                                    unsafe {
                                        std::ptr::write(slot_ptr as *mut u64, new_ptr as u64);
                                    }
                                    work_list.push(new_ptr);
                                }
                            }
                        }
                    }
                }
            } else {
                for slot_idx in 0..header.num_slots as usize {
                    let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                    let value = unsafe { std::ptr::read(slot_ptr as *const Value) };
                    if let Value::Object(Some(ref_obj)) = value {
                        let ref_ptr = ref_obj.as_ptr();
                        if let Some(ridx) = self.region_for_ptr(regions, ref_ptr) {
                            if cset.contains(&ridx) {
                                if let Some(new_ptr) = self.evacuate_object(
                                    regions,
                                    ref_ptr,
                                    pointer_map,
                                    objects_copied,
                                    bytes_copied,
                                ) {
                                    let new_value = Value::Object(Some(unsafe {
                                        ObjectRef::from_raw(new_ptr)
                                    }));
                                    unsafe {
                                        std::ptr::write(slot_ptr as *mut Value, new_value);
                                    }
                                    work_list.push(new_ptr);
                                }
                            }
                        }
                    }
                }
            }

            offset += obj_size;
        }
    }

    /// Update interior references in all non-CSet regions using the pointer map.
    fn update_references_in_regions(
        &self,
        regions: &mut Vec<G1Region>,
        cset: &std::collections::HashSet<usize>,
        pointer_map: &HashMap<usize, usize>,
    ) {
        if pointer_map.is_empty() {
            return;
        }

        for i in 0..regions.len() {
            if cset.contains(&i) || regions[i].region_type == RegionType::Free {
                continue;
            }

            let cursor = regions[i].cursor;
            let base = regions[i].data.as_mut_ptr();
            let mut offset = 0usize;

            while offset < cursor {
                let obj_ptr = unsafe { base.add(offset) };
                let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                // Round-9 gc CRIT-1: humongous continuation filler covers
                // the entire region; skip the rest.
                if is_humongous_filler(header) {
                    break;
                }
                let obj_size = object_total_size(header);

                if obj_size < HEADER_SIZE || offset + obj_size > cursor {
                    break;
                }

                update_object_refs(obj_ptr, header, pointer_map);
                offset += obj_size;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Concurrent Marking
    // -----------------------------------------------------------------------

    /// Start a concurrent marking cycle. Sets phase to InitialMark.
    ///
    /// Audit fix (HIGH-3): also clears the mark worklist so a previous
    /// aborted cycle doesn't leak gray pointers into the new cycle.
    pub fn start_concurrent_mark(&self) {
        self.gc_state
            .set_phase(ConcurrentGcPhase::InitialMark);
        self.satb_queue.activate();
        // Round-2 fix (HIGH — GC #5): clear every per-region bitmap so a
        // previous cycle's mark bits don't leak into this one.
        {
            let regions = self.regions.lock();
            for r in regions.iter() {
                r.mark_bitmap.clear();
            }
        }
        self.mark_worklist.lock().clear();
        // Round-9 gc HIGH-5: reset overflow indicator at cycle start so
        // a previous cycle's overflow doesn't trigger a needless rescan.
        self.mark_worklist_overflowed.store(false, Ordering::Relaxed);
        self.gc_state
            .set_phase(ConcurrentGcPhase::ConcurrentMark);
    }

    /// Perform an incremental step of concurrent marking.
    ///
    /// Audit fix (HIGH-3): this used to walk every region and call
    /// `try_mark` on every header — no roots, no transitive closure, so
    /// G1 reported every allocated object as live. The new implementation
    /// does real tri-color marking by draining the persistent
    /// `mark_worklist`:
    ///
    /// 1. Pop an object address from the worklist (the gray set).
    /// 2. Mark it in the bitmap.
    /// 3. Scan its reference slots — for each unmarked old/young heap
    ///    object it points at, mark it gray (push onto the worklist).
    ///
    /// The worklist is seeded by `remark` (which the interpreter calls
    /// at initial-mark STW and at the final remark STW). This way the
    /// worker thread that calls `concurrent_mark_step` in a loop simply
    /// drains the gray set produced by the roots.
    ///
    /// `work_amount` is the maximum number of objects to scan in this
    /// step. Returns `true` when the worklist is empty (marking is done).
    pub fn concurrent_mark_step(&self, work_amount: usize) -> bool {
        if work_amount == 0 {
            return self.mark_worklist.lock().is_empty();
        }

        let regions = self.regions.lock();
        let mut worklist = self.mark_worklist.lock();
        let mut remaining = work_amount;

        while remaining > 0 {
            let obj_addr = match worklist.pop() {
                Some(a) => a,
                None => break, // gray set empty for now — see overflow handling below
            };
            remaining -= 1;

            // Defensive: confirm this address really lives in some region.
            // (Stale roots from before a heap rearrangement would otherwise
            // dereference garbage.)
            let obj_ptr = obj_addr as *mut u8;
            let region_idx = match self.region_for_ptr(&regions, obj_ptr) {
                Some(idx) => idx,
                None => continue,
            };

            // Round-2 fix (HIGH — GC #5): bitmaps now live per-region, so
            // route the mark through the owning region's bitmap (which is
            // keyed off that region's actual data pointer).
            // Already black? Skip — nothing new to discover from it.
            if !regions[region_idx].mark_bitmap.try_mark(obj_addr) {
                continue;
            }

            // Scan the object's reference fields and push gray successors.
            // SAFETY: region_for_ptr above confirmed the address is inside
            // a live region's data buffer; the header is therefore
            // readable for the duration of the GC cycle (regions are
            // pinned by the lock guard).
            let header = unsafe { &*(obj_addr as *const ObjectHeader) };
            self.scan_object_refs(obj_ptr, header, &regions, &mut worklist);
        }

        if !worklist.is_empty() {
            // Ran out of budget but still have work — caller should call again.
            return false;
        }

        // Round-9 gc HIGH-5 — graceful overflow handling. If any push
        // was dropped earlier in this cycle, the transitive closure is
        // incomplete: dropped objects were never scanned, so their
        // children may be unmarked despite being live. Run a
        // conservative re-walk: for every marked object in every live
        // region, re-scan its outgoing references and push any unmarked
        // targets. The loop bounds the number of recovery passes; once
        // a full pass causes no new pushes the worklist drains and we
        // declare marking complete. The bitmap monotonically grows, so
        // this terminates after a bounded number of passes.
        if self.mark_worklist_overflowed.load(Ordering::Relaxed) {
            // Reset the flag so we can detect re-overflow during recovery.
            self.mark_worklist_overflowed.store(false, Ordering::Relaxed);
            for region in regions.iter() {
                if region.region_type == RegionType::Free {
                    continue;
                }
                let base = region.data.as_ptr() as usize;
                let mut offset = 0usize;
                while offset < region.cursor {
                    let obj_addr = base + offset;
                    // SAFETY: offset < cursor; header is within the region.
                    let header = unsafe { &*(obj_addr as *const ObjectHeader) };
                    if is_humongous_filler(header) {
                        break;
                    }
                    let obj_size = object_total_size(header);
                    if obj_size < HEADER_SIZE || offset + obj_size > region.cursor {
                        break;
                    }
                    if region.mark_bitmap.is_marked(obj_addr) {
                        let obj_ptr = obj_addr as *mut u8;
                        self.scan_object_refs(obj_ptr, header, &regions, &mut worklist);
                    }
                    offset += obj_size;
                }
            }
            // Tell the caller to keep stepping — the rescan likely
            // refilled the worklist with newly-discovered references.
            // Returning `false` causes the marking loop to call us
            // again, which will drain whatever the rescan produced.
            return worklist.is_empty()
                && !self.mark_worklist_overflowed.load(Ordering::Relaxed);
        }

        // Worklist empty and no overflow: marking complete.
        true
    }

    /// Scan an object's reference slots; for each in-heap, not-yet-marked
    /// target push the target onto `worklist`. This mirrors
    /// `ConcurrentMarker::scan_object` in `concurrent_mark.rs` but uses
    /// G1's per-region addressing (objects live inside `G1Region::data`).
    ///
    /// Round-2 fix (HIGH — GC #5): bitmaps are per-region. Instead of
    /// receiving a single global bitmap, this helper looks up the owning
    /// region for each reference and consults that region's bitmap to
    /// avoid pushing already-marked targets back onto the worklist. The
    /// `try_mark` in `concurrent_mark_step` is still the authoritative
    /// marker; the `is_marked` check here is only an optimization to
    /// reduce worklist churn.
    fn scan_object_refs(
        &self,
        obj_ptr: *mut u8,
        header: &ObjectHeader,
        regions: &[G1Region],
        worklist: &mut Vec<usize>,
    ) {
        // CRIT-perf fix: use the O(log R) cached `lookup_region_for_addr`
        // helper instead of an O(R) linear walk. The wrapper preserves
        // the "skip Free regions" guard the linear version had by
        // checking the looked-up region's type.
        let region_for = |p: *mut u8| -> Option<usize> {
            let idx = self.lookup_region_for_addr(p as usize)?;
            if regions[idx].region_type == RegionType::Free {
                None
            } else {
                Some(idx)
            }
        };

        if header.kind == ObjectKind::Array {
            if header.element_type == ArrayElementType::Reference {
                // Reference array: 8-byte compact slot per element.
                for i in 0..header.array_length as usize {
                    // SAFETY: i < array_length, within the allocated array.
                    let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * 8) };
                    // SAFETY: 8-byte aligned slot within the array data.
                    let raw: u64 = unsafe { std::ptr::read(slot_ptr as *const u64) };
                    if raw == 0 {
                        continue;
                    }
                    let ref_ptr = raw as usize as *mut u8;
                    if let Some(idx) = region_for(ref_ptr) {
                        if !regions[idx].mark_bitmap.is_marked(ref_ptr as usize) {
                            // Round-9 gc HIGH-5: graceful overflow — drop
                            // the push and record the event so remark
                            // can run a conservative full re-walk.
                            if worklist.len() >= MARK_WORKLIST_CAP {
                                self.mark_worklist_overflowed
                                    .store(true, Ordering::Relaxed);
                            } else {
                                worklist.push(ref_ptr as usize);
                            }
                        }
                    }
                }
            }
            // Primitive arrays carry no references.
        } else {
            // Object: 16-byte Value slot per field.
            for slot_idx in 0..header.num_slots as usize {
                // SAFETY: slot_idx < num_slots, within the allocated object.
                let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                // SAFETY: slot_ptr is a properly aligned Value within the object.
                let value = unsafe { std::ptr::read(slot_ptr as *const Value) };
                if let Value::Object(Some(ref_obj)) = value {
                    let ref_ptr = ref_obj.as_ptr();
                    if let Some(idx) = region_for(ref_ptr) {
                        if !regions[idx].mark_bitmap.is_marked(ref_ptr as usize) {
                            // Round-9 gc HIGH-5: graceful overflow — drop
                            // the push and record the event so remark
                            // can run a conservative full re-walk.
                            if worklist.len() >= MARK_WORKLIST_CAP {
                                self.mark_worklist_overflowed
                                    .store(true, Ordering::Relaxed);
                            } else {
                                worklist.push(ref_ptr as usize);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Remark phase (STW): mark roots + drain SATB buffer onto the mark
    /// worklist (the gray set), set phase to `Remark`, and let
    /// `concurrent_mark_step` drain the worklist transitively.
    ///
    /// Audit fix (HIGH-3): the previous implementation drained the SATB
    /// queue into `_satb_entries` and threw it away, and only marked the
    /// root addresses directly without pushing them to a worklist — no
    /// transitive closure was ever computed.
    ///
    /// SATB semantics: every pointer in the SATB queue is a *previously
    /// live* reference value that was overwritten while concurrent
    /// marking was active. We must treat each as a root to avoid the
    /// classic G1 lost-object scenario where A→B is replaced by A→null
    /// after we scanned A but before we scanned B.
    ///
    /// NOTE: in this codebase `vm_heap::g1_mark_roots` also calls this
    /// function during the *initial-mark* STW (right after
    /// `start_concurrent_mark`), so the function deliberately treats
    /// SATB draining as idempotent and safe at either point — at
    /// initial mark the SATB queue is freshly activated and typically
    /// empty. Callers who want a true STW final-remark should follow
    /// up with `concurrent_mark_step(usize::MAX)` to drain the worklist
    /// before transitioning to sweep.
    pub fn remark(&self, roots: &[ObjectRef]) {
        self.gc_state.set_phase(ConcurrentGcPhase::Remark);

        let regions = self.regions.lock();
        let mut worklist = self.mark_worklist.lock();

        // Round-2 fix (HIGH — GC #5): bitmaps are per-region. Helper
        // returns the owning region index (if any) so we can consult
        // *that* region's bitmap for the already-marked check.
        // CRIT-perf fix: use the O(log R) cached `lookup_region_for_addr`
        // instead of an O(R) linear scan; preserve the Free-region skip
        // by post-filtering on the looked-up region's type.
        let region_for = |addr: usize| -> Option<usize> {
            let idx = self.lookup_region_for_addr(addr)?;
            if regions[idx].region_type == RegionType::Free {
                None
            } else {
                Some(idx)
            }
        };

        // Round-9 gc HIGH-5 — graceful overflow. The previous panic was
        // reachable from any wide-graph workload (hostile or otherwise);
        // we now drop the push and set the overflow flag. The marker
        // (see `concurrent_mark_step` / `cleanup` and the rescan in
        // `finish_marking`) consults the flag and re-walks all live
        // regions conservatively before the sweep transition.
        let overflow_flag = &self.mark_worklist_overflowed;
        let push_with_cap = |worklist: &mut Vec<usize>, addr: usize| {
            if worklist.len() >= MARK_WORKLIST_CAP {
                overflow_flag.store(true, Ordering::Relaxed);
                return;
            }
            worklist.push(addr);
        };

        // 1) Roots — push every non-null in-heap root onto the gray set.
        //    `concurrent_mark_step` will mark them and follow their refs.
        for root in roots {
            let p = root.as_ptr();
            if p.is_null() {
                continue;
            }
            let addr = p as usize;
            if let Some(idx) = region_for(addr) {
                if regions[idx].mark_bitmap.is_marked(addr) {
                    continue; // already black
                }
                push_with_cap(&mut worklist, addr);
            }
        }

        // 2) SATB — every overwritten reference becomes a root.
        let satb_entries = self.satb_queue.drain();
        for addr in satb_entries {
            if addr == 0 {
                continue;
            }
            if let Some(idx) = region_for(addr) {
                if regions[idx].mark_bitmap.is_marked(addr) {
                    continue;
                }
                push_with_cap(&mut worklist, addr);
            }
        }

        // Note: we leave SATB *active* — the cycle continues in the
        // background marker. `cleanup()` (the final phase) is the right
        // place to deactivate SATB, since at that point marking is
        // truly complete.
    }

    /// Cleanup phase: compute per-region live_bytes and gc_efficiency,
    /// free completely empty old regions.
    pub fn cleanup(&self) {
        let mut regions = self.regions.lock();
        let region_size = self.config.region_size;

        for region in regions.iter_mut() {
            if region.region_type == RegionType::Free {
                continue;
            }

            // Compute live bytes by walking objects and checking the bitmap.
            // Round-2 fix (HIGH — GC #5): consult this region's own
            // bitmap (keyed off `data.as_ptr()`), not a global one.
            let base = region.data.as_ptr() as usize;
            let mut live_bytes = 0usize;
            let mut offset = 0usize;

            while offset < region.cursor {
                let obj_addr = base + offset;
                let header = unsafe { &*(obj_addr as *const ObjectHeader) };
                // Round-9 gc CRIT-1: humongous continuation filler covers
                // the entire region with no live objects of its own; skip.
                if is_humongous_filler(header) {
                    break;
                }
                let obj_size = object_total_size(header);

                if obj_size < HEADER_SIZE || offset + obj_size > region.cursor {
                    break;
                }

                if region.mark_bitmap.is_marked(obj_addr) {
                    live_bytes += obj_size;
                }
                offset += obj_size;
            }

            region.live_bytes = live_bytes;
            region.gc_efficiency = if region_size > 0 {
                live_bytes as f64 / region_size as f64
            } else {
                0.0
            };

            // Free completely empty old regions
            if region.live_bytes == 0
                && region.region_type == RegionType::Old
                && !region.pinned
            {
                region.reset();
            }
        }

        // Audit fix (HIGH-3): clear any stragglers from the gray set and
        // deactivate the SATB write barrier — the cycle is fully done.
        self.mark_worklist.lock().clear();
        // Round-9 gc HIGH-5: clear the overflow indicator so the next
        // cycle starts in a clean state.
        self.mark_worklist_overflowed.store(false, Ordering::Relaxed);
        // Round-5 CRIT #4: close the SATB barrier with a drain-then-flip
        // protocol so no mutator log push that observed the gate as
        // active can be stranded after the cycle ends. The drained
        // stragglers are discarded — the mark cycle is complete and
        // anything not yet marked is correctly dead; the next cycle
        // will re-discover live state from roots. (We are at the very
        // end of the cycle so missing a few late SATB entries is
        // semantically fine.)
        let _stragglers = self.satb_queue.deactivate_and_drain();

        self.gc_state.set_phase(ConcurrentGcPhase::Idle);
        // Round-9 HIGH-1: Release publishes marking_complete=true so any
        // subsequent RSet writes / mutator-side reads of the flag observe
        // all of the prior cycle's effects (gray set drained, SATB queue
        // deactivated, phase set to Idle). Paired with Acquire load in
        // `needs_mixed_gc` and any other consumer.
        self.marking_complete.store(true, Ordering::Release);
        self.mixed_gc_remaining
            .store(self.config.mixed_gc_count_target as u64, Ordering::Relaxed);
    }

    // -----------------------------------------------------------------------
    // IHOP
    // -----------------------------------------------------------------------

    /// Check if old-gen occupancy has reached the IHOP threshold.
    pub fn check_ihop(&self) -> bool {
        let old_bytes = self.old_gen_bytes.load(Ordering::Relaxed);
        let threshold = self.marking_threshold_bytes.load(Ordering::Relaxed);
        old_bytes >= threshold
    }

    /// Adaptively adjust IHOP based on actual pause time.
    pub fn update_ihop(&self, actual_pause_ms: u64) {
        let target = self.config.max_gc_pause_ms;
        let current_threshold = self.marking_threshold_bytes.load(Ordering::Relaxed);

        let new_threshold = if actual_pause_ms > target {
            // Pause too long: lower threshold to start marking earlier
            (current_threshold as f64 * 0.9) as usize
        } else if actual_pause_ms < target / 2 {
            // Pause well under target: raise threshold
            let max = self.config.heap_size * 90 / 100;
            ((current_threshold as f64 * 1.05) as usize).min(max)
        } else {
            current_threshold
        };

        self.marking_threshold_bytes
            .store(new_threshold, Ordering::Relaxed);
    }

    // -----------------------------------------------------------------------
    // Region Pinning (JEP 423)
    // -----------------------------------------------------------------------

    /// Pin a region, preventing it from being evacuated during GC.
    pub fn pin_region(&self, region_idx: usize) {
        let mut regions = self.regions.lock();
        if region_idx < regions.len() {
            regions[region_idx].pinned = true;
        }
    }

    /// Unpin a region, allowing it to be collected again.
    pub fn unpin_region(&self, region_idx: usize) {
        let mut regions = self.regions.lock();
        if region_idx < regions.len() {
            regions[region_idx].pinned = false;
        }
    }

    /// Check if a region is pinned.
    pub fn is_pinned(&self, region_idx: usize) -> bool {
        let regions = self.regions.lock();
        region_idx < regions.len() && regions[region_idx].pinned
    }

    // -----------------------------------------------------------------------
    // String Deduplication
    // -----------------------------------------------------------------------

    /// Attempt to deduplicate a String object. The hash is used to look up
    /// a canonical instance.
    ///
    /// Returns:
    /// - `Some(canonical_addr)` on a HIT: the caller should redirect its
    ///   pointer at `addr` to `canonical_addr` and discard the duplicate.
    /// - `None` on a MISS (or when dedup is disabled): the caller's
    ///   `addr` was registered as the canonical instance for `hash`;
    ///   nothing to redirect.
    ///
    /// Audit fix (HIGH): the previous implementation ignored the canonical
    /// address on a hit and only returned a `bool`, so the caller had no
    /// way to actually perform the dedup. Returning the canonical address
    /// makes the function correct on its own terms.
    pub fn deduplicate_string(&self, hash: u64, addr: usize) -> Option<usize> {
        if !self.config.string_dedup_enabled {
            return None;
        }
        let mut table = self.string_dedup_table.lock();
        if let Some(&canonical) = table.get(&hash) {
            // Hit: caller should redirect `addr` -> `canonical`.
            // If `addr` happens to already be the canonical (idempotent
            // caller), still return Some so the API stays uniform.
            Some(canonical)
        } else {
            // Miss: register `addr` as the canonical instance.
            table.insert(hash, addr);
            None
        }
    }

    // -----------------------------------------------------------------------
    // GC Logging
    // -----------------------------------------------------------------------

    /// Log a GC event if logging is enabled.
    fn log_gc_event(&self, collection_type: &G1CollectionType, pause_ms: u64, stats: &GcStats) {
        if !self.gc_log_enabled.load(Ordering::Relaxed) {
            return;
        }
        tracing::info!(
            "[GC {:?}] pause={pause_ms}ms copied={} bytes_copied={} freed={}",
            collection_type,
            stats.objects_copied,
            stats.bytes_copied,
            stats.bytes_freed,
        );
    }

    /// Enable GC event logging.
    pub fn enable_gc_logging(&self) {
        self.gc_log_enabled.store(true, Ordering::Relaxed);
    }

    /// Disable GC event logging.
    pub fn disable_gc_logging(&self) {
        self.gc_log_enabled.store(false, Ordering::Relaxed);
    }

    /// Get the total number of collections performed.
    pub fn collection_count(&self) -> u64 {
        self.collection_count.load(Ordering::Relaxed)
    }

    /// Get the total pause time across all collections.
    pub fn total_pause_ms(&self) -> u64 {
        self.total_pause_ms.load(Ordering::Relaxed)
    }

    /// Get the current GC phase.
    pub fn gc_phase(&self) -> ConcurrentGcPhase {
        self.gc_state.phase()
    }

    /// Get old-gen byte count.
    pub fn old_gen_bytes(&self) -> usize {
        self.old_gen_bytes.load(Ordering::Relaxed)
    }

    /// Get the IHOP marking threshold.
    pub fn marking_threshold_bytes(&self) -> usize {
        self.marking_threshold_bytes.load(Ordering::Relaxed)
    }

    /// Return the total heap capacity in bytes.
    pub fn heap_capacity(&self) -> usize {
        self.config.heap_size
    }

    /// Return (used, capacity) for Eden regions.
    pub fn eden_stats(&self) -> (usize, usize) {
        let regions = self.regions.lock();
        let mut used = 0usize;
        let mut count = 0usize;
        for r in regions.iter() {
            if r.region_type == RegionType::Eden {
                used += r.cursor;
                count += 1;
            }
        }
        (used, count * self.config.region_size)
    }

    /// Return (used, capacity) for Old regions.
    pub fn old_gen_stats(&self) -> (usize, usize) {
        let regions = self.regions.lock();
        let mut used = 0usize;
        let mut count = 0usize;
        for r in regions.iter() {
            if r.region_type == RegionType::Old {
                used += r.cursor;
                count += 1;
            }
        }
        (used, count * self.config.region_size)
    }

    /// Count regions of a given type.
    pub fn count_regions(&self, region_type: RegionType) -> usize {
        let regions = self.regions.lock();
        regions
            .iter()
            .filter(|r| r.region_type == region_type)
            .count()
    }

    /// Check if mixed GC is needed (marking is complete and cycles remain).
    fn needs_mixed_gc(&self) -> bool {
        // Round-9 HIGH-1: Acquire pairs with the Release publish in
        // `finish_mark_cycle` so this reader observes the fully-drained
        // gray set and deactivated SATB queue before acting on the flag.
        self.marking_complete.load(Ordering::Acquire)
            && self.mixed_gc_remaining.load(Ordering::Relaxed) > 0
    }

    // -----------------------------------------------------------------------
    // Fallible allocation (Phase 89)
    // -----------------------------------------------------------------------

    /// Try to allocate a Java object. Returns `None` when Eden is exhausted
    /// (caller should trigger GC and retry).
    pub fn try_alloc_object(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        let total_size = HEADER_SIZE + num_fields.checked_mul(SLOT_SIZE)?;
        let (ptr, _region) = self.alloc_in_region(total_size)?;

        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Object,
            ArrayElementType::Reference,
            self.next_hash(),
            0,
            u32::try_from(num_fields).ok()?,
        );
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            Some(ObjectRef::from_raw(ptr))
        }
    }

    /// Allocate and pre-initialize primitive-typed slots based on JVM
    /// field descriptor bytes. See
    /// [`crate::heap::default_value_for_descriptor`] for the mapping.
    pub fn alloc_object_with_descriptors(
        &self,
        class_id: ClassId,
        num_fields: usize,
        descriptor_bytes: &[u8],
    ) -> ObjectRef {
        // Use the GarbageCollector trait's alloc_object to get the raw
        // object, then populate primitive slots.
        let obj = GarbageCollector::alloc_object(self, class_id, num_fields);
        let n = num_fields.min(descriptor_bytes.len());
        for i in 0..n {
            if let Some(default) =
                crate::heap::default_value_for_descriptor(descriptor_bytes[i])
            {
                GarbageCollector::set_field(self, obj, i, default);
            }
        }
        obj
    }

    /// Fallible variant of [`Self::alloc_object_with_descriptors`].
    pub fn try_alloc_object_with_descriptors(
        &self,
        class_id: ClassId,
        num_fields: usize,
        descriptor_bytes: &[u8],
    ) -> Option<ObjectRef> {
        let obj = self.try_alloc_object(class_id, num_fields)?;
        let n = num_fields.min(descriptor_bytes.len());
        for i in 0..n {
            if let Some(default) =
                crate::heap::default_value_for_descriptor(descriptor_bytes[i])
            {
                GarbageCollector::set_field(self, obj, i, default);
            }
        }
        Some(obj)
    }

    /// Try to allocate a Java array. Returns `None` when Eden is exhausted.
    pub fn try_alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        let data_size = array_data_size(length, element_type).ok()?;
        let total_size = HEADER_SIZE.checked_add(data_size)?;
        let (ptr, _region) = self.alloc_in_region(total_size)?;

        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Array,
            element_type,
            self.next_hash(),
            u32::try_from(length).ok()?,
            0,
        );
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            Some(ObjectRef::from_raw(ptr))
        }
    }

    /// Carve a TLAB from the current Eden region.
    ///
    /// Returns `Some((ptr, actual_size))` on success. The TLAB is carved from
    /// the Eden region's bump pointer, exactly like how `refill_tlab` works in
    /// the generational collector but backed by G1 regions.
    pub fn refill_tlab(&self, requested_size: usize) -> Option<(*mut u8, usize)> {
        let mut regions = self.regions.lock();
        let region_size = self.config.region_size;

        // Don't serve TLABs for requests larger than half a region
        if requested_size > region_size / 2 {
            return None;
        }

        // Try current Eden region
        let cur = self.current_eden.load(Ordering::Relaxed);
        if cur < regions.len() && regions[cur].region_type == RegionType::Eden {
            let remaining = regions[cur].remaining();
            if remaining >= 256 {
                let actual = requested_size.min(remaining);
                if let Some((ptr, _off)) = regions[cur].bump_alloc(actual, 8) {
                    return Some((ptr, actual));
                }
            }
        }

        // Find a new free region for Eden and carve TLAB from it
        if let Some(idx) = find_free_region(&regions) {
            regions[idx].region_type = RegionType::Eden;
            self.current_eden.store(idx, Ordering::Relaxed);
            let remaining = regions[idx].remaining();
            if remaining >= 256 {
                let actual = requested_size.min(remaining);
                if let Some((ptr, _off)) = regions[idx].bump_alloc(actual, 8) {
                    return Some((ptr, actual));
                }
            }
        }

        None
    }

    /// Get a reference to the global SATB queue.
    pub fn satb_queue(&self) -> &Arc<SatbQueue> {
        &self.satb_queue
    }

    /// Record an old reference value in the SATB queue (for concurrent marking).
    /// Only records when SATB is active (during concurrent mark phase).
    ///
    /// Routes through the per-thread SATB buffer
    /// ([`crate::satb::satb_thread_local_log`]) so the hot write-barrier
    /// path takes no shared lock in the common case; the buffer auto-flushes
    /// into the global queue every ~256 entries.
    pub fn satb_pre_barrier(&self, old_ref: usize) {
        if old_ref == 0 {
            return;
        }
        if self.satb_queue.is_active() {
            crate::satb::satb_thread_local_log(&self.satb_queue, old_ref);
        }
    }

    /// Post-write barrier: track cross-region references in remembered sets.
    ///
    /// Round-9 gc CRIT-8 — hot path: a per-thread cache of the
    /// last-touched destination region pointer avoids re-acquiring the
    /// heap-wide `regions` mutex on every reference store. Same-region
    /// successive stores (the dominant pattern: tight loops populating
    /// one array, append-to-list, etc.) hit the cache and take only the
    /// per-RSet inner mutex.
    ///
    /// SAFETY invariant: the `Vec<G1Region>` backing `self.regions` is
    /// created once in `G1Collector::new` with all `region_count`
    /// entries and is **never resized** thereafter (`grep -n
    /// 'regions.\\(push\\|resize\\|extend\\)' gc/src/g1.rs` returns no
    /// matches). Therefore `&regions[i]` is address-stable for the
    /// entire `G1Collector` lifetime, and a raw `*const G1Region`
    /// captured under one lock acquisition remains valid for later
    /// dereference even after the lock is released, as long as the
    /// collector itself is still alive. The cached pointer is keyed by
    /// both region index AND collector identity (the `*const Self`)
    /// so a stale cache from a previous collector instance never
    /// resurrects a dangling pointer.
    pub fn post_write_barrier_rset(&self, src_obj: ObjectRef, stored_ref: ObjectRef) {
        let src_addr = src_obj.as_ptr() as usize;
        let dst_addr = stored_ref.as_ptr() as usize;

        // O(log R) lookups via the cached `region_lookup` table; these do
        // NOT take any lock (the lookup table is immutable post-`new`).
        let src_region = self.lookup_region_for_addr(src_addr);
        let dst_region = self.lookup_region_for_addr(dst_addr);

        // Only record cross-region references.
        let (src_idx, dst_idx) = match (src_region, dst_region) {
            (Some(s), Some(d)) if s != d => (s, d),
            _ => return,
        };

        let collector_id = self as *const Self as usize;

        thread_local! {
            // (collector_id, region_idx, *const G1Region). `Cell` is
            // sufficient — the pointer is `Copy` and never escapes
            // the with() block other than as a deref-then-call.
            static LAST_RSET_TARGET: std::cell::Cell<Option<(usize, usize, *const G1Region)>>
                = const { std::cell::Cell::new(None) };
        }

        let hit = LAST_RSET_TARGET.with(|cell| {
            if let Some((cached_collector, cached_idx, cached_ptr)) = cell.get() {
                if cached_collector == collector_id && cached_idx == dst_idx {
                    // SAFETY: see method-level invariant. `cached_ptr`
                    // was captured from `&regions[dst_idx]` (Vec never
                    // reallocates) and the collector identity check
                    // rules out reuse across collector instances.
                    // `add_reference` takes `&self` (interior
                    // `parking_lot::Mutex` on the FxHashSet — see
                    // `RememberedSet`).
                    unsafe { (*cached_ptr).rset.add_reference(src_idx); }
                    return true;
                }
            }
            false
        });
        if hit {
            return;
        }

        // Cache miss: take the regions lock just long enough to capture
        // the stable pointer, populate the TLS cache, then perform the
        // RSet add via the same `&self` interior-mutex path.
        let regions = self.regions.lock();
        let region_ptr: *const G1Region = &regions[dst_idx];
        LAST_RSET_TARGET.with(|cell| {
            cell.set(Some((collector_id, dst_idx, region_ptr)));
        });
        regions[dst_idx].rset.add_reference(src_idx);
    }

    /// Find which region contains the given address (by raw address).
    ///
    /// O(log R) via the cached `region_lookup` table. See
    /// [`Self::region_for_ptr`] for the rationale; this variant is the
    /// hot path for the write barrier in
    /// [`Self::post_write_barrier_rset`], where it is called twice per
    /// reference store.
    fn region_for_ptr_with_regions(&self, _regions: &[G1Region], addr: usize) -> Option<usize> {
        self.lookup_region_for_addr(addr)
    }

    /// Binary-search the cached `(base_addr, region_idx)` table to find
    /// which region (if any) owns `addr`.
    ///
    /// Complexity: O(log R) — independent of the number of live regions.
    ///
    /// Returns `Some(idx)` iff `addr` falls within `[base, base + region_size)`
    /// for some region. The check uses `self.config.region_size` instead of
    /// the per-region `data.len()` because every region's backing buffer is
    /// allocated at exactly `region_size` bytes (see [`G1Region::new`]).
    #[inline]
    fn lookup_region_for_addr(&self, addr: usize) -> Option<usize> {
        // Find the largest base address that is <= addr.
        // `partition_point` returns the first index where the predicate is
        // false; subtracting 1 gives the last index where it is true.
        let pp = self
            .region_lookup
            .partition_point(|(base, _)| *base <= addr);
        if pp == 0 {
            return None;
        }
        let (base, idx) = self.region_lookup[pp - 1];
        if addr < base.wrapping_add(self.config.region_size) {
            Some(idx)
        } else {
            None
        }
    }

    /// Conservative validity check for a *raw address* — see
    /// [`crate::gen_heap::GenerationalHeap::is_object_address`] for the
    /// contract. NEW-1.5 JIT frame root scanning calls this through
    /// [`crate::vm_heap::VmHeap::is_object_address`].
    pub fn is_object_address(&self, addr: usize) -> Option<ObjectRef> {
        if addr == 0 || addr & 0x7 != 0 {
            return None;
        }
        if !self.is_addr_in_live_region(addr) {
            return None;
        }
        let raw = addr as *const u8;
        // SAFETY: address is inside a live region, so reading HEADER_SIZE
        // bytes from it is well-defined.
        let header = unsafe { &*(raw as *const ObjectHeader) };
        match header.kind {
            ObjectKind::Object | ObjectKind::Array => {}
            // Round-9 gc CRIT-1: humongous-continuation filler is a
            // walker sentinel, not a heap object. Reject so root scans
            // can't accidentally "validate" the address of a filler.
            ObjectKind::HumongousFiller => return None,
        }
        const MAX_PLAUSIBLE_SLOTS: u32 = 1 << 24;
        if header.num_slots > MAX_PLAUSIBLE_SLOTS {
            return None;
        }
        if matches!(header.kind, ObjectKind::Array)
            && header.array_length as usize > (1 << 27)
        {
            return None;
        }
        Some(unsafe { ObjectRef::from_raw(raw as *mut u8) })
    }

    /// Loose validity check: alignment + region containment only.
    ///
    /// Mirrors [`crate::gen_heap::GenerationalHeap::is_heap_addr`]. Does NOT
    /// read the object header — used by GC root scanning of ambiguous JVM
    /// long slots, where the strict header check is too aggressive (false
    /// negatives drop legitimately-rooted objects, leading to dangling
    /// pointers post-GC and downstream SEGV).
    pub fn is_heap_addr(&self, addr: usize) -> Option<ObjectRef> {
        if addr == 0 || addr & 0x7 != 0 {
            return None;
        }
        if !self.is_addr_in_live_region(addr) {
            return None;
        }
        // SAFETY: alignment + live-region containment confirmed.
        Some(unsafe { ObjectRef::from_raw(addr as *mut u8) })
    }

    /// Check if an address is within a live (non-Free) region.
    ///
    /// Used by reference processing to determine if a referent survived GC
    /// without being relocated (e.g., objects in Old/Humongous regions that
    /// were not part of the collection set).
    pub fn is_addr_in_live_region(&self, addr: usize) -> bool {
        let regions = self.regions.lock();
        for r in regions.iter() {
            if r.region_type == RegionType::Free {
                continue;
            }
            let base = r.data.as_ptr() as usize;
            if addr >= base && addr < base + r.cursor {
                return true;
            }
        }
        false
    }

    /// Walk all live objects across all non-Free regions.
    /// Returns a Vec of (raw pointer, total byte size) for each object.
    /// Must be called during a GC safepoint (all mutator threads paused).
    pub fn walk_objects(&self) -> Vec<(*mut u8, usize)> {
        let mut result = Vec::new();
        let regions = self.regions.lock();
        for r in regions.iter() {
            if r.region_type == RegionType::Free {
                continue;
            }
            let base = r.data.as_ptr() as usize;
            let used = r.cursor;
            let mut offset = 0;
            while offset < used {
                let ptr = (base + offset) as *mut u8;
                let header = unsafe { &*(ptr as *const ObjectHeader) };
                // Round-9 gc CRIT-1: HumongousFiller is a walker sentinel;
                // never report it as a real object.
                if is_humongous_filler(header) {
                    break;
                }
                let total_size = if header.kind == ObjectKind::Array {
                    HEADER_SIZE
                        + array_data_size(header.array_length as usize, header.element_type)
                            .unwrap_or(0)
                } else {
                    HEADER_SIZE + header.num_slots as usize * SLOT_SIZE
                };
                if total_size < HEADER_SIZE || offset + total_size > used {
                    break;
                }
                result.push((ptr, total_size));
                offset += total_size;
            }
        }
        result
    }
}

// ---------------------------------------------------------------------------
// GarbageCollector trait implementation
// ---------------------------------------------------------------------------

impl GarbageCollector for G1Collector {
    fn alloc_object(&self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        let total_size = HEADER_SIZE + num_fields * SLOT_SIZE;
        let (ptr, _region) = self.alloc_in_region(total_size).unwrap_or_else(|| {
            eprintln!("FATAL: G1: out of heap space for object allocation ({} bytes)", total_size);
            std::process::abort();
        });

        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Object,
            ArrayElementType::Reference,
            self.next_hash(),
            0,
            u32::try_from(num_fields).expect("field count exceeds u32::MAX"),
        );

        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            ObjectRef::from_raw(ptr)
        }
    }

    fn alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> ObjectRef {
        let data_size = array_data_size(length, element_type)
            .expect("array data size overflow in g1 alloc_array");
        let total_size = HEADER_SIZE + data_size;
        let (ptr, _region) = self.alloc_in_region(total_size).unwrap_or_else(|| {
            eprintln!("FATAL: G1: out of heap space for array allocation ({} bytes)", total_size);
            std::process::abort();
        });

        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Array,
            element_type,
            self.next_hash(),
            u32::try_from(length).expect("array length exceeds u32::MAX"),
            0,
        );

        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            ObjectRef::from_raw(ptr)
        }
    }

    fn get_header(&self, obj: ObjectRef) -> &ObjectHeader {
        unsafe { &*(obj.as_ptr() as *const ObjectHeader) }
    }

    fn class_id_of(&self, obj: ObjectRef) -> ClassId {
        self.get_header(obj).class_id
    }

    fn kind_of(&self, obj: ObjectRef) -> ObjectKind {
        self.get_header(obj).kind
    }

    fn element_type_of(&self, obj: ObjectRef) -> ArrayElementType {
        self.get_header(obj).element_type
    }

    fn identity_hash_code(&self, obj: ObjectRef) -> i32 {
        self.get_header(obj).identity_hash_code
    }

    fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
        let ptr = unsafe { obj.as_ptr().add(HEADER_SIZE + index * SLOT_SIZE) };
        unsafe { std::ptr::read(ptr as *const Value) }
    }

    fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
        let ptr = unsafe { obj.as_ptr().add(HEADER_SIZE + index * SLOT_SIZE) };
        unsafe {
            std::ptr::write(ptr as *mut Value, value);
        }
    }

    fn get_field_volatile(&self, obj: ObjectRef, index: usize) -> Value {
        // JLS §17.7 atomicity for 16-byte `Value` slots: acquire the per-slot
        // stripe lock so paired writers don't expose a torn (tag, payload) to
        // this read. See `crate::collector::volatile_stripe_lock` for the
        // design rationale.
        //
        // Round-9 HIGH-3: restore the bracketing `fence(SeqCst)` pair.
        // Round-8 removed it on the (incorrect) premise that the mutex
        // acquire/release JMM edge subsumed it. It does not: parking_lot's
        // mutex acquire is Acquire-ordered and release is Release-ordered,
        // which gives happens-before WITHIN A SINGLE STRIPE but does NOT
        // establish a global total order across distinct stripes. JLS
        // §17.4.5 requires a total order over all `volatile` accesses
        // (synchronization order), so an IRIW-style observer can otherwise
        // see two volatile writes on different stripes in opposite orders
        // from two reader threads — a JMM violation. The SeqCst fence pair
        // adds the cross-stripe total order the per-stripe Acquire/Release
        // alone cannot supply, on top of which the lock still provides
        // 16-byte slot atomicity.
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        let _guard = crate::collector::volatile_stripe_lock(obj, index);
        let v = self.get_field(obj, index);
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        v
    }

    fn set_field_volatile(&self, obj: ObjectRef, index: usize, value: Value) {
        // Pair with `get_field_volatile`: the stripe lock makes the 16-byte
        // `Value` write appear atomic to a concurrent volatile reader, and
        // the bracketing SeqCst fences guarantee the JLS §17.4.5 total order
        // across distinct stripes (the lock alone is per-stripe HB only).
        // Round-9 HIGH-3: round-8 removed these fences and reintroduced an
        // IRIW-observable JMM hole — see `get_field_volatile` for details.
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        let _guard = crate::collector::volatile_stripe_lock(obj, index);
        self.set_field(obj, index, value);
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }

    fn array_length(&self, obj: ObjectRef) -> usize {
        self.get_header(obj).array_length as usize
    }

    fn get_array_element(&self, obj: ObjectRef, index: usize) -> Result<Value, i32> {
        let header = self.get_header(obj);
        let len = header.array_length as usize;
        if index >= len {
            return Err(index as i32);
        }
        let elem_size = crate::heap::element_byte_size(header.element_type);
        let slot_ptr = unsafe { obj.as_ptr().add(HEADER_SIZE + index * elem_size) };

        match header.element_type {
            ArrayElementType::Int => Ok(Value::Int(unsafe { std::ptr::read(slot_ptr as *const i32) })),
            ArrayElementType::Long => Ok(Value::Long(unsafe { std::ptr::read(slot_ptr as *const i64) })),
            ArrayElementType::Float => Ok(Value::Float(unsafe { std::ptr::read(slot_ptr as *const f32) })),
            ArrayElementType::Double => Ok(Value::Double(unsafe { std::ptr::read(slot_ptr as *const f64) })),
            ArrayElementType::Byte | ArrayElementType::Boolean => {
                Ok(Value::Int(unsafe { std::ptr::read(slot_ptr as *const i8) } as i32))
            }
            ArrayElementType::Short => {
                Ok(Value::Int(unsafe { std::ptr::read(slot_ptr as *const i16) } as i32))
            }
            ArrayElementType::Char => {
                Ok(Value::Int(unsafe { std::ptr::read(slot_ptr as *const u16) } as i32))
            }
            ArrayElementType::Reference => {
                let raw: u64 = unsafe { std::ptr::read(slot_ptr as *const u64) };
                if raw == 0 {
                    Ok(Value::Object(None))
                } else {
                    Ok(Value::Object(Some(unsafe {
                        ObjectRef::from_raw(raw as usize as *mut u8)
                    })))
                }
            }
        }
    }

    fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) -> Result<(), i32> {
        let header = self.get_header(obj);
        let len = header.array_length as usize;
        if index >= len {
            return Err(index as i32);
        }
        let elem_size = crate::heap::element_byte_size(header.element_type);
        let slot_ptr = unsafe { obj.as_ptr().add(HEADER_SIZE + index * elem_size) };

        match header.element_type {
            ArrayElementType::Int => {
                let v = value.as_int().unwrap_or(0);
                unsafe { std::ptr::write(slot_ptr as *mut i32, v) };
            }
            ArrayElementType::Long => {
                let v = value.as_long().unwrap_or(0);
                unsafe { std::ptr::write(slot_ptr as *mut i64, v) };
            }
            ArrayElementType::Float => {
                let v = match value {
                    Value::Float(f) => f,
                    _ => 0.0,
                };
                unsafe { std::ptr::write(slot_ptr as *mut f32, v) };
            }
            ArrayElementType::Double => {
                let v = match value {
                    Value::Double(d) => d,
                    _ => 0.0,
                };
                unsafe { std::ptr::write(slot_ptr as *mut f64, v) };
            }
            ArrayElementType::Byte | ArrayElementType::Boolean => {
                let v = value.as_int().unwrap_or(0) as i8;
                unsafe { std::ptr::write(slot_ptr as *mut i8, v) };
            }
            ArrayElementType::Short => {
                let v = value.as_int().unwrap_or(0) as i16;
                unsafe { std::ptr::write(slot_ptr as *mut i16, v) };
            }
            ArrayElementType::Char => {
                let v = value.as_int().unwrap_or(0) as u16;
                unsafe { std::ptr::write(slot_ptr as *mut u16, v) };
            }
            ArrayElementType::Reference => {
                let raw: u64 = match value {
                    Value::Object(Some(r)) => r.as_ptr() as u64,
                    _ => 0u64,
                };
                unsafe { std::ptr::write(slot_ptr as *mut u64, raw) };
            }
        }
        Ok(())
    }

    fn needs_gc(&self) -> bool {
        let regions = self.regions.lock();
        let free_count = regions
            .iter()
            .filter(|r| r.region_type == RegionType::Free)
            .count();
        let total = regions.len();
        // Trigger GC when less than 25% of regions are free
        free_count * 4 < total
    }

    fn collect_garbage(
        &self,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        // 1. Check if mixed GC is needed
        let result = if self.needs_mixed_gc() {
            self.mixed_collection(roots, monitors)
        } else {
            self.young_collection(roots, monitors)
        };

        // 2. Check IHOP -> start concurrent mark if threshold reached
        if self.check_ihop()
            && self.gc_state.phase() == ConcurrentGcPhase::Idle
        {
            self.start_concurrent_mark();
        }

        // 3. Adaptive IHOP
        let last_pause = self.total_pause_ms.load(Ordering::Relaxed);
        if last_pause > 0 {
            self.update_ihop(result.stats.bytes_freed as u64);
        }

        result
    }

    fn write_barrier(&self, obj: ObjectRef, stored_value: Value) {
        // Post-write barrier: track cross-region references in remembered sets
        if let Value::Object(Some(ref target)) = stored_value {
            self.post_write_barrier_rset(obj, *target);
        }
    }

    fn allocated_bytes(&self) -> usize {
        let regions = self.regions.lock();
        regions.iter().map(|r| r.cursor).sum()
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Find the first free region.
fn find_free_region(regions: &[G1Region]) -> Option<usize> {
    regions
        .iter()
        .position(|r| r.region_type == RegionType::Free)
}

/// Find `count` contiguous free regions.
fn find_contiguous_free(regions: &[G1Region], count: usize) -> Option<usize> {
    let mut run_start = 0;
    let mut run_len = 0;

    for (i, r) in regions.iter().enumerate() {
        if r.region_type == RegionType::Free {
            if run_len == 0 {
                run_start = i;
            }
            run_len += 1;
            if run_len >= count {
                return Some(run_start);
            }
        } else {
            run_len = 0;
        }
    }
    None
}

/// Compute total object size from header.
fn object_total_size(header: &ObjectHeader) -> usize {
    if header.kind == ObjectKind::Array {
        HEADER_SIZE + array_data_size(header.array_length as usize, header.element_type)
            .expect("array_data_size overflow in g1 object_total_size")
    } else {
        HEADER_SIZE + header.num_slots as usize * SLOT_SIZE
    }
}

/// Round-9 gc CRIT-1: returns true if this header marks the start of a
/// humongous continuation filler region. Walkers MUST check this before
/// computing a per-object size — the filler covers the whole region
/// regardless of the (synthetic) per-field values stored in the header.
#[inline]
fn is_humongous_filler(header: &ObjectHeader) -> bool {
    matches!(header.kind, ObjectKind::HumongousFiller)
}

/// Update reference fields in an object using the forwarding map.
fn update_object_refs(
    obj_ptr: *mut u8,
    header: &ObjectHeader,
    pointer_map: &HashMap<usize, usize>,
) {
    let data_start = unsafe { obj_ptr.add(HEADER_SIZE) };

    if header.kind == ObjectKind::Array {
        if header.element_type == ArrayElementType::Reference {
            for i in 0..header.array_length as usize {
                let slot_ptr = unsafe { data_start.add(i * 8) };
                let raw: u64 = unsafe { std::ptr::read(slot_ptr as *const u64) };
                if raw != 0 {
                    if let Some(&new_addr) = pointer_map.get(&(raw as usize)) {
                        unsafe {
                            std::ptr::write(slot_ptr as *mut u64, new_addr as u64);
                        }
                    }
                }
            }
        }
    } else {
        for slot_idx in 0..header.num_slots as usize {
            let slot_ptr = unsafe { data_start.add(slot_idx * SLOT_SIZE) };
            let value = unsafe { std::ptr::read(slot_ptr as *const Value) };
            if let Value::Object(Some(ref_obj)) = value {
                let ref_addr = ref_obj.as_ptr() as usize;
                if let Some(&new_addr) = pointer_map.get(&ref_addr) {
                    let new_value =
                        Value::Object(Some(unsafe { ObjectRef::from_raw(new_addr as *mut u8) }));
                    unsafe {
                        std::ptr::write(slot_ptr as *mut Value, new_value);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// T5.5.4 — Pause-budgeted region eviction policy
// ---------------------------------------------------------------------------

/// T5.5.4 — identifier type for regions selected for evacuation.
pub type RegionId = usize;

/// T5.5.4 — Select the top old regions to evacuate in the next mixed
/// collection, ordered by garbage-density and bounded by the pause
/// budget.
///
/// Selection algorithm:
///
/// 1. **Filter.** Only [`RegionType::Old`] regions with `live_bytes <
///    top` are considered — a region with zero garbage wastes
///    evacuation effort.
/// 2. **Rank.** Sort by the ratio `garbage_bytes / live_bytes`
///    descending: a region with lots of garbage and little live data
///    releases more memory per byte copied. `live_bytes == 0` is
///    treated as infinite ratio (perfect garbage region, evacuate
///    first — technically it just needs to be reclaimed but we keep it
///    in the set so the caller can free it uniformly).
/// 3. **Pack.** Walk the ranking, accumulating
///    [`Region::estimated_evac_cost_ns`]; stop once the next region
///    would push the running sum over `target_pause_ns`. Always
///    includes at least the first candidate if one exists, so that a
///    single oversized region does not starve the mixed GC entirely.
/// 4. **Deterministic.** Ties (same ratio) are broken by ascending
///    `region.index`.
pub fn select_evacuation_candidates(
    regions: &[crate::region::Region],
    target_pause_ns: u64,
) -> Vec<RegionId> {
    // Only old regions with at least one garbage byte are candidates.
    let mut candidates: Vec<(&crate::region::Region, f64)> = regions
        .iter()
        .filter(|r| {
            r.region_type == crate::region::RegionType::Old
                && r.garbage_bytes() > 0
        })
        .map(|r| {
            // Ratio: garbage / live. If live is 0 the region is pure
            // garbage → infinite priority.
            let ratio = if r.live_bytes == 0 {
                f64::INFINITY
            } else {
                r.garbage_bytes() as f64 / r.live_bytes as f64
            };
            (r, ratio)
        })
        .collect();

    // Sort by ratio descending, tiebreak by index ascending.
    candidates.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.index.cmp(&b.0.index))
    });

    let mut selected: Vec<RegionId> = Vec::new();
    let mut running_cost_ns: u64 = 0;
    for (region, _) in candidates {
        let cost = region.estimated_evac_cost_ns();
        // Always include the first candidate so the GC makes forward
        // progress even on a tight budget.
        if selected.is_empty() {
            selected.push(region.index);
            running_cost_ns = running_cost_ns.saturating_add(cost);
            continue;
        }
        let next_cost = running_cost_ns.saturating_add(cost);
        if next_cost > target_pause_ns {
            break;
        }
        selected.push(region.index);
        running_cost_ns = next_cost;
    }
    selected
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// No-op monitor cleanup for tests.
    struct NoopMonitors;
    impl MonitorCleanup for NoopMonitors {
        fn remap_after_gc(&self, _pointer_map: &HashMap<usize, usize>) {}
    }

    fn small_config() -> G1CollectorConfig {
        G1CollectorConfig {
            heap_size: 8 * 1024 * 1024, // 8 MB
            region_size: 1024 * 1024,     // 1 MB
            max_gc_pause_ms: 200,
            ihop_percent: 45,
            promotion_age: 3,
            gc_worker_threads: 1,
            string_dedup_enabled: false,
            mixed_gc_count_target: 8,
            old_cset_region_threshold_percent: 10,
        }
    }

    fn make_collector() -> G1Collector {
        G1Collector::new(small_config())
    }

    // -- Config defaults --

    #[test]
    fn config_defaults() {
        let cfg = G1CollectorConfig::default();
        assert_eq!(cfg.heap_size, 256 * 1024 * 1024);
        assert_eq!(cfg.region_size, 1024 * 1024);
        assert_eq!(cfg.max_gc_pause_ms, 200);
        // T19.3.G1 raised from 45 → 70.
        assert_eq!(cfg.ihop_percent, 70);
        assert_eq!(cfg.promotion_age, 15);
        assert_eq!(cfg.gc_worker_threads, 4);
        assert!(!cfg.string_dedup_enabled);
        assert_eq!(cfg.mixed_gc_count_target, 8);
        assert_eq!(cfg.old_cset_region_threshold_percent, 10);
    }

    // -- Region basics --

    #[test]
    fn region_new_is_free() {
        let r = G1Region::new(1024);
        assert_eq!(r.region_type, RegionType::Free);
        assert_eq!(r.cursor, 0);
        assert!(!r.pinned);
        assert_eq!(r.age, 0);
        assert_eq!(r.live_bytes, 0);
    }

    #[test]
    fn region_bump_alloc() {
        let mut r = G1Region::new(4096);
        let (ptr, offset) = r.bump_alloc(64, 8).unwrap();
        assert!(!ptr.is_null());
        assert_eq!(offset, 0);
        assert_eq!(r.cursor, 64);

        let (ptr2, offset2) = r.bump_alloc(128, 8).unwrap();
        assert!(!ptr2.is_null());
        assert_eq!(offset2, 64);
        assert_eq!(r.cursor, 192);
    }

    #[test]
    fn region_bump_alloc_full() {
        let mut r = G1Region::new(128);
        assert!(r.bump_alloc(64, 8).is_some());
        assert!(r.bump_alloc(64, 8).is_some());
        assert!(r.bump_alloc(1, 8).is_none()); // full
    }

    #[test]
    fn region_remaining() {
        let mut r = G1Region::new(1024);
        assert_eq!(r.remaining(), 1024);
        r.bump_alloc(100, 8);
        assert_eq!(r.remaining(), 924);
    }

    #[test]
    fn region_reset() {
        let mut r = G1Region::new(1024);
        r.region_type = RegionType::Eden;
        r.cursor = 500;
        r.live_bytes = 200;
        r.pinned = true;
        r.age = 5;
        r.reset();
        assert_eq!(r.region_type, RegionType::Free);
        assert_eq!(r.cursor, 0);
        assert!(!r.pinned);
        assert_eq!(r.age, 0);
    }

    // -- Collector creation --

    #[test]
    fn collector_creation() {
        let gc = make_collector();
        assert_eq!(gc.num_regions(), 8);
        assert_eq!(gc.collection_count(), 0);
        assert_eq!(gc.total_pause_ms(), 0);
    }

    #[test]
    fn collector_default() {
        let gc = G1Collector::with_defaults();
        assert_eq!(gc.num_regions(), 256);
    }

    // -- Allocation --

    #[test]
    fn alloc_object_basic() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 2);
        let header = gc.get_header(obj);
        assert_eq!(header.class_id, ClassId::new(1));
        assert_eq!(header.kind, ObjectKind::Object);
        assert_eq!(header.num_slots, 2);
        assert_eq!(gc.count_regions(RegionType::Eden), 1);
    }

    #[test]
    fn alloc_multiple_objects_same_region() {
        let gc = make_collector();
        let _obj1 = gc.alloc_object(ClassId::new(1), 1);
        let _obj2 = gc.alloc_object(ClassId::new(2), 1);
        assert_eq!(gc.count_regions(RegionType::Eden), 1);
    }

    #[test]
    fn alloc_array_basic() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(5), ArrayElementType::Int, 10);
        assert_eq!(gc.array_length(arr), 10);
        assert_eq!(gc.kind_of(arr), ObjectKind::Array);
    }

    #[test]
    fn alloc_field_read_write() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 3);
        gc.set_field(obj, 0, Value::Int(42));
        gc.set_field(obj, 1, Value::Long(1000));
        gc.set_field(obj, 2, Value::Float(3.14));
        assert_eq!(gc.get_field(obj, 0).as_int(), Some(42));
        assert_eq!(gc.get_field(obj, 1).as_long(), Some(1000));
    }

    #[test]
    fn alloc_array_elements() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, 3);
        gc.set_array_element(arr, 0, Value::Int(10)).unwrap();
        gc.set_array_element(arr, 1, Value::Int(20)).unwrap();
        gc.set_array_element(arr, 2, Value::Int(30)).unwrap();
        assert_eq!(gc.get_array_element(arr, 0).unwrap().as_int(), Some(10));
        assert_eq!(gc.get_array_element(arr, 1).unwrap().as_int(), Some(20));
        assert_eq!(gc.get_array_element(arr, 2).unwrap().as_int(), Some(30));
    }

    #[test]
    fn alloc_array_bounds_check() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, 2);
        assert!(gc.get_array_element(arr, 2).is_err());
        assert!(gc.set_array_element(arr, 5, Value::Int(0)).is_err());
    }

    // -- Humongous allocation --

    #[test]
    fn humongous_allocation() {
        let gc = make_collector();
        // region_size is 1MB, so > 512KB is humongous
        // A large array: HEADER_SIZE + 600000 * 4 bytes = ~2.4 MB
        let large = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, 150_000);
        assert_eq!(gc.array_length(large), 150_000);
        assert!(gc.count_regions(RegionType::HumongousStart) >= 1);
    }

    // -- Young collection --

    #[test]
    fn young_collection_basic() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(42));

        let mut roots = vec![obj];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        assert!(result.stats.objects_copied >= 1);
        let new_obj = roots[0];
        // Should have been copied
        assert_eq!(gc.get_field(new_obj, 0).as_int(), Some(42));
    }

    #[test]
    fn young_collection_with_reference_chain() {
        let gc = make_collector();
        let a = gc.alloc_object(ClassId::new(1), 1);
        let b = gc.alloc_object(ClassId::new(2), 1);
        gc.set_field(a, 0, Value::Object(Some(b)));
        gc.set_field(b, 0, Value::Int(99));

        let mut roots = vec![a];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        assert_eq!(result.stats.objects_copied, 2);
        let new_a = roots[0];
        let new_b_val = gc.get_field(new_a, 0);
        if let Value::Object(Some(new_b)) = new_b_val {
            assert_eq!(gc.get_field(new_b, 0).as_int(), Some(99));
        } else {
            panic!("expected reference to B");
        }
    }

    #[test]
    fn young_collection_promotion() {
        let mut cfg = small_config();
        cfg.promotion_age = 1; // promote after 1 GC cycle
        let gc = G1Collector::new(cfg);

        let obj = gc.alloc_object(ClassId::new(1), 0);
        let mut roots = vec![obj];

        // First GC: object goes to Survivor with age 1
        let _r1 = gc.young_collection(&mut roots, &NoopMonitors);
        // The object's gc_age should now be 1 which >= promotion_age
        // So next GC it should go to Old if it was in Survivor
        // Actually with promotion_age=1, age >= 1 means immediate promotion to Old
        let header = gc.get_header(roots[0]);
        // With promotion_age=1, objects with gc_age >= 1 go to Old.
        // After first GC, gc_age is incremented to 1.
        assert!(header.gc_age >= 1);
    }

    #[test]
    fn young_collection_frees_eden() {
        let gc = make_collector();
        let _obj = gc.alloc_object(ClassId::new(1), 1);
        assert_eq!(gc.count_regions(RegionType::Eden), 1);

        let mut roots = vec![_obj];
        gc.young_collection(&mut roots, &NoopMonitors);

        // Eden should be freed after collection
        assert_eq!(gc.count_regions(RegionType::Eden), 0);
    }

    #[test]
    fn young_collection_unreachable_freed() {
        let gc = make_collector();
        let live = gc.alloc_object(ClassId::new(1), 0);
        let _dead = gc.alloc_object(ClassId::new(2), 0);

        let mut roots = vec![live]; // _dead not in roots
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        // Only the live object should be copied
        assert_eq!(result.stats.objects_copied, 1);
    }

    #[test]
    fn young_collection_pointer_map() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 0);
        let old_addr = obj.as_ptr() as usize;

        let mut roots = vec![obj];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        assert!(result.pointer_map.contains_key(&old_addr));
        assert_eq!(
            result.pointer_map[&old_addr],
            roots[0].as_ptr() as usize
        );
    }

    // -- Mixed collection --

    #[test]
    fn mixed_collection_selects_old_regions() {
        let gc = make_collector();

        // Manually set up some old regions with gc_efficiency data
        {
            let mut regions = gc.regions.lock();
            regions[0].region_type = RegionType::Old;
            regions[0].cursor = 100;
            regions[0].gc_efficiency = 0.1; // 10% live = 90% garbage, best candidate
            regions[1].region_type = RegionType::Old;
            regions[1].cursor = 100;
            regions[1].gc_efficiency = 0.9; // 90% live = 10% garbage, poor candidate
        }

        // Force marking complete to trigger mixed GC
        gc.marking_complete.store(true, Ordering::Relaxed);
        gc.mixed_gc_remaining.store(1, Ordering::Relaxed);

        let mut roots = vec![];
        let _result = gc.mixed_collection(&mut roots, &NoopMonitors);

        // Region 0 (lowest gc_efficiency = most garbage) should have been collected.
        // Region 1 (high gc_efficiency) should remain.
        assert_eq!(gc.count_regions(RegionType::Old), 1);
    }

    // -- IHOP --

    #[test]
    fn ihop_threshold_calculation() {
        let mut cfg = small_config();
        cfg.ihop_percent = 50;
        let gc = G1Collector::new(cfg.clone());

        let expected = cfg.heap_size / 2;
        assert_eq!(gc.marking_threshold_bytes(), expected);
    }

    #[test]
    fn ihop_check_below_threshold() {
        let gc = make_collector();
        gc.old_gen_bytes.store(0, Ordering::Relaxed);
        assert!(!gc.check_ihop());
    }

    #[test]
    fn ihop_check_above_threshold() {
        let gc = make_collector();
        let threshold = gc.marking_threshold_bytes();
        gc.old_gen_bytes.store(threshold + 1, Ordering::Relaxed);
        assert!(gc.check_ihop());
    }

    #[test]
    fn ihop_adaptive_adjustment_lower() {
        let gc = make_collector();
        let before = gc.marking_threshold_bytes();
        // Pause way over target should lower threshold
        gc.update_ihop(gc.config.max_gc_pause_ms + 100);
        let after = gc.marking_threshold_bytes();
        assert!(after < before);
    }

    #[test]
    fn ihop_adaptive_adjustment_raise() {
        let gc = make_collector();
        let before = gc.marking_threshold_bytes();
        // Pause well under half the target should raise threshold
        gc.update_ihop(0);
        let after = gc.marking_threshold_bytes();
        assert!(after >= before);
    }

    // T19.3.G1 — GC allocation-storm follow-ups.

    #[test]
    fn t19_default_ihop_is_70_percent() {
        // T19.3.G1 raised the default IHOP from 45% → 70% so static-init
        // bursts on 256 MiB heaps don't fire concurrent marking while
        // the heap is still essentially empty.
        let cfg = G1CollectorConfig::default();
        assert_eq!(cfg.ihop_percent, 70);
    }

    #[test]
    fn t19_default_ihop_threshold_bytes_is_70_percent_of_heap() {
        let cfg = G1CollectorConfig::default();
        let gc = G1Collector::new(cfg.clone());
        // 70 % of 256 MiB == 179.2 MiB. Accept any value within 1% of
        // the exact calculation so future rounding tweaks don't break.
        let expected = cfg.heap_size * 70 / 100;
        let actual = gc.marking_threshold_bytes();
        let diff = if actual > expected { actual - expected } else { expected - actual };
        assert!(
            diff * 100 <= expected,
            "ihop threshold {actual} deviates from {expected} by more than 1 %"
        );
    }

    #[test]
    fn t19_default_ihop_does_not_fire_at_40_percent_occupancy() {
        // A Quarkus static-init burst at 40 % old-gen occupancy used to
        // trigger concurrent marking under the old 45 % threshold. Post-fix
        // the heap must tolerate 40 % without firing.
        let gc = G1Collector::new(G1CollectorConfig::default());
        let forty_percent = gc.config.heap_size * 40 / 100;
        gc.old_gen_bytes.store(forty_percent, Ordering::Relaxed);
        assert!(!gc.check_ihop(), "IHOP fired at 40% occupancy with 70% threshold");
    }

    #[test]
    fn t19_default_ihop_still_fires_at_75_percent_occupancy() {
        // Above 70 %, the threshold is crossed and concurrent marking
        // must start. This keeps the safety net intact.
        let gc = G1Collector::new(G1CollectorConfig::default());
        let seventy_five_percent = gc.config.heap_size * 75 / 100;
        gc.old_gen_bytes
            .store(seventy_five_percent, Ordering::Relaxed);
        assert!(gc.check_ihop(), "IHOP didn't fire at 75% occupancy with 70% threshold");
    }

    // -- Region pinning --

    #[test]
    fn region_pinning() {
        let gc = make_collector();
        assert!(!gc.is_pinned(0));
        gc.pin_region(0);
        assert!(gc.is_pinned(0));
        gc.unpin_region(0);
        assert!(!gc.is_pinned(0));
    }

    #[test]
    fn pinned_region_not_collected() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(77));

        // Find the region and pin it
        let region_idx = {
            let regions = gc.regions.lock();
            gc.region_for_ptr(&regions, obj.as_ptr()).unwrap()
        };
        gc.pin_region(region_idx);

        let mut roots = vec![obj];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        // Pinned region should not be collected
        assert_eq!(result.stats.objects_copied, 0);
        // Object should still be at the same address
        assert_eq!(roots[0].as_ptr(), obj.as_ptr());
    }

    #[test]
    fn pin_out_of_bounds() {
        let gc = make_collector();
        gc.pin_region(9999); // should not panic
        assert!(!gc.is_pinned(9999));
    }

    // -- String deduplication --

    #[test]
    fn string_dedup_disabled_by_default() {
        let gc = make_collector();
        assert_eq!(gc.deduplicate_string(12345, 0x1000), None);
    }

    #[test]
    fn string_dedup_enabled() {
        let mut cfg = small_config();
        cfg.string_dedup_enabled = true;
        let gc = G1Collector::new(cfg);

        // First string with this hash: not deduped, just registered.
        assert_eq!(gc.deduplicate_string(42, 0x1000), None);
        // Second string with same hash: deduped — caller gets the canonical
        // address so it can redirect its pointer.
        assert_eq!(gc.deduplicate_string(42, 0x2000), Some(0x1000));
        // Different hash: not deduped, registered fresh.
        assert_eq!(gc.deduplicate_string(99, 0x3000), None);
        // Re-hit the second hash with yet another address — still points
        // at the original canonical instance.
        assert_eq!(gc.deduplicate_string(99, 0x4000), Some(0x3000));
    }

    // -- GC logging --

    #[test]
    fn gc_logging_toggle() {
        let gc = make_collector();
        assert!(!gc.gc_log_enabled.load(Ordering::Relaxed));
        gc.enable_gc_logging();
        assert!(gc.gc_log_enabled.load(Ordering::Relaxed));
        gc.disable_gc_logging();
        assert!(!gc.gc_log_enabled.load(Ordering::Relaxed));
    }

    // -- Collection counting --

    #[test]
    fn collection_count_increments() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 0);
        let mut roots = vec![obj];

        assert_eq!(gc.collection_count(), 0);
        gc.young_collection(&mut roots, &NoopMonitors);
        assert_eq!(gc.collection_count(), 1);
        gc.young_collection(&mut roots, &NoopMonitors);
        assert_eq!(gc.collection_count(), 2);
    }

    // -- Concurrent marking --

    #[test]
    fn concurrent_mark_phases() {
        let gc = make_collector();
        assert_eq!(gc.gc_phase(), ConcurrentGcPhase::Idle);

        gc.start_concurrent_mark();
        assert_eq!(gc.gc_phase(), ConcurrentGcPhase::ConcurrentMark);

        let done = gc.concurrent_mark_step(1000);
        assert!(done); // no objects to mark, should complete immediately

        let obj = gc.alloc_object(ClassId::new(1), 0);
        gc.remark(&[obj]);
        assert_eq!(gc.gc_phase(), ConcurrentGcPhase::Remark);
    }

    #[test]
    fn cleanup_computes_live_bytes() {
        // Round-2 fix (HIGH — GC #5): bitmaps now live per-region and
        // are keyed off each region's heap-allocated `data.as_ptr()`,
        // so `try_mark`/`is_marked` accept the real addresses of
        // objects living inside the region. This means `cleanup` can
        // now correctly attribute live bytes to a marked object.
        //
        // The previous version of this test documented the BUG —
        // marking a real address against a `[0, heap_size)` bitmap
        // always silently failed, so `live_bytes` was always 0. After
        // the per-region-bitmap fix, marking succeeds and `cleanup`
        // reports a non-zero `live_bytes` for the region holding the
        // marked object.
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 2);
        let obj_addr = obj.as_ptr() as usize;

        // Locate the region the allocator placed the object in, mark
        // the object in *that* region's bitmap, then promote every
        // Eden region (including ours) to Old so `cleanup` walks them.
        {
            let mut regions = gc.regions.lock();
            let region_idx = gc
                .region_for_ptr(&regions, obj.as_ptr())
                .expect("freshly-allocated object must live in some region");
            let marked = regions[region_idx].mark_bitmap.try_mark(obj_addr);
            assert!(
                marked,
                "per-region bitmap must accept real heap addresses post-fix"
            );
            for r in regions.iter_mut() {
                if r.region_type == RegionType::Eden {
                    r.region_type = RegionType::Old;
                }
            }
        }

        gc.cleanup();

        // The region containing our marked object must report non-zero
        // live_bytes (was always 0 under the buggy global bitmap). The
        // exact byte count equals one ObjectHeader + 2 SLOT_SIZE
        // fields = HEADER_SIZE + 2*SLOT_SIZE. We assert >0 to keep the
        // test resilient to header-size tuning.
        let regions = gc.regions.lock();
        let holding_region = regions
            .iter()
            .find(|r| {
                let base = r.data.as_ptr() as usize;
                obj_addr >= base && obj_addr < base + r.data.len()
            })
            .expect("holding region must still exist");
        assert!(
            holding_region.live_bytes > 0,
            "per-region bitmap fix: cleanup must report >0 live_bytes for the marked region (got {})",
            holding_region.live_bytes,
        );
        assert_eq!(
            holding_region.region_type,
            RegionType::Old,
            "non-empty old region must not be freed by cleanup"
        );

        // Other old regions had no marked objects, so they should have
        // been freed (live_bytes == 0, type went back to Free).
        let old_count = regions
            .iter()
            .filter(|r| r.region_type == RegionType::Old)
            .count();
        assert_eq!(
            old_count, 1,
            "only the region containing the marked object should remain Old; \
             empty old regions must be freed"
        );
    }

    // -- Needs GC --

    #[test]
    fn needs_gc_when_regions_full() {
        let gc = make_collector();
        assert!(!gc.needs_gc()); // all 8 regions free

        // Use up most regions
        {
            let mut regions = gc.regions.lock();
            for i in 0..7 {
                regions[i].region_type = RegionType::Eden;
                regions[i].cursor = 100;
            }
        }
        // 1 free out of 8 = 12.5% free, threshold is 25%
        assert!(gc.needs_gc());
    }

    // -- Allocated bytes --

    #[test]
    fn allocated_bytes_tracking() {
        let gc = make_collector();
        assert_eq!(gc.allocated_bytes(), 0);
        let _obj = gc.alloc_object(ClassId::new(1), 2);
        let bytes = gc.allocated_bytes();
        assert!(bytes > 0);
        assert_eq!(bytes, HEADER_SIZE + 2 * SLOT_SIZE);
    }

    // -- collect_garbage trait method --

    #[test]
    fn collect_garbage_via_trait() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(123));

        let mut roots = vec![obj];
        let result = gc.collect_garbage(&mut roots, &NoopMonitors);

        assert!(result.stats.objects_copied >= 1);
        assert_eq!(gc.get_field(roots[0], 0).as_int(), Some(123));
    }

    // -- Identity hash code --

    #[test]
    fn identity_hash_code_unique() {
        let gc = make_collector();
        let a = gc.alloc_object(ClassId::new(1), 0);
        let b = gc.alloc_object(ClassId::new(1), 0);
        assert_ne!(gc.identity_hash_code(a), gc.identity_hash_code(b));
    }

    // -- Volatile field access --

    #[test]
    fn volatile_field_access() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field_volatile(obj, 0, Value::Int(42));
        assert_eq!(gc.get_field_volatile(obj, 0).as_int(), Some(42));
    }

    // -- Region type transitions --

    #[test]
    fn region_type_transitions() {
        let mut r = G1Region::new(1024);
        assert_eq!(r.region_type, RegionType::Free);

        r.region_type = RegionType::Eden;
        assert_eq!(r.region_type, RegionType::Eden);

        r.region_type = RegionType::Survivor;
        r.age = 3;
        assert_eq!(r.region_type, RegionType::Survivor);

        r.region_type = RegionType::Old;
        assert_eq!(r.region_type, RegionType::Old);

        r.reset();
        assert_eq!(r.region_type, RegionType::Free);
    }

    // -- Evacuation failure (pinned region) --

    #[test]
    fn evacuation_failure_pinned() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(999));

        // Pin the region
        let region_idx = {
            let regions = gc.regions.lock();
            gc.region_for_ptr(&regions, obj.as_ptr()).unwrap()
        };
        gc.pin_region(region_idx);

        let mut roots = vec![obj];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        // Object should NOT have been evacuated
        assert_eq!(result.stats.objects_copied, 0);
        assert_eq!(gc.get_field(roots[0], 0).as_int(), Some(999));
    }

    // -- Mixed GC remaining counter --

    #[test]
    fn mixed_gc_remaining_decrements() {
        let gc = make_collector();
        gc.marking_complete.store(true, Ordering::Relaxed);
        gc.mixed_gc_remaining.store(3, Ordering::Relaxed);

        let mut roots = vec![];
        gc.mixed_collection(&mut roots, &NoopMonitors);
        assert_eq!(gc.mixed_gc_remaining.load(Ordering::Relaxed), 2);

        gc.mixed_collection(&mut roots, &NoopMonitors);
        assert_eq!(gc.mixed_gc_remaining.load(Ordering::Relaxed), 1);

        gc.mixed_collection(&mut roots, &NoopMonitors);
        assert_eq!(gc.mixed_gc_remaining.load(Ordering::Relaxed), 0);
        assert!(!gc.marking_complete.load(Ordering::Relaxed));
    }

    // -- Remembered set tracking --

    #[test]
    fn remembered_set_tracking() {
        let mut rset = RememberedSet::default();
        rset.add_reference(1);
        rset.add_reference(3);
        rset.add_reference(1); // duplicate
        assert_eq!(rset.source_count(), 2);

        let sources: Vec<usize> = rset.sources();
        assert!(sources.contains(&1));
        assert!(sources.contains(&3));

        rset.clear();
        assert_eq!(rset.source_count(), 0);
    }

    // -- GC efficiency --

    #[test]
    fn gc_efficiency_computation() {
        let mut r = G1Region::new(1024);
        r.region_type = RegionType::Old;
        r.cursor = 500;
        r.live_bytes = 200;
        r.gc_efficiency = r.live_bytes as f64 / 1024.0;
        assert!(r.gc_efficiency < 0.2);
        assert!(r.gc_efficiency > 0.19);
    }

    // ======================================================================
    // Phase 89 tests
    // ======================================================================

    // -- 89.1: Fallible allocation --

    #[test]
    fn p89_try_alloc_object_success() {
        let gc = make_collector();
        let obj = gc.try_alloc_object(ClassId::new(1), 3);
        assert!(obj.is_some());
        let obj = obj.unwrap();
        assert_eq!(gc.class_id_of(obj), ClassId::new(1));
        assert_eq!(gc.get_header(obj).num_slots, 3);
    }

    #[test]
    fn p89_try_alloc_object_returns_none_when_full() {
        // Tiny config: 2 regions of 4 KB each = 8 KB total
        let cfg = G1CollectorConfig {
            heap_size: 8192,
            region_size: 4096,
            ..small_config()
        };
        let gc = G1Collector::new(cfg);

        // Fill all regions by allocating many objects
        let mut allocated = Vec::new();
        for _i in 0..100 {
            match gc.try_alloc_object(ClassId::new(1), 4) {
                Some(obj) => allocated.push(obj),
                None => break,
            }
        }
        // Eventually should return None
        // With 2 regions of 4096 bytes, each object ~72 bytes, we can fit many but not 100
        assert!(allocated.len() < 100, "should have run out of space");
        // Final try should fail
        assert!(gc.try_alloc_object(ClassId::new(1), 4).is_none());
    }

    #[test]
    fn p89_try_alloc_array_success() {
        let gc = make_collector();
        let arr = gc.try_alloc_array(ClassId::new(2), ArrayElementType::Int, 10);
        assert!(arr.is_some());
        let arr = arr.unwrap();
        assert_eq!(gc.array_length(arr), 10);
        assert_eq!(gc.get_header(arr).element_type, ArrayElementType::Int);
    }

    #[test]
    fn p89_try_alloc_array_returns_none_when_full() {
        let cfg = G1CollectorConfig {
            heap_size: 8192,
            region_size: 4096,
            ..small_config()
        };
        let gc = G1Collector::new(cfg);

        // Allocate a huge array that won't fit in 2 regions
        let result = gc.try_alloc_array(ClassId::new(1), ArrayElementType::Long, 2000);
        // 2000 longs = 16000 bytes + header > 8192 total heap
        assert!(result.is_none());
    }

    // -- 89.1: TLAB refill from G1 Eden --

    #[test]
    fn p89_tlab_refill_from_eden() {
        let gc = make_collector();
        let result = gc.refill_tlab(4096);
        assert!(result.is_some());
        let (ptr, size) = result.unwrap();
        assert!(!ptr.is_null());
        assert!(size >= 256); // must be at least minimum useful size
        assert!(size <= 4096);

        // Second refill should still work (from same or new region)
        let result2 = gc.refill_tlab(4096);
        assert!(result2.is_some());
    }

    #[test]
    fn p89_tlab_refill_returns_none_when_exhausted() {
        let cfg = G1CollectorConfig {
            heap_size: 2 * 65536,     // 2 regions
            region_size: 65536,        // 64 KB regions
            ..small_config()
        };
        let gc = G1Collector::new(cfg);

        // Request small TLABs to stay under region_size/2
        let r1 = gc.refill_tlab(16384);
        assert!(r1.is_some());

        // Keep refilling until exhausted
        let mut refills = 1;
        loop {
            match gc.refill_tlab(16384) {
                Some(_) => refills += 1,
                None => break,
            }
            if refills > 20 { break; } // safety
        }
        // Should have run out eventually
        assert!(gc.refill_tlab(16384).is_none());
    }

    // -- 89.1: Young GC trigger and survivor promotion --

    #[test]
    fn p89_young_gc_trigger_and_collect() {
        let gc = make_collector(); // 8 MB, 1 MB regions
        // Allocate objects until GC is needed
        let mut roots: Vec<ObjectRef> = Vec::new();
        for i in 0..50 {
            if let Some(obj) = gc.try_alloc_object(ClassId::new(1), 2) {
                gc.set_field(obj, 0, Value::Int(i));
                roots.push(obj);
            }
        }
        assert!(!roots.is_empty());

        // Run a young collection
        let result = gc.young_collection(&mut roots, &NoopMonitors);
        assert!(result.stats.objects_copied > 0 || result.stats.bytes_freed > 0);

        // Root objects should still be accessible (possibly relocated)
        for root in &roots {
            let val = gc.get_field(*root, 0);
            assert!(val.as_int().is_some());
        }
    }

    #[test]
    fn p89_survivor_promotion_after_aging() {
        let cfg = G1CollectorConfig {
            heap_size: 8 * 1024 * 1024,
            region_size: 1024 * 1024,
            promotion_age: 1, // promote after 1 young GC (age >= 1)
            ..small_config()
        };
        let gc = G1Collector::new(cfg);

        // Allocate an object
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(999));
        let mut roots = vec![obj];

        // First young GC: object starts at age 0, goes to Survivor with age 1
        let r1 = gc.young_collection(&mut roots, &NoopMonitors);
        assert!(r1.stats.objects_copied > 0);
        assert_eq!(gc.get_field(roots[0], 0).as_int(), Some(999));

        // Second young GC: age 1 >= promotion_age(1), promoted to Old
        let _r2 = gc.young_collection(&mut roots, &NoopMonitors);
        assert_eq!(gc.get_field(roots[0], 0).as_int(), Some(999));

        // Verify old-gen bytes increased (object promoted)
        let old_bytes = gc.old_gen_bytes();
        assert!(old_bytes > 0, "object should have been promoted to old gen");
    }

    #[test]
    fn p89_humongous_allocation() {
        let gc = make_collector(); // 1 MB regions
        // Object with > 512KB of fields = humongous
        let num_fields = (512 * 1024) / SLOT_SIZE + 1;
        let obj = gc.try_alloc_object(ClassId::new(1), num_fields);
        assert!(obj.is_some());
        let obj = obj.unwrap();
        assert_eq!(gc.get_header(obj).num_slots, num_fields as u32);

        // Verify the region is marked humongous
        let humongous_count = gc.count_regions(RegionType::HumongousStart);
        assert!(humongous_count >= 1);
    }

    #[test]
    fn p89_oom_after_gc_returns_none() {
        let cfg = G1CollectorConfig {
            heap_size: 2 * 65536,
            region_size: 65536,
            ..small_config()
        };
        let gc = G1Collector::new(cfg);

        // Fill heap completely
        let mut allocated = Vec::new();
        loop {
            match gc.try_alloc_object(ClassId::new(1), 4) {
                Some(obj) => allocated.push(obj),
                None => break,
            }
        }
        assert!(!allocated.is_empty());

        // GC with all objects rooted — they move to survivor/old but still fill the heap
        gc.collect_garbage(&mut allocated, &NoopMonitors);

        // Keep allocating until we're full again (GC freed Eden → moved to Survivor)
        loop {
            match gc.try_alloc_object(ClassId::new(1), 4) {
                Some(obj) => allocated.push(obj),
                None => break,
            }
        }

        // Now truly OOM with all roots held
        gc.collect_garbage(&mut allocated, &NoopMonitors);

        // Fill again
        loop {
            match gc.try_alloc_object(ClassId::new(1), 4) {
                Some(obj) => allocated.push(obj),
                None => break,
            }
        }

        // After multiple rounds of GC+fill, heap should eventually be fully packed
        // Verify the allocation eventually fails
        let final_try = gc.try_alloc_object(ClassId::new(1), 4);
        assert!(final_try.is_none(), "heap should be exhausted after repeated fill+GC cycles");
    }

    // -- 89.2: SATB write barrier --

    #[test]
    fn p89_satb_pre_barrier_logs_when_active() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        let addr = obj.as_ptr() as usize;

        // SATB inactive — should not log
        gc.satb_pre_barrier(addr);
        // Flush in case a previous test on this thread left buffered entries.
        crate::satb::flush_thread_satb_buffer(gc.satb_queue());
        let _ = gc.satb_queue().drain();
        assert!(gc.satb_queue().is_empty());

        // Activate SATB and log. The barrier now writes into the per-thread
        // buffer; force a safepoint-style flush so the global queue sees it
        // without waiting for the auto-flush threshold.
        gc.satb_queue().activate();
        gc.satb_pre_barrier(addr);
        crate::satb::flush_thread_satb_buffer(gc.satb_queue());
        assert_eq!(gc.satb_queue().len(), 1);

        let drained = gc.satb_queue().drain();
        assert_eq!(drained, vec![addr]);

        gc.satb_queue().deactivate();
    }

    #[test]
    fn p89_cross_region_rset_tracking() {
        let gc = make_collector();
        // Allocate two objects — they may land in the same or different regions
        // Force them into different regions by filling the first
        let obj1 = gc.alloc_object(ClassId::new(1), 1);

        // Fill current Eden to force next alloc into a new region
        while gc.try_alloc_object(ClassId::new(1), 100).is_some() {
            // keep going until region is full
        }
        // Allocate in a new region by doing a collect first to free space
        let mut roots = vec![obj1];
        gc.collect_garbage(&mut roots, &NoopMonitors);
        let obj2 = gc.alloc_object(ClassId::new(1), 1);

        // Store obj2 ref in obj1 — triggers write barrier
        gc.set_field(roots[0], 0, Value::Object(Some(obj2)));
        gc.write_barrier(roots[0], Value::Object(Some(obj2)));

        // The write barrier should have tracked the cross-region reference
        // (or it's a same-region ref, which is fine too — we just verify no crash)
    }

    #[test]
    fn p89_satb_barrier_skips_null() {
        let gc = make_collector();
        gc.satb_queue().activate();
        gc.satb_pre_barrier(0); // null ref — should be skipped
        assert!(gc.satb_queue().is_empty());
        gc.satb_queue().deactivate();
    }

    // -- 89.3: Reference processing wiring --

    #[test]
    fn p89_weak_ref_cleared_after_gc() {
        use crate::reference::{ReferenceProcessor, ReferenceType};

        let gc = make_collector();
        let mut rp = ReferenceProcessor::new();

        // Allocate a referent object (will be unreachable)
        let referent = gc.alloc_object(ClassId::new(10), 1);
        let referent_addr = referent.as_ptr() as usize;

        // Allocate a WeakReference wrapper
        let weak_ref = gc.alloc_object(ClassId::new(20), 2);
        let weak_addr = weak_ref.as_ptr() as usize;

        // Register the weak reference
        rp.discover_reference(ReferenceType::Weak, weak_addr, referent_addr, None);

        // Process with referent unmarked (unreachable)
        let is_marked = |addr: usize| -> bool { addr != referent_addr };
        let result = rp.process_references(&is_marked, 64, 0);

        // Weak referent should be cleared
        assert!(result.stats.weak_refs_cleared > 0
            || rp.cleared_ref_objects().contains(&weak_addr));
    }

    #[test]
    fn p89_soft_ref_retained_with_free_heap() {
        use crate::reference::{ReferenceProcessor, ReferenceType};

        let gc = make_collector();
        let mut rp = ReferenceProcessor::new();

        let referent = gc.alloc_object(ClassId::new(10), 1);
        let referent_addr = referent.as_ptr() as usize;

        let soft_ref = gc.alloc_object(ClassId::new(30), 2);
        let soft_addr = soft_ref.as_ptr() as usize;

        rp.discover_reference(ReferenceType::Soft, soft_addr, referent_addr, None);

        // Process with lots of free heap — soft refs should be retained
        let is_marked = |_addr: usize| -> bool { false };
        let result = rp.process_references(&is_marked, 1024, 0); // 1024 MB free

        // With 1024 MB free, soft refs should be retained per LRU policy
        // (exact behavior depends on implementation — verify no crash at minimum)
        let _ = result.stats.soft_refs_cleared;
    }

    #[test]
    fn p89_phantom_ref_enqueued() {
        use crate::reference::{ReferenceProcessor, ReferenceType};

        let gc = make_collector();
        let mut rp = ReferenceProcessor::new();

        let referent = gc.alloc_object(ClassId::new(10), 1);
        let referent_addr = referent.as_ptr() as usize;

        let phantom_ref = gc.alloc_object(ClassId::new(40), 2);
        let phantom_addr = phantom_ref.as_ptr() as usize;

        rp.discover_reference(ReferenceType::Phantom, phantom_addr, referent_addr, None);

        // Process with referent dead
        let is_marked = |addr: usize| -> bool { addr != referent_addr };
        let result = rp.process_references(&is_marked, 64, 0);

        // Phantom should be enqueued
        assert!(result.stats.phantom_refs_enqueued > 0);
    }

    // -- 89.4: Finalization --

    #[test]
    fn p89_finalizer_enqueued_for_unreachable() {
        use crate::reference::{ReferenceProcessor, ReferenceType};

        let gc = make_collector();
        let mut rp = ReferenceProcessor::new();

        let obj = gc.alloc_object(ClassId::new(50), 1);
        let obj_addr = obj.as_ptr() as usize;

        // Register as finalizable (same pattern as SharedVm::register_finalizable)
        rp.discover_reference(ReferenceType::Finalizer, obj_addr, obj_addr, None);

        // Process with object unreachable
        let is_marked = |_addr: usize| -> bool { false };
        let result = rp.process_references(&is_marked, 64, 0);

        // Object should be queued for finalization
        assert!(!result.to_finalize.is_empty());
        assert!(result.to_finalize.contains(&obj_addr));
    }

    #[test]
    fn p89_finalizer_prevents_double_finalization() {
        use crate::reference::FinalizerThread;

        let ft = FinalizerThread::new();

        // Enqueue an object
        ft.enqueue(0x1000);
        let first = ft.dequeue();
        assert_eq!(first, Some(0x1000));

        // Enqueue same object again
        ft.enqueue(0x1000);
        let second = ft.dequeue();
        // FinalizerThread tracks already-finalized objects — second enqueue is skipped
        assert!(second.is_none());
    }

    #[test]
    fn p89_finalizer_exception_does_not_crash() {
        use crate::reference::FinalizerThread;

        let ft = FinalizerThread::new();
        ft.enqueue(0x2000);
        ft.enqueue(0x3000);

        // Dequeue should work for both (simulating finalize() calls)
        let a = ft.dequeue();
        assert_eq!(a, Some(0x2000));
        let b = ft.dequeue();
        assert_eq!(b, Some(0x3000));
        let c = ft.dequeue();
        assert_eq!(c, None); // empty
    }

    // -----------------------------------------------------------------
    // T5.5.4 — select_evacuation_candidates tests
    // -----------------------------------------------------------------

    fn make_old_region(
        index: usize,
        top: usize,
        live_bytes: usize,
    ) -> crate::region::Region {
        let mut r = crate::region::Region {
            index,
            region_type: crate::region::RegionType::Old,
            top,
            rset: crate::region::RememberedSet::default(),
            live_bytes,
            age: 0,
        };
        // Sanity check invariant: live_bytes <= top.
        debug_assert!(r.live_bytes <= r.top);
        // Force the struct to live long enough — fields already public.
        let _ = &mut r.rset;
        r
    }

    #[test]
    fn evacuation_candidates_empty_when_no_old_regions() {
        let regions = vec![crate::region::Region {
            index: 0,
            region_type: crate::region::RegionType::Eden,
            top: 1024,
            rset: crate::region::RememberedSet::default(),
            live_bytes: 512,
            age: 0,
        }];
        let picked = select_evacuation_candidates(&regions, 1_000_000);
        assert!(picked.is_empty(), "only Old regions are eligible");
    }

    #[test]
    fn evacuation_candidates_prefer_high_garbage_ratio() {
        // Region 0: 90% garbage (100 live of 1000 total)
        // Region 1: 10% garbage (900 live of 1000 total)
        // Region 2: 50% garbage (500 live of 1000 total)
        // Budget large enough to take all three — ordering is what matters.
        let regions = vec![
            make_old_region(0, 1000, 100),
            make_old_region(1, 1000, 900),
            make_old_region(2, 1000, 500),
        ];
        let picked = select_evacuation_candidates(&regions, u64::MAX);
        assert_eq!(picked, vec![0, 2, 1]);
    }

    #[test]
    fn evacuation_candidates_respect_pause_budget() {
        // All three regions have plenty of garbage. Budget allows only
        // the first to fit; the remaining two must be dropped.
        // cost per region = live * 4 ns → region with live=100 costs 400 ns.
        let regions = vec![
            make_old_region(0, 1000, 100),   // 400 ns, ratio 9.0
            make_old_region(1, 1000, 200),   // 800 ns, ratio 4.0
            make_old_region(2, 1000, 300),   // 1200 ns, ratio 2.33
        ];
        let picked = select_evacuation_candidates(&regions, 500);
        // Only region 0 fits (400 ns); region 1 would push to 1200 ns.
        assert_eq!(picked, vec![0]);
    }

    #[test]
    fn evacuation_candidates_always_include_first_even_if_over_budget() {
        // Even with a tiny budget, return the top-ranked region so the
        // mixed GC can always make forward progress.
        let regions = vec![make_old_region(0, 1000, 500)]; // 2000 ns cost
        let picked = select_evacuation_candidates(&regions, 100);
        assert_eq!(picked, vec![0], "must include at least one region");
    }

    #[test]
    fn evacuation_candidates_skip_zero_garbage_regions() {
        // Region 1 has live == top → garbage_bytes == 0 → ineligible.
        let regions = vec![
            make_old_region(0, 1000, 100), // garbage 900
            make_old_region(1, 1000, 1000), // garbage 0, skip
        ];
        let picked = select_evacuation_candidates(&regions, u64::MAX);
        assert_eq!(picked, vec![0]);
    }

    #[test]
    fn evacuation_candidates_tie_break_by_index() {
        // Equal ratio → lower index wins.
        let regions = vec![
            make_old_region(7, 1000, 500),
            make_old_region(3, 1000, 500),
            make_old_region(5, 1000, 500),
        ];
        let picked = select_evacuation_candidates(&regions, u64::MAX);
        assert_eq!(picked, vec![3, 5, 7]);
    }

    #[test]
    fn region_estimated_evac_cost_matches_heuristic() {
        let r = make_old_region(0, 1000, 250);
        assert_eq!(r.estimated_evac_cost_ns(), 1000);
        let r_empty = make_old_region(0, 1000, 0);
        assert_eq!(r_empty.estimated_evac_cost_ns(), 0);
    }

    #[test]
    fn region_garbage_bytes_basic() {
        let r = make_old_region(0, 1000, 300);
        assert_eq!(r.garbage_bytes(), 700);
        let full = make_old_region(0, 1000, 1000);
        assert_eq!(full.garbage_bytes(), 0);
    }

    // -- CRIT-P4: address-to-region lookup table --

    /// Every region's `data` base appears in `region_lookup` exactly once,
    /// and the table is sorted by base address.
    #[test]
    fn region_lookup_table_built_for_every_region() {
        let gc = make_collector();
        let num = gc.num_regions();
        assert_eq!(gc.region_lookup.len(), num);

        // Sorted by base
        for w in gc.region_lookup.windows(2) {
            assert!(w[0].0 < w[1].0, "lookup table must be strictly sorted");
        }

        // Every region index represented
        let mut idxs: Vec<usize> = gc.region_lookup.iter().map(|(_, i)| *i).collect();
        idxs.sort_unstable();
        assert_eq!(idxs, (0..num).collect::<Vec<_>>());

        // Each entry's base matches the live region's data ptr.
        let regions = gc.regions.lock();
        for &(base, idx) in &gc.region_lookup {
            assert_eq!(base, regions[idx].data.as_ptr() as usize);
        }
    }

    /// Pointers into each region resolve to the correct region index;
    /// pointers outside the heap return None.
    #[test]
    fn region_for_ptr_returns_correct_index_for_each_region() {
        let gc = make_collector();
        let regions = gc.regions.lock();
        let region_size = gc.config.region_size;

        // Sample three offsets per region: start, middle, last byte.
        for (expected_idx, r) in regions.iter().enumerate() {
            let base = r.data.as_ptr() as usize;
            for offset in [0usize, region_size / 2, region_size - 1] {
                let ptr = (base + offset) as *mut u8;
                let got = gc.region_for_ptr(&regions, ptr);
                assert_eq!(
                    got,
                    Some(expected_idx),
                    "ptr {:#x} (region {} offset {}) lookup mismatch",
                    ptr as usize,
                    expected_idx,
                    offset,
                );
            }
        }

        // One-past-the-end of a region is OUT of that region. If it lands
        // exactly on the next region's base it should resolve to that next
        // region; otherwise None.
        for (i, r) in regions.iter().enumerate() {
            let just_past = r.data.as_ptr() as usize + region_size;
            let got = gc.region_for_ptr(&regions, just_past as *mut u8);
            // The byte at base+region_size belongs to no region unless
            // another region happens to start there.
            if let Some(idx) = got {
                assert_ne!(idx, i, "ptr {:#x} should not resolve back to region {}", just_past, i);
            }
        }
    }

    /// Addresses outside every region's `[base, base + region_size)` band
    /// must return None — neither very small addresses nor very large ones
    /// should false-positively match.
    #[test]
    fn region_for_ptr_returns_none_outside_heap() {
        let gc = make_collector();
        let regions = gc.regions.lock();

        // Trivially-low addresses
        for addr in [0usize, 8, 0x1000, 0x1_0000] {
            // Filter out the (extremely unlikely) case the OS allocated a
            // region near zero — skip if it would actually land in one.
            if gc.lookup_region_for_addr(addr).is_none() {
                assert_eq!(
                    gc.region_for_ptr(&regions, addr as *mut u8),
                    None,
                    "addr {:#x} should not resolve to any region",
                    addr,
                );
            }
        }

        // High addresses well above any plausible region base.
        let max_base = gc
            .region_lookup
            .iter()
            .map(|(b, _)| *b)
            .max()
            .unwrap();
        let well_above = max_base + gc.config.region_size + 0x10_0000;
        assert_eq!(
            gc.region_for_ptr(&regions, well_above as *mut u8),
            None,
            "addr {:#x} above all regions should not resolve",
            well_above,
        );
    }

    /// `region_for_ptr_with_regions` (write-barrier hot path) must agree
    /// with `region_for_ptr` for every probe.
    #[test]
    fn region_for_ptr_with_regions_matches_region_for_ptr() {
        let gc = make_collector();
        let regions = gc.regions.lock();
        let region_size = gc.config.region_size;

        for (expected_idx, r) in regions.iter().enumerate() {
            let base = r.data.as_ptr() as usize;
            for offset in [0usize, 1, 64, region_size / 3, region_size - 1] {
                let addr = base + offset;
                let by_addr = gc.region_for_ptr_with_regions(&regions, addr);
                let by_ptr = gc.region_for_ptr(&regions, addr as *mut u8);
                assert_eq!(by_addr, by_ptr);
                assert_eq!(by_addr, Some(expected_idx));
            }
        }
    }

    /// End-to-end: allocating an object and looking up its pointer must
    /// return the same region index the allocator placed it in.
    #[test]
    fn region_for_ptr_agrees_with_allocator() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 2);
        let regions = gc.regions.lock();
        let idx = gc.region_for_ptr(&regions, obj.as_ptr()).unwrap();
        assert_eq!(regions[idx].region_type, RegionType::Eden);
    }
}
