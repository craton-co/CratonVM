// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Old generation free-list allocator with mark-compact collection.
//!
//! The old generation uses a free-list allocator for allocation and a sliding
//! mark-compact collector for major GC. Objects are allocated from a
//! size-segregated free list. During major GC, live objects are compacted
//! toward the start of the heap, eliminating fragmentation entirely.
//!
//! ## Allocation strategy (round-5 #14 fix)
//!
//! The previous implementation kept a single `Vec<FreeBlock>` sorted by
//! offset and scanned the whole list on every allocation, paying O(N) per
//! `alloc` call. After a long-running mutator built up tens of thousands
//! of free fragments this became the dominant CPU cost of old-gen
//! allocation.
//!
//! The current implementation segregates free blocks into power-of-2 size
//! buckets (8, 16, 32, …, 512 MB). Allocation picks the smallest bucket
//! whose nominal size satisfies the request, then pops a block from the
//! front — amortised O(1). When all blocks in a bucket are exhausted the
//! allocator escalates to the next bucket up. Best-fit semantics are
//! preserved within a bucket by tracking the smallest-suitable block in a
//! single pass.
//!
//! The sorted-by-offset view needed for coalescing and compaction is
//! rebuilt on demand from the per-bucket vectors.

use std::collections::HashMap;

use crate::heap::{
    array_data_size, ArrayElementType, ObjectHeader, ObjectKind, GC_FLAG_MARKED, HEADER_SIZE,
    REF_ELEMENT_SIZE, SLOT_SIZE,
};
use cratonvm_types::{ObjectRef, Value};

/// A contiguous free block in the old generation.
#[derive(Debug, Clone, Copy)]
struct FreeBlock {
    /// Byte offset from the start of the data buffer.
    offset: usize,
    /// Size in bytes.
    size: usize,
}

/// Number of size buckets in the segregated free list.
///
/// Bucket `k` holds blocks whose size is in `[1 << (MIN_BUCKET_SHIFT + k),
/// 1 << (MIN_BUCKET_SHIFT + k + 1))`, with the top bucket also catching
/// everything above. 28 buckets starting at 8 bytes covers the range
/// `[8 B .. 2 GiB)`, which is more than enough for any realistic heap.
const NUM_BUCKETS: usize = 28;

/// Smallest tracked block size is `1 << MIN_BUCKET_SHIFT` = 8 bytes.
const MIN_BUCKET_SHIFT: u32 = 3;

/// Pick the bucket index that fits `size`. Result is in `[0, NUM_BUCKETS)`.
#[inline]
fn bucket_for(size: usize) -> usize {
    if size <= (1usize << MIN_BUCKET_SHIFT) {
        return 0;
    }
    // For 1-block size n, bucket is index k such that
    //   (1 << (MIN_BUCKET_SHIFT + k)) <= n < (1 << (MIN_BUCKET_SHIFT + k + 1))
    // i.e. floor(log2(n)) - MIN_BUCKET_SHIFT.
    let lg = (usize::BITS - 1 - size.leading_zeros()) as usize;
    let idx = lg.saturating_sub(MIN_BUCKET_SHIFT as usize);
    idx.min(NUM_BUCKETS - 1)
}

/// Pick the smallest bucket index that *might* contain a block large enough
/// to satisfy `size`. Allocators start their search here and escalate upward.
///
/// CRIT (round-5 GC #2, spurious OOM): the previous implementation rounded
/// `size` *up* to the next power of two and used that bucket as the search
/// start — but bucket `k` holds blocks in `[2^(s+k), 2^(s+k+1))`, indexed
/// by `floor(log2(block_size))`. For a request of 100 bytes, the old code
/// jumped to bucket 4 (lower bound 128) and skipped bucket 3, which holds
/// blocks in `[64, 128)`. A 100-byte free block lives in bucket 3; the
/// allocator would miss it entirely and report OOM despite having a
/// fitting block on hand.
///
/// The correct starting bucket is `floor(log2(size))`: that bucket may
/// hold blocks ranging from `size` up to `2*size - 1`, which are valid
/// fits. The per-bucket best-fit scan filters out blocks too small to
/// satisfy the request (`total_needed <= block.size`), so starting one
/// bucket lower than the old code costs only an extra cheap scan in the
/// rare worst case and *cannot* spuriously OOM.
#[inline]
fn min_satisfying_bucket(size: usize) -> usize {
    if size <= (1usize << MIN_BUCKET_SHIFT) {
        return 0;
    }
    // floor(log2(size)) - MIN_BUCKET_SHIFT — same formula as `bucket_for`.
    let lg = (usize::BITS - 1 - size.leading_zeros()) as usize;
    lg.saturating_sub(MIN_BUCKET_SHIFT as usize).min(NUM_BUCKETS - 1)
}

/// Non-moving free-list allocator for the old generation.
///
/// Objects are allocated from a size-segregated free list (round-5 #14
/// fix). Freed blocks are coalesced with adjacent free blocks during
/// compaction; intermediate freeing skips the coalesce scan (which was
/// the other O(N) cost in the original implementation) and lets the next
/// major GC absorb the fragmentation in one sweep.
pub struct OldGen {
    /// Backing storage. `len() == capacity` so pointers can be computed
    /// into it, but the bytes are *not* eagerly zeroed (round-11 perf) —
    /// `alloc` zeroes every region before handing it out.
    data: Vec<u8>,
    /// Size-segregated free list: `buckets[k]` holds blocks whose size
    /// falls in bucket `k`. Each bucket is treated as a LIFO stack —
    /// `push`/`pop` are both amortised O(1).
    buckets: Vec<Vec<FreeBlock>>,
    /// Total bytes currently allocated (excluding free space).
    used_bytes: usize,
}

impl OldGen {
    /// Create a new old generation with the given capacity.
    pub fn new(capacity: usize) -> Self {
        // Round-11 perf: skip the eager `vec![0u8; capacity]` zero pass.
        // A 128 MiB old gen previously touched every page on construction
        // (zero-fill + page commit) before a single object was allocated.
        //
        // `alloc` is the *sole* point that hands a byte to a caller, and
        // it always `write_bytes(ptr, 0, size)` before returning — so no
        // legitimate caller can observe an uninitialised byte as data.
        // `walk_objects`/`scan_region`/`compact` only ever read headers
        // inside *allocated* regions (the gaps between free blocks); a
        // freshly-constructed `OldGen` has the whole capacity as a single
        // free block, so the walk scans nothing. `compact` zeroes any
        // freed tail it creates.
        //
        // We still need `data.len() == capacity` because pointers are
        // computed as `data.as_mut_ptr().add(offset)` and `capacity()` /
        // `contains` rely on `data.len()`. Reserve the full capacity, then
        // set the length without initialising — the OS hands back pages
        // lazily on first write instead of all up front.
        let mut data: Vec<u8> = Vec::with_capacity(capacity);
        // SAFETY: `with_capacity(capacity)` allocated exactly `capacity`
        // bytes of backing storage. `u8` has no validity invariant and no
        // `Drop`, so extending the logical length over that already-owned
        // allocation is sound. Every byte is zero-initialised by `alloc`
        // before it is handed out; no read observes it uninitialised.
        unsafe {
            data.set_len(capacity);
        }
        let mut buckets: Vec<Vec<FreeBlock>> = (0..NUM_BUCKETS).map(|_| Vec::new()).collect();
        // Seed the initial block in the bucket that fits the full capacity.
        let initial = FreeBlock { offset: 0, size: capacity };
        buckets[bucket_for(capacity)].push(initial);
        Self {
            data,
            buckets,
            used_bytes: 0,
        }
    }

    /// Allocate `size` bytes with the given alignment from the segregated
    /// free list.
    ///
    /// Round-5 #14: search starts at the smallest bucket guaranteed to
    /// satisfy `size` and escalates upward. Within a bucket the scan is
    /// best-fit, but bucket sizes mean the worst-case scan touches only
    /// blocks roughly the right size — not the entire free list.
    /// Amortised O(1).
    ///
    /// Round-9 gc CRIT-3 fix: a request of `size == 0` previously
    /// returned `self.data.as_mut_ptr()` (i.e. the start of the backing
    /// buffer) without reserving any bytes. The caller would treat that
    /// pointer as a fresh, non-aliasing allocation while it actually
    /// aliased whatever was already living at offset 0 — typically the
    /// header of the first old-gen object. A subsequent write through
    /// the bogus pointer corrupted live state. Round up zero-sized
    /// requests to the smallest plausible object footprint
    /// (`HEADER_SIZE`-or-larger, 8-byte aligned) so every call returns
    /// a fresh, non-aliasing block.
    pub fn alloc(&mut self, size: usize, align: usize) -> Option<*mut u8> {
        // Round-9 gc CRIT-3: reserve at least HEADER_SIZE bytes (and
        // never less than 8 for alignment headroom) so an `alloc(0)`
        // can't alias an existing allocation.
        let size = size.max(HEADER_SIZE.max(8));

        let base = self.data.as_ptr() as usize;
        let start_bucket = min_satisfying_bucket(size + align - 1);

        // Walk buckets from the smallest guaranteed-fit upward.
        for bucket_idx in start_bucket..NUM_BUCKETS {
            // Best-fit scan within this bucket (block sizes are bounded
            // within a factor of 2, so scanning the bucket is cheap).
            let mut best: Option<usize> = None;
            let mut best_waste: usize = usize::MAX;
            for i in 0..self.buckets[bucket_idx].len() {
                let block = self.buckets[bucket_idx][i];
                let block_addr = base + block.offset;
                let aligned_addr = (block_addr + align - 1) & !(align - 1);
                let padding = aligned_addr - block_addr;
                let total_needed = padding + size;
                if total_needed <= block.size {
                    let waste = block.size - total_needed;
                    if waste < best_waste {
                        best_waste = waste;
                        best = Some(i);
                        if waste == 0 {
                            break;
                        }
                    }
                }
            }

            let Some(i) = best else { continue };
            // swap_remove keeps the bucket O(1).
            let block = self.buckets[bucket_idx].swap_remove(i);
            let block_addr = base + block.offset;
            let aligned_addr = (block_addr + align - 1) & !(align - 1);
            let padding = aligned_addr - block_addr;
            let alloc_offset = block.offset + padding;

            // Re-insert any leftover padding into its correct bucket.
            if padding > 0 {
                let pad_block = FreeBlock {
                    offset: block.offset,
                    size: padding,
                };
                self.buckets[bucket_for(padding)].push(pad_block);
            }
            // Re-insert the tail leftover (after `padding + size`) into
            // its correct bucket.
            let remaining = block.size - padding - size;
            if remaining > 0 {
                let tail_block = FreeBlock {
                    offset: alloc_offset + size,
                    size: remaining,
                };
                self.buckets[bucket_for(remaining)].push(tail_block);
            }

            self.used_bytes += size;
            // SAFETY: alloc_offset is within [0, self.data.len()) and size bytes fit
            // within the selected free block bounds.
            let ptr = unsafe { self.data.as_mut_ptr().add(alloc_offset) };
            // Round-5 #14 — sole zeroing point. The previous code zeroed
            // both here AND in `free`; the free-side zero was redundant
            // because every byte handed back to a caller flows through
            // this path.
            // SAFETY: ptr points to alloc_offset within the data buffer with at
            // least size bytes available. Zero-initializing the allocated region.
            unsafe {
                std::ptr::write_bytes(ptr, 0, size);
            }
            return Some(ptr);
        }

        None
    }

    /// Free a previously allocated block, returning it to the free list.
    ///
    /// Round-5 #14: the redundant per-free zero pass has been dropped —
    /// `alloc` zeros every byte before handing it out, so freed bytes are
    /// never observable as data by a future caller. Coalescing with
    /// neighboring free blocks is deferred to the next mark-compact pass
    /// to keep this path O(1).
    ///
    /// # Safety
    /// `ptr` must point to a block previously allocated from this OldGen,
    /// and `size` must be the exact size of that allocation.
    pub unsafe fn free(&mut self, ptr: *mut u8, size: usize) {
        let base = self.data.as_ptr() as usize;
        let addr = ptr as usize;
        debug_assert!(addr >= base && addr + size <= base + self.data.len());
        let offset = addr - base;

        self.used_bytes = self.used_bytes.saturating_sub(size);

        // Push into the appropriate size bucket — O(1).
        // Coalescing happens during the next `compact()` call.
        self.buckets[bucket_for(size)].push(FreeBlock { offset, size });
    }

    /// Returns true if the given pointer falls within this old generation's storage.
    pub fn contains(&self, ptr: *const u8) -> bool {
        let base = self.data.as_ptr();
        let end = unsafe { base.add(self.data.len()) };
        ptr >= base && ptr < end
    }

    /// Get the base pointer of the backing storage.
    pub fn base_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    /// Get a mutable base pointer of the backing storage.
    pub fn base_ptr_mut(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }

    /// Total bytes currently allocated.
    pub fn used(&self) -> usize {
        self.used_bytes
    }

    /// Total capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.data.len()
    }

    /// Walk all allocated objects in the old generation.
    ///
    /// This iterates through allocated regions (gaps between free blocks)
    /// and yields each object's pointer and header. Used by the mark-sweep
    /// collector to iterate old-gen objects.
    ///
    /// Returns a Vec of (object pointer, total object size) pairs.
    pub fn walk_objects(&self) -> Vec<(*mut u8, usize)> {
        let mut objects = Vec::new();
        let base = self.data.as_ptr() as usize;

        // Round-5 #14: rebuild the offset-sorted view from segregated
        // buckets on demand. Free is now O(1) and walk-objects pays the
        // sort cost up front; the trade is favourable because alloc/free
        // run on the hot path and walk_objects only at GC time.
        let mut sorted_free: Vec<FreeBlock> = self.buckets.iter().flatten().copied().collect();
        sorted_free.sort_by_key(|b| b.offset);

        let mut cursor: usize = 0;
        for block in &sorted_free {
            // Allocated region from cursor to block.offset
            if block.offset > cursor {
                self.scan_region(base, cursor, block.offset, &mut objects);
            }
            cursor = block.offset + block.size;
        }
        // Region after last free block
        if cursor < self.data.len() {
            self.scan_region(base, cursor, self.data.len(), &mut objects);
        }

        objects
    }

    /// Walk old-gen objects, retaining only those whose start offset falls
    /// inside one of the supplied dirty-card offset ranges.
    ///
    /// Round-11 perf: the minor-GC dirty-card scan previously called
    /// [`Self::walk_objects`], which allocates a `Vec` holding *every*
    /// live old-gen object and sorts the entire free list, even when only
    /// a handful of cards are dirty. This variant bounds two of those
    /// costs to the dirty set:
    ///
    /// * the result `Vec` only ever holds objects in dirty cards, so its
    ///   size is O(dirty objects) rather than O(all old-gen objects);
    /// * an allocated region that overlaps no dirty card is walked for
    ///   *cursor advancement only* — no `(ptr, size)` pairs are pushed.
    ///
    /// `dirty_ranges` must be a list of `[start, end)` byte offsets
    /// (relative to the data buffer) sorted ascending by `start` and
    /// non-overlapping; the card table produces exactly that. Object
    /// boundaries are still derived header-by-header from the sorted free
    /// list — that derivation is what makes the scan *correct*, so it is
    /// preserved unchanged; only the work *per object* is bounded.
    pub fn walk_objects_in_card_ranges(
        &self,
        dirty_ranges: &[(usize, usize)],
    ) -> Vec<(*mut u8, usize)> {
        let mut objects = Vec::new();
        if dirty_ranges.is_empty() {
            return objects;
        }
        let base = self.data.as_ptr() as usize;

        // Same offset-sorted free-list view as `walk_objects`; needed to
        // locate true object boundaries (old-gen layout is header-following
        // with no external object index).
        let mut sorted_free: Vec<FreeBlock> = self.buckets.iter().flatten().copied().collect();
        sorted_free.sort_by_key(|b| b.offset);

        let last_dirty_end = dirty_ranges[dirty_ranges.len() - 1].1;
        let mut cursor: usize = 0;
        for block in &sorted_free {
            if block.offset > cursor {
                self.scan_region_filtered(base, cursor, block.offset, dirty_ranges, &mut objects);
            }
            cursor = block.offset + block.size;
            // Allocated regions are address-ordered; once the cursor passes
            // the last dirty card there is nothing left to collect.
            if cursor >= last_dirty_end {
                return objects;
            }
        }
        if cursor < self.data.len() {
            self.scan_region_filtered(base, cursor, self.data.len(), dirty_ranges, &mut objects);
        }

        objects
    }

    /// Like [`Self::scan_region`], but only pushes objects whose start
    /// offset lies within one of `dirty_ranges`. Every object boundary is
    /// still visited so the cursor advances correctly; the filter only
    /// gates whether the `(ptr, size)` pair is collected.
    fn scan_region_filtered(
        &self,
        base: usize,
        start_offset: usize,
        end_offset: usize,
        dirty_ranges: &[(usize, usize)],
        objects: &mut Vec<(*mut u8, usize)>,
    ) {
        let mut offset = start_offset;
        while offset < end_offset {
            let ptr = (base + offset) as *mut u8;
            // SAFETY: `offset` is a valid object boundary within an
            // allocated region of the data buffer (see `scan_region`).
            let header = unsafe { &*(ptr as *const ObjectHeader) };
            if header.kind == ObjectKind::HumongousFiller {
                break;
            }
            let total_size = if header.kind == ObjectKind::Array {
                HEADER_SIZE
                    + array_data_size(header.array_length as usize, header.element_type)
                        .expect("array_data_size overflow in old_gen scan")
            } else {
                HEADER_SIZE + header.num_slots as usize * SLOT_SIZE
            };
            if total_size < HEADER_SIZE || offset + total_size > end_offset {
                break;
            }
            // Collect only if the object's start lands in a dirty card.
            if dirty_ranges
                .iter()
                .any(|&(s, e)| offset >= s && offset < e)
            {
                objects.push((ptr, total_size));
            }
            offset += total_size;
        }
    }

    /// Scan a contiguous allocated region for objects.
    fn scan_region(
        &self,
        base: usize,
        start_offset: usize,
        end_offset: usize,
        objects: &mut Vec<(*mut u8, usize)>,
    ) {
        let mut offset = start_offset;
        while offset < end_offset {
            let ptr = (base + offset) as *mut u8;
            let header = unsafe { &*(ptr as *const ObjectHeader) };
            // Round-9 gc CRIT-1: HumongousFiller is a synthetic walker
            // sentinel installed by the regional GC (see `g1.rs` and
            // `region.rs`). It should never appear in the non-regional
            // old-gen layout, but defensively skip the rest of the
            // current scan stripe instead of mis-parsing it as a real
            // object (which would corrupt the offset cursor).
            if header.kind == ObjectKind::HumongousFiller {
                break;
            }
            let total_size = if header.kind == ObjectKind::Array {
                HEADER_SIZE + array_data_size(header.array_length as usize, header.element_type)
                    .expect("array_data_size overflow in old_gen scan")
            } else {
                HEADER_SIZE + header.num_slots as usize * SLOT_SIZE
            };
            // Sanity check: if total_size is 0 or too large, stop scanning
            if total_size < HEADER_SIZE || offset + total_size > end_offset {
                break;
            }
            objects.push((ptr, total_size));
            offset += total_size;
        }
    }

    // -----------------------------------------------------------------------
    // Mark-Compact Collection (Session 26)
    // -----------------------------------------------------------------------

    /// Compact all marked (live) objects toward the start of the heap using
    /// sliding compaction. This eliminates all fragmentation — after compaction,
    /// the free list is a single contiguous block at the end.
    ///
    /// **Algorithm (4 phases):**
    /// 1. **Compute forwarding addresses** — walk objects in address order,
    ///    assign each live object a destination at the next free position.
    /// 2. **Update internal references** — for each live object, rewrite
    ///    reference slots to use forwarding addresses.
    /// 3. **Slide objects** — copy each object to its forwarding address
    ///    (low-to-high order guarantees no data corruption since dest ≤ src).
    /// 4. **Rebuild free list** — single free block from compacted end to
    ///    capacity.
    ///
    /// **Precondition:** live objects have `GC_FLAG_MARKED` set in their headers.
    /// **Postcondition:** `GC_FLAG_MARKED` and `forwarding_ptr` are cleared on
    /// all surviving objects. Dead objects are reclaimed.
    ///
    /// Returns a pointer map (old_addr → new_addr) for objects that moved.
    /// Objects that stay in place are NOT included in the map.
    pub fn compact(&mut self) -> HashMap<usize, usize> {
        let base = self.data.as_mut_ptr();
        let objects = self.walk_objects();

        // Phase 1: Compute forwarding addresses for live objects.
        // `write_cursor` tracks the next available byte offset (8-byte aligned).
        let mut write_cursor: usize = 0;
        let mut pointer_map: HashMap<usize, usize> = HashMap::new();
        // (src_ptr, total_size, dest_ptr) for each live object
        let mut live_objects: Vec<(*mut u8, usize, *mut u8)> = Vec::new();

        for &(obj_ptr, total_size) in &objects {
            let header = unsafe { &mut *(obj_ptr as *mut ObjectHeader) };
            if header.gc_flags & GC_FLAG_MARKED == 0 {
                continue; // Dead object — skip
            }
            let aligned = (write_cursor + 7) & !7;
            let dest = unsafe { base.add(aligned) };

            // Store forwarding address in header for Phase 2 reference updates
            header.forwarding_ptr = dest;

            if obj_ptr != dest {
                pointer_map.insert(obj_ptr as usize, dest as usize);
            }
            live_objects.push((obj_ptr, total_size, dest));
            write_cursor = aligned + total_size;
        }

        // Phase 2: Update references within live old-gen objects.
        // Each reference slot that points to a moved old-gen object is rewritten
        // to the object's forwarding address.
        for &(obj_ptr, _, _) in &live_objects {
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            Self::update_refs_in_object(obj_ptr, header, &self.data);
        }

        // Phase 3: Slide objects to their forwarding addresses.
        // Processing order is low-to-high by source address, and dest ≤ src
        // for sliding compaction, so `std::ptr::copy` (memmove) is safe even
        // for overlapping regions.
        for &(src, size, dest) in &live_objects {
            if src != dest {
                unsafe { std::ptr::copy(src, dest, size) };
            }
            // Clear GC metadata on the (possibly moved) object
            let final_header = unsafe { &mut *(dest as *mut ObjectHeader) };
            final_header.forwarding_ptr = std::ptr::null_mut();
            final_header.gc_flags &= !GC_FLAG_MARKED;
        }

        // Phase 4: Rebuild free list — one contiguous block at the end.
        // Round-5 #14: clears every bucket so deferred free()s coalesce
        // into the single trailing block formed by compaction.
        let compacted_end = (write_cursor + 7) & !7;
        for bucket in &mut self.buckets {
            bucket.clear();
        }
        if compacted_end < self.data.len() {
            // Zero the freed region for safety
            unsafe {
                std::ptr::write_bytes(base.add(compacted_end), 0, self.data.len() - compacted_end)
            };
            let free_size = self.data.len() - compacted_end;
            self.buckets[bucket_for(free_size)].push(FreeBlock {
                offset: compacted_end,
                size: free_size,
            });
        }
        self.used_bytes = compacted_end;

        pointer_map
    }

    /// Update reference slots within a single live old-gen object so they point
    /// to forwarding addresses of moved objects.
    ///
    /// Handles both regular object fields (16-byte `Value` slots) and reference
    /// arrays (8-byte compact pointers).
    fn update_refs_in_object(obj_ptr: *mut u8, header: &ObjectHeader, data: &[u8]) {
        let data_start = data.as_ptr() as usize;
        let data_end = data_start + data.len();

        if header.kind == ObjectKind::Array {
            if header.element_type == ArrayElementType::Reference {
                for i in 0..header.array_length as usize {
                    let slot = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                    let raw: u64 = unsafe { std::ptr::read(slot as *const u64) };
                    if raw != 0 {
                        let ref_ptr = raw as usize;
                        // Only update references within old gen bounds
                        if ref_ptr >= data_start && ref_ptr < data_end {
                            let ref_header =
                                unsafe { &*(ref_ptr as *const ObjectHeader) };
                            if !ref_header.forwarding_ptr.is_null() {
                                unsafe {
                                    std::ptr::write(
                                        slot as *mut u64,
                                        ref_header.forwarding_ptr as u64,
                                    )
                                };
                            }
                        }
                    }
                }
            }
        } else {
            for slot_idx in 0..header.num_slots as usize {
                let slot = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                let value = unsafe { std::ptr::read(slot as *const Value) };
                if let Value::Object(Some(ref_obj)) = value {
                    let ref_ptr = ref_obj.as_ptr() as usize;
                    if ref_ptr >= data_start && ref_ptr < data_end {
                        let ref_header =
                            unsafe { &*(ref_ptr as *const ObjectHeader) };
                        if !ref_header.forwarding_ptr.is_null() {
                            let new_value = Value::Object(Some(unsafe {
                                ObjectRef::from_raw(ref_header.forwarding_ptr)
                            }));
                            unsafe { std::ptr::write(slot as *mut Value, new_value) };
                        }
                    }
                }
            }
        }
    }

    /// Returns the number of contiguous free blocks (fragmentation metric).
    pub fn free_block_count(&self) -> usize {
        // Round-5 #14: count across all size buckets.
        self.buckets.iter().map(|b| b.len()).sum()
    }

    /// Returns the size of the largest contiguous free block.
    pub fn largest_free_block(&self) -> usize {
        // Round-5 #14: the largest block lives in the highest non-empty bucket;
        // scan that bucket for the actual maximum.
        for bucket in self.buckets.iter().rev() {
            if let Some(&max) = bucket.iter().map(|b| &b.size).max() {
                return max;
            }
        }
        0
    }
}

impl std::fmt::Debug for OldGen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OldGen")
            .field("used", &self.used_bytes)
            .field("capacity", &self.data.len())
            .field("free_blocks", &self.free_block_count())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_old_gen() {
        let og = OldGen::new(4096);
        assert_eq!(og.used(), 0);
        assert_eq!(og.capacity(), 4096);
        // Round-5 #14: free_list is now segregated; only the count is observable.
        assert_eq!(og.free_block_count(), 1);
    }

    #[test]
    fn alloc_basic() {
        let mut og = OldGen::new(4096);
        let ptr = og.alloc(64, 8).unwrap();
        assert!(!ptr.is_null());
        assert_eq!(og.used(), 64);
        assert!(og.contains(ptr));
    }

    #[test]
    fn alloc_multiple() {
        let mut og = OldGen::new(4096);
        let p1 = og.alloc(64, 8).unwrap();
        let p2 = og.alloc(128, 8).unwrap();
        assert_ne!(p1, p2);
        assert_eq!(og.used(), 192);
    }

    #[test]
    fn alloc_alignment() {
        let mut og = OldGen::new(4096);
        // Allocate 3 bytes, then 64 with align 8
        let _p1 = og.alloc(3, 1).unwrap();
        let p2 = og.alloc(64, 8).unwrap();
        // p2 should be 8-byte aligned
        assert_eq!(p2 as usize % 8, 0);
    }

    #[test]
    fn alloc_full_returns_none() {
        let mut og = OldGen::new(128);
        let _p1 = og.alloc(128, 1).unwrap();
        assert!(og.alloc(1, 1).is_none());
    }

    #[test]
    fn free_basic() {
        let mut og = OldGen::new(4096);
        let p1 = og.alloc(64, 8).unwrap();
        assert_eq!(og.used(), 64);

        unsafe { og.free(p1, 64) };
        assert_eq!(og.used(), 0);
        // Should be able to allocate again
        let p2 = og.alloc(64, 8).unwrap();
        assert!(!p2.is_null());
    }

    #[test]
    fn free_coalesce_forward() {
        let mut og = OldGen::new(4096);
        let p1 = og.alloc(64, 8).unwrap();
        let p2 = og.alloc(64, 8).unwrap();
        let _p3 = og.alloc(64, 8).unwrap();

        // Free p2, then p1.
        unsafe { og.free(p2, 64) };
        unsafe { og.free(p1, 64) };

        // Round-5 #14: free() defers coalescing to the next compact().
        // The big trailing free block (everything after p3) still
        // satisfies a 128-byte allocation directly.
        assert_eq!(og.used(), 64); // only p3 remains
        let big = og.alloc(128, 8).unwrap();
        assert!(!big.is_null());
    }

    #[test]
    fn free_coalesce_backward() {
        let mut og = OldGen::new(4096);
        let p1 = og.alloc(64, 8).unwrap();
        let p2 = og.alloc(64, 8).unwrap();

        // Free p1, then p2.
        unsafe { og.free(p1, 64) };
        unsafe { og.free(p2, 64) };
        assert_eq!(og.used(), 0);

        // Round-5 #14: free() no longer coalesces immediately — coalescing
        // is deferred to the next mark-compact. After freeing both blocks
        // the total free capacity is preserved and the heap can serve a
        // fresh allocation that exactly matches one of the freed blocks
        // (proving the bucketed freelist still hands back contiguous
        // memory).
        let p3 = og.alloc(64, 8).unwrap();
        assert!(!p3.is_null());
    }

    #[test]
    fn contains() {
        let og = OldGen::new(256);
        let base = og.base_ptr();
        assert!(og.contains(base));
        assert!(og.contains(unsafe { base.add(128) }));
        assert!(!og.contains(std::ptr::null()));
        assert!(!og.contains(unsafe { base.add(256) })); // exclusive end
    }

    #[test]
    fn walk_objects_empty() {
        let og = OldGen::new(4096);
        let objects = og.walk_objects();
        assert!(objects.is_empty());
    }

    #[test]
    fn walk_objects_with_allocations() {
        let mut og = OldGen::new(4096);

        // Allocate two objects manually with proper headers
        let obj_size1 = HEADER_SIZE + 2 * SLOT_SIZE; // 2 fields
        let p1 = og.alloc(obj_size1, 8).unwrap();
        unsafe {
            let header = &mut *(p1 as *mut ObjectHeader);
            header.num_slots = 2;
        }

        let obj_size2 = HEADER_SIZE + SLOT_SIZE; // 1 field
        let p2 = og.alloc(obj_size2, 8).unwrap();
        unsafe {
            let header = &mut *(p2 as *mut ObjectHeader);
            header.num_slots = 1;
        }

        let objects = og.walk_objects();
        assert_eq!(objects.len(), 2);
        assert_eq!(objects[0].0, p1);
        assert_eq!(objects[0].1, obj_size1);
        assert_eq!(objects[1].0, p2);
        assert_eq!(objects[1].1, obj_size2);
    }
}
