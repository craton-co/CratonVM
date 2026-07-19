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

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use crate::heap::{
    array_data_size, ArrayElementType, ObjectHeader, ObjectKind, GC_FLAG_MARKED, HEADER_SIZE,
    REF_ELEMENT_SIZE, SLOT_SIZE,
};
use cratonvm_types::{ObjectRef, Value};

/// Cached `CRATONVM_DBG_SEEDHUNT` gate (bc math-ec `0x4`). When on,
/// `update_refs_in_object` logs any referent whose `forwarding_ptr` is
/// non-null but `< 0x1000` — the §6.1 suspect that would write
/// `Object(Some(0x4))` into a live referrer's field during major-GC
/// compaction. See docs/bc-math-ec-gc-0x4-handoff.md §6.1.
#[inline]
fn seedhunt_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| std::env::var_os("CRATONVM_DBG_SEEDHUNT").is_some())
}

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
    lg.saturating_sub(MIN_BUCKET_SHIFT as usize)
        .min(NUM_BUCKETS - 1)
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
    /// PERF (gc-oldgen-perf): cache of the offset-sorted free-block view.
    ///
    /// `walk_objects` / `walk_objects_in_card_ranges` / `compact` all need
    /// the free blocks in ascending-offset order to locate object boundaries
    /// (the gaps between free blocks are the allocated regions). The previous
    /// code re-`collect()`ed every block out of all 28 buckets and ran a fresh
    /// `sort_by_key` on *every* call. Several GC phases call `walk_objects`
    /// repeatedly with no intervening `alloc`/`free` (e.g. the
    /// `concurrent_mark` rescan loop and the multiple sequential sweep/verify
    /// passes in `gen_heap`), so that view is recomputed identically many
    /// times per cycle.
    ///
    /// We cache the sorted view and rebuild it lazily only when the buckets
    /// have actually changed. `dirty` is set by every bucket mutation
    /// (`alloc`, `free`, and `compact`'s free-list rebuild); while it stays
    /// clear the cached `Vec` is byte-for-byte the same sort that would have
    /// been recomputed, so behaviour is identical — only redundant work is
    /// removed. `RefCell`/`Cell` give interior mutability so the `&self`
    /// walkers can refresh the cache; `OldGen` lives inside a `Mutex`, which
    /// only requires `Send` (satisfied), not `Sync`.
    sorted_free_cache: RefCell<Vec<FreeBlock>>,
    /// `true` when `buckets` has been mutated since `sorted_free_cache` was
    /// last rebuilt, so the cache must be regenerated before use.
    sorted_free_dirty: Cell<bool>,
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
        let initial = FreeBlock {
            offset: 0,
            size: capacity,
        };
        buckets[bucket_for(capacity)].push(initial);
        Self {
            data,
            buckets,
            used_bytes: 0,
            // Start dirty: the cache is empty and the first walk rebuilds it.
            sorted_free_cache: RefCell::new(Vec::new()),
            sorted_free_dirty: Cell::new(true),
        }
    }

    /// Mark the cached offset-sorted free-block view stale.
    ///
    /// PERF (gc-oldgen-perf): called from every site that mutates `buckets`
    /// (`alloc`, `free`, and `compact`'s free-list rebuild). Cheap — just
    /// flips a `Cell<bool>`; the actual recompute is deferred to the next
    /// walker that needs the sorted view.
    #[inline]
    fn invalidate_sorted_free(&self) {
        self.sorted_free_dirty.set(true);
    }

    /// Run `f` with the offset-sorted free-block view, rebuilding the cached
    /// view first only if the buckets changed since it was last built.
    ///
    /// PERF (gc-oldgen-perf): replaces the per-call `buckets.iter().flatten()
    /// .collect()` + `sort_by_key` that `walk_objects` /
    /// `walk_objects_in_card_ranges` / `compact` each used to do unconditionally.
    /// The produced slice is the *exact same* ascending-offset ordering as
    /// before (same elements, same comparator), so every walk is unchanged;
    /// when nothing mutated the buckets between two walks the second one reuses
    /// the cache and skips the collect+sort entirely.
    ///
    /// The closure receives a borrowed `&[FreeBlock]`; the `RefCell` borrow is
    /// held only for the duration of `f`. Callers must not mutate the buckets
    /// (which would require `&mut self`) from inside `f`, and none do.
    fn with_sorted_free_blocks<R>(&self, f: impl FnOnce(&[FreeBlock]) -> R) -> R {
        if self.sorted_free_dirty.get() {
            let mut cache = self.sorted_free_cache.borrow_mut();
            cache.clear();
            cache.extend(self.buckets.iter().flatten().copied());
            cache.sort_by_key(|b| b.offset);
            self.sorted_free_dirty.set(false);
        }
        let cache = self.sorted_free_cache.borrow();
        f(&cache)
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
        // Keep the reserved extent consistent with the young arena: compact
        // bodies may be non-aligned, but the next object must not start after
        // an unowned padding gap.
        let size = size.checked_add(align - 1).map(|v| v & !(align - 1))?;

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
            // PERF (gc-oldgen-perf): buckets change here (and via the pushes
            // below) — invalidate the cached sorted free-block view once.
            self.invalidate_sorted_free();
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
        // Mirror the rounding `alloc` applied so accounting stays
        // symmetric: `alloc` reserved `size.max(HEADER_SIZE.max(8))`
        // bytes (and added that rounded amount to `used_bytes`), so a
        // caller that frees with the originally-requested sub-HEADER_SIZE
        // size must decrement — and return to the free list — the same
        // rounded amount. This expression MUST match `alloc`'s rounding.
        let size = size.max(HEADER_SIZE.max(8));

        let base = self.data.as_ptr() as usize;
        let addr = ptr as usize;
        debug_assert!(addr >= base && addr + size <= base + self.data.len());
        let offset = addr - base;

        self.used_bytes = self.used_bytes.saturating_sub(size);

        // Push into the appropriate size bucket — O(1).
        // Coalescing happens during the next `compact()` call.
        self.buckets[bucket_for(size)].push(FreeBlock { offset, size });
        // PERF (gc-oldgen-perf): a new free block changes the sorted view.
        self.invalidate_sorted_free();
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
        //
        // PERF (gc-oldgen-perf): the collect+sort is now memoised via
        // `with_sorted_free_blocks` — when the buckets are unchanged since the
        // previous walk (common across GC phases that call `walk_objects`
        // repeatedly without allocating) the cached ordering is reused instead
        // of being rebuilt from scratch. The slice contents are identical to
        // the old inline `collect()`+`sort_by_key`, so the walk is unchanged.
        self.with_sorted_free_blocks(|sorted_free| {
            let mut cursor: usize = 0;
            for block in sorted_free {
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
        });

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
        //
        // PERF (gc-oldgen-perf): shares the memoised sorted view with
        // `walk_objects` via `with_sorted_free_blocks` rather than re-collecting
        // and re-sorting the buckets here. Same ordering, same early-exit once
        // the cursor passes the last dirty card.
        let last_dirty_end = dirty_ranges[dirty_ranges.len() - 1].1;
        self.with_sorted_free_blocks(|sorted_free| {
            let mut cursor: usize = 0;
            for block in sorted_free {
                if block.offset > cursor {
                    self.scan_region_filtered(
                        base,
                        cursor,
                        block.offset,
                        dirty_ranges,
                        &mut objects,
                    );
                }
                cursor = block.offset + block.size;
                // Allocated regions are address-ordered; once the cursor passes
                // the last dirty card there is nothing left to collect.
                if cursor >= last_dirty_end {
                    return;
                }
            }
            if cursor < self.data.len() {
                self.scan_region_filtered(
                    base,
                    cursor,
                    self.data.len(),
                    dirty_ranges,
                    &mut objects,
                );
            }
        });

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
            let raw_size = if header.kind == ObjectKind::Array {
                HEADER_SIZE
                    + array_data_size(header.array_length as usize, header.element_type)
                        .expect("array_data_size overflow in old_gen scan")
            } else {
                // Compact reference-field layout: a promoted compact object's body
                // size is in the header's `array_length`, not `num_slots*SLOT_SIZE`.
                // `object_body_size` honours the per-object `GC_FLAG_COMPACT` bit
                // (legacy objects still size as `num_slots*SLOT_SIZE`). Without this
                // the walker strides `num_slots*16` over a compact object, desyncing
                // the whole old-gen walk and tripping the size-consistency skip in
                // `scan_dirty_cards` (→ missed old→young roots → corruption).
                HEADER_SIZE + cratonvm_types::object_body_size(header)
            };
            let total_size = raw_size.checked_add(7).map(|size| size & !7).unwrap_or(0);
            if total_size < HEADER_SIZE || offset + total_size > end_offset {
                break;
            }
            // Collect only if the object's start lands in a dirty card.
            if dirty_ranges.iter().any(|&(s, e)| offset >= s && offset < e) {
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
            let raw_size = if header.kind == ObjectKind::Array {
                HEADER_SIZE
                    + array_data_size(header.array_length as usize, header.element_type)
                        .expect("array_data_size overflow in old_gen scan")
            } else {
                // Compact reference-field layout: honour the per-object
                // `GC_FLAG_COMPACT` bit (body size in `array_length`); legacy
                // objects still size as `num_slots*SLOT_SIZE`. See the matching
                // note in `scan_region_filtered`.
                HEADER_SIZE + cratonvm_types::object_body_size(header)
            };
            let total_size = raw_size.checked_add(7).map(|size| size & !7).unwrap_or(0);
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

        // Phase 0 (dangling-ref guard): close the live set under "referenced
        // by a live old-gen object".
        //
        // Phase 1 assigns a `forwarding_ptr` only to MARKED objects, and Phase
        // 2 rewrites a referrer's slot only when the target's `forwarding_ptr`
        // is non-null. For an in-old-gen target, a null `forwarding_ptr` after
        // Phase 1 means exactly one thing: the target was UNMARKED (floating
        // garbage). Previously Phase 2 silently skipped such slots, leaving the
        // live referrer pointing at the target's *old* location — which Phase 3
        // then slides another object onto (or Phase 4 zeroes), turning the slot
        // into a dangling pointer that the mutator later dereferences.
        //
        // That situation should not arise if the marker is transitively
        // correct (a live referrer's targets are themselves live). But the
        // compactor must not *corrupt the heap* when the marker under-marks:
        // GC correctness is fail-safe here. So we conservatively promote any
        // unmarked old-gen object reachable from a marked one to live (a small
        // mark-closure fixpoint restricted to old-gen), guaranteeing every slot
        // a live object can hold gets a forwarding address in Phase 1. This
        // retains a little floating garbage until the next cycle (cheap, safe)
        // rather than producing a dangling pointer (fatal). It is task option
        // (a) — "treat any object reachable from a live referrer as live" —
        // applied locally and bounded to the objects we are about to walk.
        Self::close_live_set_over_old_gen(&objects, &self.data);

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
            } else if crate::gc_quiescence::is_watched_referent(obj_ptr as usize) {
                // RandomizedContext WeakHashMap<Thread,...> fix (same class of
                // bug as gen_heap.rs's non-moving young sweep — see
                // gc_quiescence::is_watched_referent doc comment): this live
                // object happened not to move during sliding compaction, so
                // it gets no `pointer_map` entry ("objects that stay in place
                // are NOT included in the map", by design, above). Post-GC
                // reference processing's `is_marked` check treats an address
                // absent from `pointer_map` as "did not survive" — correct
                // for a moved object, wrong for one that simply stayed put.
                // A long-lived object (e.g. a test suite's own `Thread`
                // mirror, promoted to old gen after surviving many young
                // GCs) is exactly the kind of object likely to stay at a
                // stable position across compactions. Record an identity
                // entry so a live Weak/Soft/Phantom reference watching this
                // address is not wrongly cleared.
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
        // PERF (gc-oldgen-perf): the buckets are about to be fully rewritten,
        // so the cached sorted free-block view is stale — invalidate it once.
        self.invalidate_sorted_free();
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

    /// Invoke `f` once for each in-old-gen reference target of `obj_ptr`.
    ///
    /// Mirrors the slot-walking logic in [`Self::update_refs_in_object`]
    /// (object fields are 16-byte `Value` slots; reference arrays are 8-byte
    /// compact pointers) but only *reads* targets — it never writes the slot.
    /// Targets outside `[data_start, data_end)` (young gen, metaspace, native)
    /// are filtered out, matching the bounds gate Phase 2 uses, so a caller
    /// only ever sees old-gen referents.
    ///
    /// The header is *not* borrowed across the `f` callback: the four scalar
    /// fields needed to drive the walk are copied out up front via
    /// `read_unaligned` on raw field pointers. This matters because a caller
    /// (e.g. the Phase 0 closure) may write to the *referent's* header from
    /// inside `f`, and for a self-referential object the referent is this very
    /// header — holding a live `&ObjectHeader` across that write would alias a
    /// `&mut` to the same bytes. Reading scalars up front keeps the borrow
    /// short and the walk sound under a self-loop.
    fn for_each_old_gen_ref(obj_ptr: *mut u8, data: &[u8], mut f: impl FnMut(usize)) {
        let data_start = data.as_ptr() as usize;
        let data_end = data_start + data.len();

        // Snapshot the layout-driving fields (incl. the compact oop-map Arc),
        // then drop the reference before any callback runs (see the aliasing
        // note above — `f` may mutate the object's header/fields).
        let (kind, element_type, array_length, num_slots, compact) = unsafe {
            let h = &*(obj_ptr as *const ObjectHeader);
            (
                h.kind,
                h.element_type,
                h.array_length,
                h.num_slots,
                crate::heap::compact_oop_scan(h),
            )
        };

        if kind == ObjectKind::Array {
            if element_type == ArrayElementType::Reference {
                for i in 0..array_length as usize {
                    let slot = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                    let raw: u64 = unsafe { std::ptr::read(slot as *const u64) };
                    if raw != 0 {
                        let ref_ptr = raw as usize;
                        if ref_ptr >= data_start && ref_ptr < data_end {
                            f(ref_ptr);
                        }
                    }
                }
            }
        } else if let Some((layout, body)) = compact {
            // Compact object: 8-byte reference slots at the oop-map offsets.
            for &off in &layout.ref_offsets {
                let off = off as usize;
                if off + crate::heap::REF_FIELD_SIZE > body {
                    break;
                }
                let slot = unsafe { obj_ptr.add(HEADER_SIZE + off) };
                let raw: u64 = unsafe { std::ptr::read(slot as *const u64) };
                if raw != 0 {
                    let ref_ptr = raw as usize;
                    if ref_ptr >= data_start && ref_ptr < data_end {
                        f(ref_ptr);
                    }
                }
            }
        } else {
            for slot_idx in 0..num_slots as usize {
                let slot = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                let value = unsafe { std::ptr::read(slot as *const Value) };
                if let Value::Object(Some(ref_obj)) = value {
                    let ref_ptr = ref_obj.as_ptr() as usize;
                    if ref_ptr >= data_start && ref_ptr < data_end {
                        f(ref_ptr);
                    }
                }
            }
        }
    }

    /// Promote any UNMARKED old-gen object reachable from a MARKED old-gen
    /// object to live (set `GC_FLAG_MARKED`), iterating to a fixpoint.
    ///
    /// This is the dangling-reference guard described in [`Self::compact`]
    /// Phase 0. After this returns, the marked set is closed under the
    /// old-gen reference relation, so every reference a live object holds to
    /// an in-old-gen target will see a non-null `forwarding_ptr` in Phase 2 —
    /// no live referrer can be left pointing at an un-relocated (and about to
    /// be overwritten/zeroed) target.
    ///
    /// `objects` is the address-ordered `(ptr, size)` list from
    /// [`Self::walk_objects`]; it is treated as the complete set of old-gen
    /// objects. The closure is conservative (it only ever *adds* survivors)
    /// so it can never drop a live object or corrupt the heap; the worst case
    /// is retaining a little floating garbage for one extra cycle.
    ///
    /// Cost: in the normal case (a transitively-correct marker) the first pass
    /// promotes nothing — every target of a live object is already live — so
    /// this is a single O(objects) walk and returns. Additional passes only
    /// occur when the marker under-marked (the exact failure this guards), and
    /// the fixpoint is bounded by the longest under-marked chain.
    fn close_live_set_over_old_gen(objects: &[(*mut u8, usize)], data: &[u8]) {
        // Fixpoint: keep re-scanning marked objects until a pass promotes
        // nothing. `objects` is finite and each pass can only flip flags
        // from 0→1, so this terminates in at most `objects.len()` passes.
        loop {
            let mut promoted_any = false;
            for &(obj_ptr, _size) in objects {
                // Snapshot the marked bit; don't hold a header borrow while the
                // closure below may write the same header (self-loop case).
                let is_marked =
                    unsafe { (*(obj_ptr as *const ObjectHeader)).gc_flags & GC_FLAG_MARKED != 0 };
                if !is_marked {
                    continue; // only trace *live* referrers
                }
                Self::for_each_old_gen_ref(obj_ptr, data, |ref_ptr| {
                    // SAFETY: `ref_ptr` is in-bounds (filtered by
                    // `for_each_old_gen_ref`) and points at an old-gen object
                    // header. The pre-pass runs before any relocation, so the
                    // referent is still at its original address. No outstanding
                    // borrow of this header is live here (fields were copied
                    // out before the walk), so the `&mut` does not alias.
                    let ref_header = unsafe { &mut *(ref_ptr as *mut ObjectHeader) };
                    if ref_header.gc_flags & GC_FLAG_MARKED == 0 {
                        ref_header.gc_flags |= GC_FLAG_MARKED;
                        promoted_any = true;
                    }
                });
            }
            if !promoted_any {
                break;
            }
        }
    }

    /// Update reference slots within a single live old-gen object so they point
    /// to forwarding addresses of moved objects.
    ///
    /// Handles both regular object fields (16-byte `Value` slots) and reference
    /// arrays (8-byte compact pointers).
    ///
    /// Dangling-ref invariant: by the time this runs, Phase 0
    /// ([`Self::close_live_set_over_old_gen`]) has promoted every in-old-gen
    /// target of a live object to live, and Phase 1 has stamped a forwarding
    /// address on every live object. Therefore any in-bounds target read here
    /// MUST have a non-null `forwarding_ptr`; a null one would mean a live
    /// referrer points at floating garbage and the rewrite below would be
    /// skipped, leaving a dangling pointer. We `debug_assert!` against that to
    /// catch a regression in the closure, and in release simply leave the slot
    /// untouched (the closure makes this case unreachable in practice).
    fn update_refs_in_object(obj_ptr: *mut u8, header: &ObjectHeader, data: &[u8]) {
        let data_start = data.as_ptr() as usize;
        let data_end = data_start + data.len();

        // Remap every reference into a relocated (forwarded) in-old-gen object.
        // Arrays + compact objects use 8-byte pointer slots; legacy objects use
        // 16-byte Value cells. `forward_ref_slots` rewrites a slot only when the
        // closure returns Some(new_addr).
        // SAFETY: `obj_ptr`/`header` are a valid live old-gen object under STW.
        unsafe {
            crate::gen_heap::forward_ref_slots(obj_ptr, header, |ref_ptr| {
                let r = ref_ptr as usize;
                if r < data_start || r >= data_end {
                    return None; // reference outside the compacted old-gen region
                }
                let ref_header = &*(ref_ptr as *const ObjectHeader);
                if !ref_header.forwarding_ptr.is_null() {
                    if seedhunt_enabled() && (ref_header.forwarding_ptr as usize) < 0x1000 {
                        eprintln!(
                            "[gcfwd] write small fwd: holder@0x{:x} cid={} \
                             referent@0x{:x} cid={} marked={} fwd=0x{:x}",
                            obj_ptr as usize,
                            header.class_id.as_u32(),
                            r,
                            ref_header.class_id.as_u32(),
                            ref_header.gc_flags & GC_FLAG_MARKED != 0,
                            ref_header.forwarding_ptr as usize,
                        );
                    }
                    Some(ref_header.forwarding_ptr)
                } else {
                    // Dangling-ref guard: an in-old-gen target with a null
                    // forwarding pointer is an UNMARKED object that Phase 0
                    // should have promoted. If this fires, the live closure
                    // missed an edge and leaving the slot as-is would dangle.
                    debug_assert!(
                        false,
                        "old_gen.compact: live holder@0x{:x} -> unmarked old-gen \
                         referent@0x{:x} (no forwarding addr); \
                         close_live_set_over_old_gen missed an edge",
                        obj_ptr as usize, r,
                    );
                    None
                }
            });
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
    fn alloc_alignment_reserves_compact_object_padding() {
        let mut og = OldGen::new(4096);
        let first = og.alloc(44, 8).unwrap();
        let second = og.alloc(8, 8).unwrap();

        assert_eq!(second as usize - first as usize, 48);
        // OldGen reserves at least one header for any request, so the second
        // 8-byte request occupies 40 bytes after the 48-byte compact extent.
        assert_eq!(og.used(), 88);
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

    /// Dangling-ref guard (Phase 0): a MARKED object that references an
    /// UNMARKED old-gen object must not be left pointing at floating garbage
    /// after compaction. The closure promotes the target to live and relocates
    /// it; the referrer's field must end up pointing at the target's *new*,
    /// still-valid location — never at zeroed/overwritten bytes.
    #[test]
    fn compact_promotes_unmarked_target_of_live_ref() {
        let mut og = OldGen::new(4096);

        // Object A: live, one reference field.
        let a_size = HEADER_SIZE + SLOT_SIZE;
        let a = og.alloc(a_size, 8).unwrap();

        // Dead filler between A and B so that, once it is reclaimed, B must
        // actually slide to a lower address during compaction (giving it a
        // pointer-map entry and exercising the relocation path).
        let filler_size = HEADER_SIZE + 4 * SLOT_SIZE;
        let filler = og.alloc(filler_size, 8).unwrap();
        unsafe {
            (*(filler as *mut ObjectHeader)).num_slots = 4;
            // left UNMARKED -> dead -> reclaimed.
        }

        // Object B: the (initially unmarked) target. Tag it with a distinctive
        // identity hash so we can prove we land on real B data, not zeroes.
        const B_TAG: i32 = 0x5EED_BEEF_u32 as i32;
        let b_size = HEADER_SIZE + SLOT_SIZE;
        let b = og.alloc(b_size, 8).unwrap();

        unsafe {
            // A is live, references B in field 0.
            let a_hdr = &mut *(a as *mut ObjectHeader);
            a_hdr.num_slots = 1;
            a_hdr.gc_flags |= GC_FLAG_MARKED;
            let a_field0 = a.add(HEADER_SIZE) as *mut Value;
            std::ptr::write(a_field0, Value::Object(Some(ObjectRef::from_raw(b))));

            // B is left UNMARKED (would be floating garbage without the guard).
            let b_hdr = &mut *(b as *mut ObjectHeader);
            b_hdr.num_slots = 1;
            b_hdr.identity_hash_code = B_TAG;
        }

        let map = og.compact();

        // A is the lowest-address live object, so it stays put (no map entry);
        // B must have been promoted and relocated, hence present in the map.
        let b_new = *map
            .get(&(b as usize))
            .expect("unmarked target reachable from a live ref must be promoted + relocated");

        unsafe {
            // A's field must now point at B's NEW location, in-bounds.
            let a_field0 = a.add(HEADER_SIZE) as *const Value;
            let field_val = std::ptr::read(a_field0);
            let Value::Object(Some(target)) = field_val else {
                panic!("A's reference field was clobbered: {field_val:?}");
            };
            assert_eq!(
                target.as_ptr() as usize,
                b_new,
                "A's field must be forwarded to B's new location, not left dangling",
            );

            // The forwarded B must still hold real B data (tag preserved),
            // proving we did not leave the slot pointing at zeroed memory.
            let b_hdr = &*(b_new as *const ObjectHeader);
            assert_eq!(
                b_hdr.identity_hash_code, B_TAG,
                "B data lost across compaction"
            );
            // GC metadata cleared on the survivor.
            assert!(b_hdr.forwarding_ptr.is_null());
            assert_eq!(b_hdr.gc_flags & GC_FLAG_MARKED, 0);
        }
    }

    /// RandomizedContext WeakHashMap<Thread,...> fix regression: a live
    /// old-gen object that stays in place during sliding compaction (the
    /// lowest-address survivor, exactly like object A in
    /// `compact_promotes_unmarked_target_of_live_ref` above) gets NO
    /// `pointer_map` entry by design — but if it is a WATCHED referent
    /// (`gc_quiescence::set_watched_referents`), it must get an IDENTITY
    /// entry so post-GC reference processing's `is_marked` check can
    /// recognize it as alive. An unwatched object that also stays in place
    /// must still get no entry at all (bounded cost — this must not start
    /// recording every stationary survivor).
    #[test]
    fn compact_records_identity_map_for_watched_stationary_survivor() {
        let mut og = OldGen::new(4096);

        // Object A: lowest address, stays in place after compaction.
        let a_size = HEADER_SIZE + SLOT_SIZE;
        let a = og.alloc(a_size, 8).unwrap();
        unsafe {
            let a_hdr = &mut *(a as *mut ObjectHeader);
            a_hdr.num_slots = 1;
            a_hdr.gc_flags |= GC_FLAG_MARKED;
        }

        // Object B: also stays in place (contiguous with A, nothing to
        // reclaim between them) — left UNWATCHED as a control.
        let b_size = HEADER_SIZE + SLOT_SIZE;
        let b = og.alloc(b_size, 8).unwrap();
        unsafe {
            let b_hdr = &mut *(b as *mut ObjectHeader);
            b_hdr.num_slots = 1;
            b_hdr.gc_flags |= GC_FLAG_MARKED;
        }

        crate::gc_quiescence::set_watched_referents(&[a as usize]);
        let map = og.compact();
        crate::gc_quiescence::set_watched_referents(&[]);

        assert_eq!(
            map.get(&(a as usize)),
            Some(&(a as usize)),
            "watched stationary survivor must get an identity pointer_map entry"
        );
        assert!(
            !map.contains_key(&(b as usize)),
            "unwatched stationary survivor must NOT get a pointer_map entry (bounded cost)"
        );
    }
}
