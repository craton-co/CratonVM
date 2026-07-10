// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! G1-style region-based garbage collector.
//!
//! Divides the heap into equal-sized regions (default 1 MB) that can be
//! independently classified as Eden, Survivor, Old, or Free. Collection
//! is incremental: young-gen regions are collected frequently with short
//! STW pauses, while old-gen regions are collected in mixed GCs guided
//! by concurrent marking liveness data.
//!
//! Key ideas from G1:
//! - **Region classification:** each region has a type (Eden, Survivor, Old, Humongous, Free)
//! - **Remembered sets (RSet):** per-region card sets tracking incoming cross-region references
//! - **Collection set (CSet):** the subset of regions to evacuate in a given GC pause
//! - **Mixed GC:** after concurrent marking, collect some old regions alongside young regions
//! - **Humongous objects:** objects larger than half a region span contiguous regions

use std::collections::{HashMap, HashSet};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::heap::{
    array_data_size, ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE, SLOT_SIZE,
};
use crate::mark_bitmap::MarkBitmap;

// ---------------------------------------------------------------------------
// Region configuration
// ---------------------------------------------------------------------------

/// Default region size: 1 MB
pub const DEFAULT_REGION_SIZE: usize = 1024 * 1024;

/// Minimum region size: 256 KB
pub const MIN_REGION_SIZE: usize = 256 * 1024;

/// Objects larger than this fraction of region size are humongous.
const HUMONGOUS_THRESHOLD_FRACTION: usize = 2; // 1/2 of region size

// ---------------------------------------------------------------------------
// Region types
// ---------------------------------------------------------------------------

/// Classification of a heap region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionType {
    /// Newly allocated objects go here.
    Eden,
    /// Survived one young GC — one more collection before promotion to Old.
    Survivor,
    /// Long-lived objects.
    Old,
    /// Start of a humongous allocation (object > region_size/2).
    HumongousStart,
    /// Continuation of a humongous allocation.
    HumongousContinuation,
    /// Available for allocation.
    Free,
}

// ---------------------------------------------------------------------------
// Remembered set
// ---------------------------------------------------------------------------

/// Per-region remembered set: tracks which other regions hold references into this one.
///
/// Stored as a set of (source_region_index, card_offset) pairs. This is a
/// simplified hash-set RSet — production G1 uses a multi-level structure
/// (sparse → fine → coarse) for space efficiency.
///
/// Round-9 gc CRIT-8 (post_write_barrier_rset hot path): `sources` lives
/// behind a `parking_lot::Mutex` so that `add_reference` takes `&self`
/// and can be called from the per-thread cache fast path in
/// `G1Collector::post_write_barrier_rset` without re-acquiring the
/// heap-wide `regions` mutex. The Vec backing the G1 regions is created
/// once with all entries and never reallocated, so a cached `*const
/// G1Region` (and therefore `*const RememberedSet`) is stable for the
/// lifetime of the mutator phase.
#[derive(Debug, Default)]
pub struct RememberedSet {
    /// Set of (source_region_index) that have references into this region.
    /// T10.9.B: FxHashSet — source region indices are internal.
    ///
    /// `parking_lot::Mutex` is uncontended-fast and lets the write
    /// barrier mutate via `&self`. The RSet for any given target
    /// region is only contended when many mutator threads
    /// simultaneously store cross-region refs whose target happens to
    /// land in that one region — a low-frequency case compared with
    /// per-thread same-region successive stores (handled by the
    /// caller's TLS pointer cache without touching this mutex at all).
    sources: parking_lot::Mutex<FxHashSet<usize>>,
}

impl RememberedSet {
    /// Record that `source_region` has a reference into this region.
    pub fn add_reference(&self, source_region: usize) {
        self.sources.lock().insert(source_region);
    }

    /// Clear the remembered set.
    pub fn clear(&self) {
        self.sources.lock().clear();
    }

    /// Number of distinct source regions.
    pub fn source_count(&self) -> usize {
        self.sources.lock().len()
    }

    /// Snapshot the source region indices.
    ///
    /// Returns an owned `Vec` so the caller does not need to hold the
    /// internal mutex across iteration (and so the signature does not
    /// leak a `MutexGuard` lifetime).
    pub fn sources(&self) -> Vec<usize> {
        self.sources.lock().iter().copied().collect()
    }
}

// ---------------------------------------------------------------------------
// Region descriptor
// ---------------------------------------------------------------------------

/// Metadata for a single heap region.
#[derive(Debug)]
pub struct Region {
    /// Region index (0-based).
    pub index: usize,
    /// Current type of this region.
    pub region_type: RegionType,
    /// Bump pointer: next free byte offset within this region.
    pub top: usize,
    /// Remembered set: incoming cross-region references.
    pub rset: RememberedSet,
    /// Number of live bytes (set during marking).
    pub live_bytes: usize,
    /// GC age: number of young GC cycles survived (for Survivor regions).
    pub age: u32,
}

impl Region {
    fn new(index: usize) -> Self {
        Self {
            index,
            region_type: RegionType::Free,
            top: 0,
            rset: RememberedSet::default(),
            live_bytes: 0,
            age: 0,
        }
    }

    /// Returns remaining free bytes in this region.
    fn _remaining(&self, region_size: usize) -> usize {
        region_size.saturating_sub(self.top)
    }

    /// Reset this region to Free state.
    fn reset(&mut self) {
        self.region_type = RegionType::Free;
        self.top = 0;
        self.rset.clear();
        self.live_bytes = 0;
        self.age = 0;
    }

    /// T5.5.4 — Estimate the CPU cost (nanoseconds) of evacuating this
    /// region during a G1 mixed GC.
    ///
    /// The evacuation work is dominated by copying live data to the
    /// destination region and updating references. On contemporary
    /// hardware copying roughly tracks 4 ns per live byte (≈250 MB/s
    /// effective, accounting for write-barrier overhead and reference
    /// rewriting per slot). That's the heuristic used here: no
    /// instrumentation feedback is available in-process, so the
    /// estimate is purely data-driven.
    ///
    /// Callers should treat the result as a relative ranking signal,
    /// not an absolute wall-clock prediction.
    pub fn estimated_evac_cost_ns(&self) -> u64 {
        const NS_PER_LIVE_BYTE: u64 = 4;
        (self.live_bytes as u64).saturating_mul(NS_PER_LIVE_BYTE)
    }

    /// T5.5.4 — Garbage bytes estimated for this region.
    ///
    /// Uses the bump pointer (`top`) as the upper-bound size of
    /// allocated data. `garbage = top - live_bytes`.
    pub fn garbage_bytes(&self) -> usize {
        self.top.saturating_sub(self.live_bytes)
    }
}

// ---------------------------------------------------------------------------
// G1-style region heap
// ---------------------------------------------------------------------------

/// G1-style region-based heap — UNUSED PROTOTYPE, do not wire up.
///
/// The heap is divided into `num_regions` equal-sized regions. Allocation
/// goes through a current allocation region (bump pointer). Collection
/// selects a collection set (CSet) and evacuates live objects.
///
/// G1CORE-11: this type has no production consumer (the real region-based
/// collector is `G1Collector` in `g1.rs`, reached through `VmHeap::G1`) and
/// its evacuation rewrites reference slots CONSERVATIVELY — any 8-byte word
/// that numerically equals a moved object's old address is rewritten, so an
/// integer field that happens to alias a heap address is silently corrupted.
/// It is kept only as reference material for its unit tests; it is not
/// re-exported from the crate root.
#[deprecated(
    note = "unused prototype with unsound conservative slot rewriting — use G1Collector (VmHeap::G1) instead"
)]
pub struct RegionHeap {
    /// Contiguous backing storage.
    data: Vec<u8>,
    /// Region size in bytes.
    region_size: usize,
    /// Total number of regions.
    num_regions: usize,
    /// Per-region metadata.
    regions: Vec<Region>,
    /// Current Eden allocation region index (None if none available).
    current_eden: Option<usize>,
    /// Total bytes allocated (across all regions).
    allocated_bytes: usize,
    /// Tenuring threshold: number of young GCs an object must survive.
    tenuring_threshold: u32,
    /// Mark bitmap for concurrent marking.
    pub bitmap: MarkBitmap,
}

#[allow(deprecated)] // G1CORE-11: the type itself is deprecated; its impl stays for the tests.
impl RegionHeap {
    /// Create a new region heap with the given total capacity and region size.
    pub fn new(total_capacity: usize, region_size: usize) -> Self {
        let region_size = region_size.max(MIN_REGION_SIZE);
        let num_regions = total_capacity / region_size;
        assert!(
            num_regions > 0,
            "capacity must accommodate at least one region"
        );

        let actual_capacity = num_regions * region_size;
        let data = vec![0u8; actual_capacity];
        let regions: Vec<Region> = (0..num_regions).map(Region::new).collect();

        let base_addr = data.as_ptr() as usize;
        let bitmap = MarkBitmap::new(base_addr, actual_capacity);

        Self {
            data,
            region_size,
            num_regions,
            regions,
            current_eden: None,
            allocated_bytes: 0,
            tenuring_threshold: 3,
            bitmap,
        }
    }

    /// Create with default region size.
    pub fn with_capacity(total_capacity: usize) -> Self {
        Self::new(total_capacity, DEFAULT_REGION_SIZE)
    }

    /// Get the base address of the backing storage.
    pub fn base_addr(&self) -> usize {
        self.data.as_ptr() as usize
    }

    /// Get the start address of a region.
    pub fn region_start(&self, region_index: usize) -> usize {
        self.base_addr() + region_index * self.region_size
    }

    /// Get the region index for an address.
    pub fn region_index_for(&self, addr: usize) -> Option<usize> {
        let base = self.base_addr();
        if addr < base {
            return None;
        }
        let offset = addr - base;
        let idx = offset / self.region_size;
        if idx < self.num_regions {
            Some(idx)
        } else {
            None
        }
    }

    /// Allocate an object in Eden.
    pub fn alloc_eden(&mut self, size: usize, align: usize) -> Option<*mut u8> {
        // Humongous check
        if size > self.region_size / HUMONGOUS_THRESHOLD_FRACTION {
            return self.alloc_humongous(size, align);
        }

        // Try current Eden region
        if let Some(idx) = self.current_eden {
            if let Some(ptr) = self.bump_alloc_in(idx, size, align) {
                self.allocated_bytes += size;
                return Some(ptr);
            }
        }

        // Find a new Free region for Eden
        if let Some(idx) = self.find_free_region() {
            self.regions[idx].region_type = RegionType::Eden;
            self.current_eden = Some(idx);
            if let Some(ptr) = self.bump_alloc_in(idx, size, align) {
                self.allocated_bytes += size;
                return Some(ptr);
            }
        }

        None
    }

    /// Allocate a humongous object spanning multiple contiguous regions.
    fn alloc_humongous(&mut self, size: usize, align: usize) -> Option<*mut u8> {
        let regions_needed = size.div_ceil(self.region_size);

        // Find contiguous free regions
        let start = self.find_contiguous_free(regions_needed)?;

        // Mark them
        self.regions[start].region_type = RegionType::HumongousStart;
        for i in 1..regions_needed {
            self.regions[start + i].region_type = RegionType::HumongousContinuation;
        }

        let base = self.base_addr();
        let region_start_addr = base + start * self.region_size;
        let aligned_addr = (region_start_addr + align - 1) & !(align - 1);
        let ptr = aligned_addr as *mut u8;

        self.regions[start].top = size.min(self.region_size);
        for i in 1..regions_needed {
            let remaining = size - i * self.region_size;
            self.regions[start + i].top = remaining.min(self.region_size);
        }

        self.allocated_bytes += size;

        // Round-5 #14 — no per-allocation memset. Free regions are
        // already zeroed at two well-defined points:
        //   1. `RegionHeap::new()` — initial `vec![0u8; capacity]`.
        //   2. `evacuate()` — `write_bytes(.., 0, top)` on every region
        //      that returns to Free state at the end of a young/mixed GC.
        // Any byte handed out by an allocator path is therefore already
        // zero. The previous explicit `write_bytes` here was redundant
        // and costly for humongous objects (multi-MB zero pass twice).
        //
        // Round-9 gc CRIT-1: install a HumongousFiller sentinel at the
        // start of every continuation region so walkers iterate it as a
        // single dark region instead of decoding zeroed bytes as a chain
        // of phantom Object headers. The first region (`start`) holds the
        // real application header at the aligned offset and must not be
        // touched here.
        for i in 1..regions_needed {
            let region_addr = base + (start + i) * self.region_size;
            if self.regions[start + i].top >= HEADER_SIZE {
                let filler = ObjectHeader::new(
                    cratonvm_types::ClassId::new(0),
                    ObjectKind::HumongousFiller,
                    ArrayElementType::Reference,
                    0,
                    0,
                    0,
                );
                // SAFETY: region_addr is the base of a continuation region
                // we just claimed; the first HEADER_SIZE bytes are exclusive
                // to this allocation.
                unsafe {
                    std::ptr::write(region_addr as *mut ObjectHeader, filler);
                }
            }
        }

        Some(ptr)
    }

    /// Bump-allocate within a specific region.
    fn bump_alloc_in(&mut self, region_idx: usize, size: usize, align: usize) -> Option<*mut u8> {
        let region_start = self.region_start(region_idx);
        let current_addr = region_start + self.regions[region_idx].top;
        let aligned_addr = (current_addr + align - 1) & !(align - 1);
        let end = aligned_addr + size;
        let region_end = region_start + self.region_size;

        if end > region_end {
            return None;
        }

        self.regions[region_idx].top = end - region_start;
        let ptr = aligned_addr as *mut u8;
        // Round-5 #14 — no per-allocation memset. Region bytes are zero
        // at acquisition (see `alloc_humongous` for the full reasoning);
        // adding a per-bump zero on top of that was pure overhead. This
        // bump path is on the hot allocation critical section, so the
        // saved write back-pressure shows up immediately for short-lived
        // objects.
        Some(ptr)
    }

    /// Find the first free region.
    fn find_free_region(&self) -> Option<usize> {
        self.regions
            .iter()
            .position(|r| r.region_type == RegionType::Free)
    }

    /// Find `count` contiguous free regions.
    fn find_contiguous_free(&self, count: usize) -> Option<usize> {
        let mut run_start = 0;
        let mut run_len = 0;

        for i in 0..self.num_regions {
            if self.regions[i].region_type == RegionType::Free {
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

    /// Count regions by type.
    pub fn count_regions(&self, region_type: RegionType) -> usize {
        self.regions
            .iter()
            .filter(|r| r.region_type == region_type)
            .count()
    }

    /// Get the total number of regions.
    pub fn num_regions(&self) -> usize {
        self.num_regions
    }

    /// Get the region size.
    pub fn region_size(&self) -> usize {
        self.region_size
    }

    /// Total bytes allocated.
    pub fn allocated_bytes(&self) -> usize {
        self.allocated_bytes
    }

    /// Total heap capacity.
    pub fn capacity(&self) -> usize {
        self.data.len()
    }

    /// Check if an address is within the heap.
    pub fn contains(&self, addr: usize) -> bool {
        let base = self.base_addr();
        addr >= base && addr < base + self.data.len()
    }

    /// Get region metadata (read-only).
    pub fn region(&self, index: usize) -> &Region {
        &self.regions[index]
    }

    // -----------------------------------------------------------------------
    // Write barrier (cross-region reference tracking)
    // -----------------------------------------------------------------------

    /// Write barrier: track cross-region references.
    ///
    /// Called after storing a reference `target_addr` into an object in
    /// `source_addr`. If the reference crosses region boundaries, record it
    /// in the target region's remembered set.
    pub fn write_barrier(&mut self, source_addr: usize, target_addr: usize) {
        if let (Some(src_idx), Some(tgt_idx)) = (
            self.region_index_for(source_addr),
            self.region_index_for(target_addr),
        ) {
            if src_idx != tgt_idx {
                self.regions[tgt_idx].rset.add_reference(src_idx);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Young GC (evacuate Eden + Survivor regions)
    // -----------------------------------------------------------------------

    /// Select the collection set for a young GC.
    ///
    /// Returns indices of all Eden and Survivor regions.
    pub fn young_collection_set(&self) -> Vec<usize> {
        self.regions
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                r.region_type == RegionType::Eden || r.region_type == RegionType::Survivor
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Select the collection set for a mixed GC.
    ///
    /// Returns all young regions plus old regions sorted by garbage ratio
    /// (most garbage first), limited to `max_old_regions`.
    pub fn mixed_collection_set(&self, max_old_regions: usize) -> Vec<usize> {
        let mut cset = self.young_collection_set();

        // Sort old regions by garbage ratio (live_bytes ascending = most garbage first)
        let mut old_regions: Vec<(usize, usize)> = self
            .regions
            .iter()
            .enumerate()
            .filter(|(_, r)| r.region_type == RegionType::Old)
            .map(|(i, r)| (i, r.live_bytes))
            .collect();
        old_regions.sort_by_key(|(_, live)| *live);

        for (idx, _) in old_regions.into_iter().take(max_old_regions) {
            cset.push(idx);
        }

        cset
    }

    /// Evacuate live objects from the collection set.
    ///
    /// Walks objects in each CSet region, copies live objects to a Survivor or
    /// Old region, and returns a forwarding map (old_addr → new_addr).
    ///
    /// `roots` are updated in place. `is_live` is a closure that returns true
    /// if the object at the given address should be evacuated (e.g., marked in bitmap).
    /// For young GC, all objects in CSet are live if reachable from roots + RSets.
    pub fn evacuate<F>(
        &mut self,
        cset: &[usize],
        roots: &mut [usize],
        is_live: F,
    ) -> HashMap<usize, usize>
    where
        F: Fn(usize) -> bool,
    {
        let mut forwarding: HashMap<usize, usize> = HashMap::new();
        let cset_set: HashSet<usize> = cset.iter().copied().collect();

        for &region_idx in cset {
            let region = &self.regions[region_idx];
            if region.top == 0 {
                continue;
            }
            let region_start = self.region_start(region_idx);
            let region_age = region.age;
            let was_old = region.region_type == RegionType::Old;

            // Walk objects in this region
            let mut offset = 0usize;
            while offset < self.regions[region_idx].top {
                let obj_addr = region_start + offset;
                let header = unsafe { &*(obj_addr as *const ObjectHeader) };
                let obj_size = object_total_size(header);

                if obj_size < HEADER_SIZE || offset + obj_size > self.regions[region_idx].top {
                    break;
                }

                if is_live(obj_addr) {
                    // Decide destination: promote to Old if age >= threshold, else Survivor
                    let promote = was_old || region_age + 1 >= self.tenuring_threshold;
                    let dest_type = if promote {
                        RegionType::Old
                    } else {
                        RegionType::Survivor
                    };

                    if let Some(dest_ptr) = self.alloc_in_type(dest_type, obj_size, 8) {
                        let dest_addr = dest_ptr as usize;
                        // Round-5 #10 / round-7 #10: split the bulk memcpy
                        // so the destination header is published as a single
                        // atomic-sized store *after* the data area is in
                        // place. A concurrent scanner that observes the
                        // dest header during a bulk memcpy could otherwise
                        // see a half-written `forwarding_ptr` (a *mut u8
                        // value that straddles the memcpy boundary). By
                        // copying the data tail first and then writing the
                        // header struct in one shot, the header transition
                        // from "stale" to "fully initialized" is a single
                        // 40-byte aligned write — and the `forwarding_ptr`
                        // field within it is a single naturally-aligned
                        // pointer-sized store.
                        unsafe {
                            // 1) Copy data area (bytes after the header).
                            if obj_size > HEADER_SIZE {
                                std::ptr::copy_nonoverlapping(
                                    (obj_addr + HEADER_SIZE) as *const u8,
                                    dest_ptr.add(HEADER_SIZE),
                                    obj_size - HEADER_SIZE,
                                );
                            }
                            // ----------------------------------------------
                            // CRIT (round-5 GC #3, ARM ordering):
                            //
                            // Inject a Release fence between the data-area
                            // memcpy and the subsequent header-field stores.
                            //
                            // The "data-then-header publication" pattern
                            // relies on a concurrent scanner observing the
                            // *header* (with Acquire ordering on
                            // `forwarding_ptr`) only after the *data area*
                            // is in place. On TSO (x86) the per-byte stores
                            // from `copy_nonoverlapping` retire in program
                            // order, so this happens to work without a
                            // fence. On weakly-ordered architectures (ARM,
                            // POWER, RISC-V), the CPU and the compiler are
                            // free to reorder the data-area stores past the
                            // header writes that publish the destination
                            // object's `forwarding_ptr`. A concurrent
                            // scanner that performs an Acquire load of
                            // `forwarding_ptr`, observes the new value,
                            // then dereferences fields of the new object
                            // would race against the still-in-flight
                            // data-area stores.
                            //
                            // The Release fence pairs with the Acquire load
                            // of `forwarding_ptr` on the scanner side:
                            //   Writer:  data memcpy ; FENCE(Release) ; store header
                            //   Reader:  load header (Acquire) ; read data
                            //
                            // The Acquire on `forwarding_ptr` is the
                            // synchronisation point that observes everything
                            // sequenced before the matching Release here.
                            // ----------------------------------------------
                            std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
                            // 2) Copy the header as the last step so any
                            //    concurrent reader of `dest_ptr` either
                            //    sees zeroed bytes (allocation zero-init
                            //  performed by `bump_alloc_in`) or the
                            //  finished header — never a torn one.
                            //    `*ObjectHeader` is `#[repr(C)]` with a
                            //    forwarding_ptr field at a fixed offset;
                            //    a plain struct copy is atomic-enough for
                            //    each field individually under STW. The
                            //    mark_word stays untouched at offset 32
                            //    (AtomicU64) — re-published below.
                            let src_hdr = obj_addr as *const ObjectHeader;
                            let dst_hdr = dest_ptr as *mut ObjectHeader;
                            // Manually mirror non-atomic header fields to
                            // avoid bulk-copying the AtomicU64 mark_word.
                            (*dst_hdr).class_id = (*src_hdr).class_id;
                            (*dst_hdr).kind = (*src_hdr).kind;
                            (*dst_hdr).element_type = (*src_hdr).element_type;
                            (*dst_hdr)._padding = (*src_hdr)._padding;
                            (*dst_hdr).identity_hash_code = (*src_hdr).identity_hash_code;
                            (*dst_hdr).array_length = (*src_hdr).array_length;
                            (*dst_hdr).num_slots = (*src_hdr).num_slots;
                            (*dst_hdr).gc_age = (*src_hdr).gc_age;
                            (*dst_hdr).gc_flags = (*src_hdr).gc_flags;
                            (*dst_hdr)._gc_reserved = (*src_hdr)._gc_reserved;
                            // `forwarding_ptr` published as a single naturally-aligned
                            // pointer-sized store — no torn read possible.
                            (*dst_hdr).forwarding_ptr = (*src_hdr).forwarding_ptr;
                            // Re-publish the atomic mark_word last.
                            let mark = (*src_hdr)
                                .mark_word
                                .load(std::sync::atomic::Ordering::Relaxed);
                            (*dst_hdr)
                                .mark_word
                                .store(mark, std::sync::atomic::Ordering::Relaxed);
                        }
                        forwarding.insert(obj_addr, dest_addr);
                    }
                    // If allocation fails (heap full), object is lost — in production
                    // this would trigger a full GC.
                }

                offset += obj_size;
            }
        }

        // Update roots
        for root in roots.iter_mut() {
            if let Some(&new_addr) = forwarding.get(root) {
                *root = new_addr;
            }
        }

        // Free evacuated regions
        for &region_idx in cset {
            if cset_set.contains(&region_idx) {
                let region_start = self.region_start(region_idx);
                let top = self.regions[region_idx].top;
                self.allocated_bytes = self.allocated_bytes.saturating_sub(top);
                // Zero the region
                unsafe {
                    std::ptr::write_bytes(region_start as *mut u8, 0, top);
                }
                self.regions[region_idx].reset();
            }
        }

        // Update current_eden if it was evacuated
        if let Some(eden) = self.current_eden {
            if cset_set.contains(&eden) {
                self.current_eden = None;
            }
        }

        forwarding
    }

    /// Allocate in a region of the given type, finding or creating one.
    fn alloc_in_type(
        &mut self,
        target_type: RegionType,
        size: usize,
        align: usize,
    ) -> Option<*mut u8> {
        // Try existing regions of this type
        for i in 0..self.num_regions {
            if self.regions[i].region_type == target_type {
                if let Some(ptr) = self.bump_alloc_in(i, size, align) {
                    self.allocated_bytes += size;
                    return Some(ptr);
                }
            }
        }

        // Allocate a new free region
        if let Some(idx) = self.find_free_region() {
            self.regions[idx].region_type = target_type;
            if target_type == RegionType::Survivor {
                self.regions[idx].age = 1;
            }
            if let Some(ptr) = self.bump_alloc_in(idx, size, align) {
                self.allocated_bytes += size;
                return Some(ptr);
            }
        }

        None
    }

    // -----------------------------------------------------------------------
    // Concurrent marking support
    // -----------------------------------------------------------------------

    /// Mark all objects reachable from roots in the given regions.
    ///
    /// Returns the number of objects marked. Updates `live_bytes` on each region.
    pub fn mark_live_objects(&mut self, roots: &[usize]) -> usize {
        self.bitmap.clear();

        // Reset live bytes
        for r in &mut self.regions {
            r.live_bytes = 0;
        }

        let mut mark_stack: Vec<usize> = Vec::new();
        let mut marked = 0usize;

        // Mark roots
        for &root_addr in roots {
            if self.contains(root_addr) && self.bitmap.try_mark(root_addr) {
                mark_stack.push(root_addr);
                marked += 1;
            }
        }

        // Trace
        while let Some(obj_addr) = mark_stack.pop() {
            let header = unsafe { &*(obj_addr as *const ObjectHeader) };
            let obj_size = object_total_size(header);

            // Update region live bytes
            if let Some(ridx) = self.region_index_for(obj_addr) {
                self.regions[ridx].live_bytes += obj_size;
            }

            // Scan fields for references
            let refs = scan_object_refs(obj_addr, header);
            for ref_addr in refs {
                if self.contains(ref_addr) && self.bitmap.try_mark(ref_addr) {
                    mark_stack.push(ref_addr);
                    marked += 1;
                }
            }
        }

        marked
    }

    /// Returns true if enough regions are full to warrant a GC.
    pub fn needs_gc(&self, threshold_pct: u32) -> bool {
        let used_regions = self
            .regions
            .iter()
            .filter(|r| r.region_type != RegionType::Free)
            .count();
        let pct = (used_regions * 100) / self.num_regions.max(1);
        pct as u32 >= threshold_pct
    }

    /// Number of free regions remaining.
    pub fn free_region_count(&self) -> usize {
        self.count_regions(RegionType::Free)
    }

    /// Update Survivor region ages. Promote any that exceed threshold.
    pub fn age_survivor_regions(&mut self) {
        for r in &mut self.regions {
            if r.region_type == RegionType::Survivor {
                r.age += 1;
                if r.age >= self.tenuring_threshold {
                    r.region_type = RegionType::Old;
                }
            }
        }
    }

    /// Update interior references in all non-free regions using a forwarding map.
    pub fn update_references(&mut self, forwarding: &HashMap<usize, usize>) {
        if forwarding.is_empty() {
            return;
        }

        for region_idx in 0..self.num_regions {
            let rtype = self.regions[region_idx].region_type;
            if rtype == RegionType::Free {
                continue;
            }
            let region_start = self.region_start(region_idx);
            let top = self.regions[region_idx].top;

            let mut offset = 0usize;
            while offset < top {
                let obj_addr = region_start + offset;
                let header = unsafe { &*(obj_addr as *const ObjectHeader) };
                let obj_size = object_total_size(header);

                if obj_size < HEADER_SIZE || offset + obj_size > top {
                    break;
                }

                // Update reference fields
                update_object_refs(obj_addr, header, forwarding);
                offset += obj_size;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Object helpers
// ---------------------------------------------------------------------------

/// Compute total object size from header.
///
/// Defensive corruption handling (gc-abort-cleanup, mirrors `gc.rs`): a corrupt /
/// implausible array header (e.g. a stale `array_length` so large that
/// `header + length * element_size` overflows `usize`) must NOT abort the whole
/// VM. This previously `.expect()`-panicked here, killing the process on a bad
/// header, whereas the non-moving sweep (`gen_heap.rs::gen_object_total_size`)
/// returns a `0` sentinel and lets its walker re-sync. Mirror that behavior: on
/// overflow, log a diagnostic and return `0`. `0 < HEADER_SIZE`, so every
/// caller's existing corruption guard treats it as a bad header and stops /
/// re-syncs the linear walk rather than advancing the cursor by 0 and spinning.
///
/// This does not mask genuine bugs silently — the corruption is logged — but it
/// converts a hard process abort into a recoverable / fail-safe path.
fn object_total_size(header: &ObjectHeader) -> usize {
    if header.kind == ObjectKind::Array {
        match array_data_size(header.array_length as usize, header.element_type) {
            Ok(data) => HEADER_SIZE + data,
            Err(_) => {
                // Implausible array header — treat as corrupt. Return 0 so the
                // caller's `total_size < HEADER_SIZE` guard fires (matching the
                // non-moving sweep's re-sync contract) instead of panicking.
                tracing::warn!(
                    "region: implausible array_length {} (element_type={:?}) in object header — \
                     treating as corrupt; caller will skip/stop the walk",
                    header.array_length,
                    header.element_type,
                );
                0
            }
        }
    } else {
        HEADER_SIZE + header.num_slots as usize * SLOT_SIZE
    }
}

/// Round-9 gc CRIT-1: returns true for the synthetic walker sentinel that
/// covers a humongous-continuation region. Walkers MUST break out of the
/// per-region iteration when they observe this.
#[inline]
fn is_humongous_filler(header: &ObjectHeader) -> bool {
    matches!(header.kind, ObjectKind::HumongousFiller)
}

/// Scan an object's reference fields, returning addresses of referenced objects.
fn scan_object_refs(obj_addr: usize, header: &ObjectHeader) -> Vec<usize> {
    let mut refs = Vec::new();
    let data_start = obj_addr + HEADER_SIZE;

    if header.kind == ObjectKind::Array {
        if header.element_type == ArrayElementType::Reference {
            let len = header.array_length as usize;
            for i in 0..len {
                let slot_addr = data_start + i * 8;
                let ptr = unsafe { *(slot_addr as *const usize) };
                if ptr != 0 {
                    refs.push(ptr);
                }
            }
        }
    } else {
        let num_slots = header.num_slots as usize;
        for i in 0..num_slots {
            let slot_addr = data_start + i * SLOT_SIZE;
            // Check if slot looks like a heap pointer (non-zero, aligned)
            // In practice, Value tags would be checked, but for GC scanning
            // we look at the raw bytes as potential references.
            let raw = unsafe { *(slot_addr as *const u64) };
            // Value::Object(Some(ref)) stores the pointer in the Value enum.
            // We use a simple heuristic: if the value has a reference tag,
            // extract the pointer. For simplicity, treat any aligned non-zero
            // value as a potential reference and let the bitmap filter.
            if raw != 0 && raw % 8 == 0 {
                refs.push(raw as usize);
            }
        }
    }

    refs
}

/// Update reference fields in an object using the forwarding map.
fn update_object_refs(obj_addr: usize, header: &ObjectHeader, forwarding: &HashMap<usize, usize>) {
    let data_start = obj_addr + HEADER_SIZE;

    if header.kind == ObjectKind::Array {
        if header.element_type == ArrayElementType::Reference {
            let len = header.array_length as usize;
            for i in 0..len {
                let slot_addr = data_start + i * 8;
                let ptr = unsafe { *(slot_addr as *const usize) };
                if let Some(&new_ptr) = forwarding.get(&ptr) {
                    unsafe {
                        *(slot_addr as *mut usize) = new_ptr;
                    }
                }
            }
        }
    } else {
        let num_slots = header.num_slots as usize;
        for i in 0..num_slots {
            let slot_addr = data_start + i * SLOT_SIZE;
            let raw = unsafe { *(slot_addr as *const usize) };
            if let Some(&new_ptr) = forwarding.get(&raw) {
                unsafe {
                    *(slot_addr as *mut usize) = new_ptr;
                }
            }
        }
    }
}

#[allow(deprecated)] // G1CORE-11
impl std::fmt::Debug for RegionHeap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegionHeap")
            .field("num_regions", &self.num_regions)
            .field("region_size", &self.region_size)
            .field("allocated_bytes", &self.allocated_bytes)
            .field("eden", &self.count_regions(RegionType::Eden))
            .field("survivor", &self.count_regions(RegionType::Survivor))
            .field("old", &self.count_regions(RegionType::Old))
            .field("free", &self.count_regions(RegionType::Free))
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(deprecated)] // G1CORE-11: tests exercise the deprecated prototype on purpose.
mod tests {
    use super::*;

    const SMALL_REGION: usize = MIN_REGION_SIZE; // 256 KB
    const HEAP_SIZE: usize = SMALL_REGION * 8; // 8 regions

    fn make_heap() -> RegionHeap {
        RegionHeap::new(HEAP_SIZE, SMALL_REGION)
    }

    #[test]
    fn new_heap_all_free() {
        let heap = make_heap();
        assert_eq!(heap.num_regions(), 8);
        assert_eq!(heap.region_size(), SMALL_REGION);
        assert_eq!(heap.count_regions(RegionType::Free), 8);
        assert_eq!(heap.allocated_bytes(), 0);
    }

    #[test]
    fn alloc_eden_basic() {
        let mut heap = make_heap();
        let ptr = heap.alloc_eden(64, 8).unwrap();
        assert!(!ptr.is_null());
        assert_eq!(heap.count_regions(RegionType::Eden), 1);
        assert_eq!(heap.allocated_bytes(), 64);
    }

    #[test]
    fn alloc_eden_multiple_same_region() {
        let mut heap = make_heap();
        let p1 = heap.alloc_eden(64, 8).unwrap();
        let p2 = heap.alloc_eden(128, 8).unwrap();
        assert_ne!(p1, p2);
        // Both should be in the same Eden region
        assert_eq!(heap.count_regions(RegionType::Eden), 1);
        assert_eq!(heap.allocated_bytes(), 192);
    }

    #[test]
    fn alloc_fills_region_spills_to_next() {
        let mut heap = make_heap();
        // Fill a region with many small allocations (staying under humongous threshold)
        let chunk = 1024; // 1 KB each
        let count = SMALL_REGION / chunk - 1; // nearly fill it
        for _ in 0..count {
            heap.alloc_eden(chunk, 8).unwrap();
        }
        assert_eq!(heap.count_regions(RegionType::Eden), 1);

        // Allocate more than the remaining space to force a new region
        heap.alloc_eden(chunk * 2, 8).unwrap();
        assert_eq!(heap.count_regions(RegionType::Eden), 2);
    }

    #[test]
    fn alloc_humongous() {
        let mut heap = make_heap();
        // Object larger than region_size/2
        let huge_size = SMALL_REGION / 2 + 1;
        let ptr = heap.alloc_eden(huge_size, 8).unwrap();
        assert!(!ptr.is_null());
        assert_eq!(heap.count_regions(RegionType::HumongousStart), 1);
    }

    #[test]
    fn alloc_humongous_multi_region() {
        let mut heap = make_heap();
        // Object spanning 2 full regions
        let huge_size = SMALL_REGION + 100;
        let ptr = heap.alloc_eden(huge_size, 8).unwrap();
        assert!(!ptr.is_null());
        assert_eq!(heap.count_regions(RegionType::HumongousStart), 1);
        assert_eq!(heap.count_regions(RegionType::HumongousContinuation), 1);
    }

    #[test]
    fn region_index_for_address() {
        let heap = make_heap();
        let base = heap.base_addr();
        assert_eq!(heap.region_index_for(base), Some(0));
        assert_eq!(heap.region_index_for(base + SMALL_REGION), Some(1));
        assert_eq!(
            heap.region_index_for(base + SMALL_REGION * 7 + 100),
            Some(7)
        );
        assert_eq!(heap.region_index_for(base + HEAP_SIZE), None);
        assert_eq!(heap.region_index_for(0), None);
    }

    #[test]
    fn write_barrier_cross_region() {
        let mut heap = make_heap();

        // Manually set up two Eden regions with allocations in each
        heap.regions[0].region_type = RegionType::Eden;
        heap.regions[0].top = 64;
        heap.regions[1].region_type = RegionType::Eden;
        heap.regions[1].top = 64;
        let p1 = heap.region_start(0) as *mut u8;
        let p2 = heap.region_start(1) as *mut u8;

        let addr1 = p1 as usize;
        let addr2 = p2 as usize;

        // These should be in different regions
        let r1 = heap.region_index_for(addr1).unwrap();
        let r2 = heap.region_index_for(addr2).unwrap();
        assert_ne!(r1, r2);

        // Write barrier: r1 stores ref to r2
        heap.write_barrier(addr1, addr2);
        assert_eq!(heap.regions[r2].rset.source_count(), 1);
    }

    #[test]
    fn write_barrier_same_region_no_rset() {
        let mut heap = make_heap();
        let p1 = heap.alloc_eden(64, 8).unwrap();
        let p2 = heap.alloc_eden(64, 8).unwrap();

        let r1 = heap.region_index_for(p1 as usize).unwrap();
        let r2 = heap.region_index_for(p2 as usize).unwrap();

        // Same region — no RSet entry
        if r1 == r2 {
            heap.write_barrier(p1 as usize, p2 as usize);
            assert_eq!(heap.regions[r1].rset.source_count(), 0);
        }
    }

    #[test]
    fn young_collection_set() {
        let mut heap = make_heap();
        // Manually set up regions
        heap.regions[0].region_type = RegionType::Eden;
        heap.regions[0].top = 100;
        heap.regions[1].region_type = RegionType::Eden;
        heap.regions[1].top = 200;
        heap.regions[5].region_type = RegionType::Survivor;
        heap.regions[6].region_type = RegionType::Old;

        let cset = heap.young_collection_set();
        assert!(cset.contains(&0));
        assert!(cset.contains(&1));
        assert!(cset.contains(&5));
        assert!(!cset.contains(&6));
    }

    #[test]
    fn mixed_collection_set() {
        let mut heap = make_heap();
        heap.regions[0].region_type = RegionType::Eden;
        heap.regions[0].top = 100;
        heap.regions[3].region_type = RegionType::Old;
        heap.regions[3].live_bytes = 100;
        heap.regions[4].region_type = RegionType::Old;
        heap.regions[4].live_bytes = 50; // more garbage

        let cset = heap.mixed_collection_set(1);
        // Should include Eden(0) and the most garbage Old region (4, fewer live bytes)
        assert!(cset.contains(&0));
        assert!(cset.contains(&4));
        // Region 3 not included (only 1 old region allowed)
        assert!(!cset.contains(&3));
    }

    #[test]
    fn evacuate_basic() {
        let mut heap = make_heap();

        // Allocate a small object with a proper header
        let obj_size = HEADER_SIZE + 2 * SLOT_SIZE;
        let ptr = heap.alloc_eden(obj_size, 8).unwrap();
        let obj_addr = ptr as usize;

        // Write header
        unsafe {
            let header = &mut *(ptr as *mut ObjectHeader);
            header.num_slots = 2;
            header.kind = ObjectKind::Object;
        }

        let eden_region = heap.region_index_for(obj_addr).unwrap();
        let cset = vec![eden_region];

        let mut roots = vec![obj_addr];
        let forwarding = heap.evacuate(&cset, &mut roots, |addr| addr == obj_addr);

        // Object should have been forwarded
        assert_eq!(forwarding.len(), 1);
        assert!(forwarding.contains_key(&obj_addr));

        // Root should be updated
        let new_addr = roots[0];
        assert_ne!(new_addr, obj_addr);
        assert_eq!(forwarding[&obj_addr], new_addr);

        // Original region should be free
        assert_eq!(heap.regions[eden_region].region_type, RegionType::Free);
    }

    #[test]
    fn needs_gc_threshold() {
        let mut heap = make_heap();
        assert!(!heap.needs_gc(50));

        // Use 5/8 regions (62.5%)
        for i in 0..5 {
            heap.regions[i].region_type = RegionType::Eden;
        }
        assert!(heap.needs_gc(60));
        assert!(!heap.needs_gc(70));
    }

    #[test]
    fn age_survivor_promotion() {
        let mut heap = make_heap();
        heap.regions[0].region_type = RegionType::Survivor;
        heap.regions[0].age = 2;
        heap.tenuring_threshold = 3;

        heap.age_survivor_regions();

        // Age 2 → 3 >= threshold → promoted to Old
        assert_eq!(heap.regions[0].region_type, RegionType::Old);
    }

    #[test]
    fn free_region_count() {
        let mut heap = make_heap();
        assert_eq!(heap.free_region_count(), 8);
        heap.alloc_eden(64, 8).unwrap();
        assert_eq!(heap.free_region_count(), 7);
    }

    #[test]
    fn remembered_set_basic() {
        let mut rset = RememberedSet::default();
        assert_eq!(rset.source_count(), 0);
        rset.add_reference(3);
        rset.add_reference(7);
        rset.add_reference(3); // duplicate
        assert_eq!(rset.source_count(), 2);
        rset.clear();
        assert_eq!(rset.source_count(), 0);
    }
}
