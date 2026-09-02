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

use crate::gc_flags;
use crate::heap::{
    array_data_size, array_element_type_from_tag, object_kind_from_tag, ArrayElementType,
    ObjectHeader, ObjectKind, ARRAY_DATA_OFFSET, GC_FLAG_MARKED, HEADER_SIZE, REF_ELEMENT_SIZE,
    SLOT_SIZE,
};
use cratonvm_types::narrow_oop::{read_ref_slot, ref_element_size, ref_field_size};
use cratonvm_types::{ObjectRef, Value};

/// Cached `CRATONVM_DBG_SEEDHUNT` gate (bc math-ec `0x4`). When on,
/// `update_refs_in_object` logs any referent whose `forwarding_ptr` is
/// non-null but `< 0x1000` — the §6.1 suspect that would write
/// `Object(Some(0x4))` into a live referrer's field during major-GC
/// compaction. See gaps/bc-math-ec-gc-0x4-handoff.md §6.1.
#[inline]
fn seedhunt_enabled() -> bool {
    gc_flags().dbg_seedhunt
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

/// How many times [`OldGen::coalesce_free_blocks`] ran, and how many free
/// blocks it eliminated. Reported by `VmHeap::print_gc_summary` so "is the
/// old-gen coalescer doing anything in this workload?" is answerable from a
/// log instead of a debugger — the question that decides whether a
/// fragmentation fix is load-bearing or an inert lever.
pub static COALESCE_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static BLOCKS_MERGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many times [`OldGen::scan_region`]/[`OldGen::scan_region_filtered`]
/// found an invalid `ObjectKind`/`ArrayElementType` tag byte at what should
/// have been an object boundary and stopped that scan stripe instead of
/// trusting it.
///
/// A non-zero value means the walk desynced from real object headers and
/// landed on payload bytes (a stale free-list sliver, a mis-sized prior
/// object, an unparsed TLAB tail, ...). Before this guard existed, that same
/// desync read the payload byte as a typed `#[repr(u8)]` enum — instant UB
/// for a discriminant outside the declared set, which optimized code can
/// lower to a hardware trap (`HIB-DCAST-LATEPHASE.1`,
/// `fixed-suite-bugs/source-debug-jit-conservative-root-invalid-header-tag-sigill.md`
/// fixed the same class of bug for the conservative-root validators but
/// never reached this walk). This counter turns that silent-until-it-traps
/// failure mode into something a regression test or a log can see.
pub static WALK_DESYNC_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// GCAUD-2: how many old-gen compactions were ABANDONED because Phase 0's
/// live-set closure escaped the object walk (see
/// [`OldGen::close_live_set_over_old_gen`]). A non-zero value means the
/// generation was retained wholesale for that cycle — a reclamation the
/// collector declined to make rather than manufacture a dangling slot — and
/// that some earlier phase left a live reference to an unwalked address.
/// Silent before this counter existed; a compaction that reclaims nothing and
/// a compaction that had nothing to reclaim look identical from outside.
pub static COMPACT_ESCAPE_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// GCAUD-9 (2026-08-03): how many old-gen compactions were ABANDONED because
/// `walk_objects` covered fewer bytes than `used_bytes` says are allocated —
/// i.e. some region's `scan_region` broke early on an implausible header and
/// left real (possibly live, possibly overlay-only-referenced) memory outside
/// the walked set. See the check at the top of [`OldGen::compact`]. Distinct
/// from `COMPACT_ESCAPE_HITS`: that counter fires when a walked, MARKED
/// object's ordinary field points outside the walk; this one fires when the
/// walk itself is incomplete, which `COMPACT_ESCAPE_HITS`'s ordinary-field
/// closure cannot detect for an object reachable only through a Rust-side
/// overlay side table.
pub static COMPACT_WALK_GAP_HITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// GCAUD-9 follow-up (2026-08-03): how many times `scan_region` broke a
/// region's walk early on an implausible header. Distinct from
/// `COMPACT_WALK_GAP_HITS` (which counts abandoned *compactions*, one per
/// GC cycle): this counts every individual break, including ones a later
/// `compact()` call re-discovers at the exact same offset because nothing
/// upstream has fixed the underlying header. Gates the raw-byte dump in
/// [`scan_region`]'s break arm to the first few hits so a persistent,
/// unmoving break point doesn't spam every subsequent GC cycle.
pub static SCAN_REGION_BREAK_HITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

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
    /// GCAUD-4 — monotone stamp of "old-gen storage has been RECLAIMED or
    /// RELOCATED since you last looked".
    ///
    /// Every side table an out-of-STW consumer keys on an old-gen ADDRESS
    /// (`ConcurrentMarker`'s mark bitmap and its `sweep_eligible` snapshot are
    /// the live examples) is only valid while no other collector has handed a
    /// block back to the free list or slid an object. Both events make an
    /// address ambiguous: after `compact` the address names a different
    /// object, and after `free` + a later `alloc` it names a brand-new one —
    /// and in both cases the stale table says "existed at remark, not marked",
    /// which is the sweep's licence to FREE it.
    ///
    /// This is the generational-heap analogue of G1's
    /// `recycled_in_generation` stamp (defect G1-8): a bare address is not an
    /// identity, an address plus an epoch is. Bumped by [`Self::free`] and by
    /// [`Self::compact`]; read via [`Self::reclaim_epoch`].
    reclaim_epoch: u64,
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
            reclaim_epoch: 0,
        }
    }

    /// GCAUD-4 — the current reclamation epoch (see the field doc).
    ///
    /// A consumer that caches anything keyed on an old-gen address across a
    /// window in which it does NOT hold the old-gen lock must record this
    /// value and re-check it before acting on the cache. An unequal value
    /// means at least one block was freed or relocated in between, so every
    /// cached address may now name a different object; the only safe response
    /// is to discard the cache.
    #[inline]
    pub fn reclaim_epoch(&self) -> u64 {
        self.reclaim_epoch
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
        self.alloc_impl(size, align, true)
    }

    /// [`Self::alloc`] without the zeroing pass.
    ///
    /// For a caller that overwrites every byte it is handed before anything
    /// can read the block: the parallel evacuator's promotion buffers
    /// (`gen_evac`), where the copy is the write. Zeroing there was one
    /// `memset` of every promoted byte followed by one `memcpy` over it.
    ///
    /// The caller inherits the obligation `alloc`'s zeroing discharged: no
    /// walker may reach the block before it holds objects end to end, and an
    /// unused tail must go back through [`Self::release_unused_tail`] before
    /// the next `walk_objects`.
    pub fn alloc_unzeroed(&mut self, size: usize, align: usize) -> Option<*mut u8> {
        self.alloc_impl(size, align, false)
    }

    fn alloc_impl(&mut self, size: usize, align: usize, zero: bool) -> Option<*mut u8> {
        if let Some(p) = self.alloc_from_buckets(size, align, zero) {
            return Some(p);
        }
        // FRAGMENTATION FIX (xt-helper-window OOM, 2026-07-31): the free list
        // is size-segregated and `free` never looks at a block's neighbours,
        // because coalescing was deferred to `compact`. When compaction cannot
        // run — the old generation reclaimed IN PLACE by
        // `old_gen_gc(compact = false)` under conservative JIT roots — that
        // deferral never comes due, and the free list degenerates into one
        // isolated block per dead object: the SUM of free bytes stays large
        // while the LARGEST block shrinks toward a single object, so a modest
        // array request fails on a generation that is mostly free. Before
        // reporting failure, merge adjacent blocks once and retry. Costs
        // nothing on the success path, and turns a spurious `OutOfMemoryError`
        // into an allocation whenever the bytes are physically there.
        if self.coalesce_free_blocks() > 0 {
            return self.alloc_from_buckets(size, align, zero);
        }
        None
    }

    /// Merge adjacent (and, defensively, overlapping) free blocks into maximal
    /// runs and rebuild the size buckets. Returns how many blocks the merge
    /// eliminated — `0` means the list was already maximally coalesced and a
    /// retry cannot help.
    ///
    /// `compact` rebuilds the free list as a single trailing block and so has
    /// never needed this; it exists for the paths that reclaim old-gen storage
    /// WITHOUT compacting (`old_gen_gc(compact = false)`, reached whenever a
    /// live JIT frame's roots are conservative and therefore un-rewritable).
    ///
    /// O(n log n) in the number of free blocks. Called once per in-place
    /// old-gen sweep and once as a last-ditch step before `alloc` gives up, so
    /// it is never on a hot path.
    pub fn coalesce_free_blocks(&mut self) -> usize {
        COALESCE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if crate::gc_flags().no_oldgen_coalesce {
            return 0;
        }
        let before: usize = self.buckets.iter().map(|b| b.len()).sum();
        if before < 2 {
            return 0;
        }
        let mut blocks: Vec<FreeBlock> = self.buckets.iter().flatten().copied().collect();
        blocks.sort_unstable_by_key(|b| b.offset);

        // H2-CID0 (2026-08-02): the list is sorted here anyway, so one
        // comparison per block answers "did something free the same span
        // twice?". The young sweep has had `DOUBLE_FREE_SPANS` since
        // 2026-08-01; old gen had no equivalent, and a double free there
        // produces the same all-zero-header face (the allocator serves the
        // bytes twice and `alloc` zeroes them under the first owner).
        {
            let mut overlaps = 0usize;
            let mut first: Option<(usize, usize, usize)> = None;
            for w in blocks.windows(2) {
                if w[0].offset + w[0].size > w[1].offset {
                    overlaps += 1;
                    if first.is_none() {
                        first = Some((w[0].offset, w[0].size, w[1].offset));
                    }
                }
            }
            if overlaps > 0 {
                crate::gen_heap::OLD_FREE_LIST_OVERLAPS
                    .fetch_add(overlaps as u64, std::sync::atomic::Ordering::Relaxed);
                static REPORTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                if REPORTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                    let (o, s, n) = first.unwrap_or((0, 0, 0));
                    tracing::error!(
                        target: "cratonvm::gc::guard",
                        pairs = overlaps,
                        first = format!("+{o:#x}+{s:#x} overlaps +{n:#x}"),
                        "old-gen free list contains OVERLAPPING blocks — a span was freed \
                         twice. The allocator can serve the same bytes to two objects and \
                         zero them under the first owner.",
                    );
                }
            }
        }

        let mut merged: Vec<FreeBlock> = Vec::with_capacity(blocks.len());
        for b in blocks {
            match merged.last_mut() {
                // STRICT adjacency only. The union of two touching free spans
                // is provably free, so merging them can never hand out live
                // memory. An OVERLAPPING pair is deliberately left alone: it
                // means something already double-freed or over-freed a span,
                // and silently collapsing it would both mask that bug and risk
                // widening a block over a live object. `walk_objects` derives
                // "allocated" as the GAPS between free blocks, so this
                // conservative choice also leaves the object walk unchanged.
                Some(last) if last.offset + last.size == b.offset => {
                    last.size += b.size;
                }
                _ => merged.push(b),
            }
        }

        if merged.len() == before {
            // Nothing was adjacent — leave the buckets untouched so the caller
            // knows a retry is pointless.
            return 0;
        }
        for bucket in &mut self.buckets {
            bucket.clear();
        }
        for b in &merged {
            self.buckets[bucket_for(b.size)].push(*b);
        }
        self.invalidate_sorted_free();
        let eliminated = before - merged.len();
        BLOCKS_MERGED.fetch_add(eliminated as u64, std::sync::atomic::Ordering::Relaxed);
        eliminated
    }

    /// The size-segregated bucket walk. Split out of [`Self::alloc`] so the
    /// coalesce-and-retry step can run it twice.
    fn alloc_from_buckets(&mut self, size: usize, align: usize, zero: bool) -> Option<*mut u8> {
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
            if zero {
                unsafe {
                    std::ptr::write_bytes(ptr, 0, size);
                }
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
        //
        // GCAUD-1 (2026-08-01): the clause above was only HALF of `alloc`'s
        // rounding. `alloc_from_buckets` follows the `max` with
        // `size.checked_add(align - 1).map(|v| v & !(align - 1))`, i.e. it
        // rounds the reservation UP to a multiple of `align`, charges
        // `used_bytes` that rounded amount, and starts the next object there.
        // `free` did not round, so an unrounded `free(ptr, 44)` after
        // `alloc(44, 8)` returned a 44-byte block covering a 48-byte
        // reservation and left a 4-byte sliver OUTSIDE the free list. That
        // sliver reads as ALLOCATED to `walk_objects` (which derives allocated
        // extents as the gaps between free blocks), so the walk resumes at a
        // non-object-start, mis-parses it as a header, and `break`s — dropping
        // every object after it from the walk. `compact` then treats those
        // objects as absent and Phase 4 rebuilds the free list over them.
        //
        // Every production caller passes an already-8-aligned `total_size`
        // from `walk_objects`, and every production `alloc` uses `align = 8`,
        // so the asymmetry is latent rather than live — the same shape as the
        // TLAB audit's T-2. Rounding UP to 8 here is exactly `alloc`'s
        // arithmetic for `align <= 8` and is strictly conservative for a
        // larger alignment (it returns no more than was reserved, so it can
        // never hand a live neighbour's bytes to the free list).
        let size = (size.max(HEADER_SIZE.max(8)) + 7) & !7;

        let base = self.data.as_ptr() as usize;
        let addr = ptr as usize;
        debug_assert!(addr >= base && addr + size <= base + self.data.len());
        let offset = addr - base;

        self.used_bytes = self.used_bytes.saturating_sub(size);
        // GCAUD-4: this address may be handed to a different object by the
        // next `alloc`, so every address-keyed cache taken before now is
        // ambiguous from here on.
        self.reclaim_epoch = self.reclaim_epoch.wrapping_add(1);

        // Push into the appropriate size bucket — O(1).
        // Coalescing happens during the next `compact()` call.
        self.buckets[bucket_for(size)].push(FreeBlock { offset, size });
        // PERF (gc-oldgen-perf): a new free block changes the sorted view.
        self.invalidate_sorted_free();
    }

    /// Hand back the NEVER-WRITTEN tail of a block [`Self::alloc_unzeroed`]
    /// carved, without stamping [`Self::reclaim_epoch`].
    ///
    /// [`Self::free`] bumps the epoch because the bytes it releases held an
    /// object whose address a concurrent marker's remark snapshot may still
    /// name; hand the address to a new object and "existed at remark, not
    /// marked" becomes a licence to free something live. A promotion buffer's
    /// tail never held an object: it was free space at every remark that
    /// preceded this pause (a block that was live at the remark reaches the
    /// buckets only through `free`, which bumped the epoch already), so no
    /// snapshot can name any address in it, and returning it changes nothing
    /// any address-keyed cache believes. That is why a young collection can
    /// retire its buffers every cycle without invalidating an in-flight
    /// concurrent old-gen sweep.
    ///
    /// # Safety
    /// `[ptr, ptr + size)` must be the unused tail of a block obtained from
    /// this old gen, at least `HEADER_SIZE` bytes, 8-aligned, and never
    /// written since it was carved.
    pub unsafe fn release_unused_tail(&mut self, ptr: *mut u8, size: usize) {
        debug_assert!(size >= HEADER_SIZE && size % 8 == 0 && (ptr as usize) % 8 == 0);
        let base = self.data.as_ptr() as usize;
        let addr = ptr as usize;
        debug_assert!(addr >= base && addr + size <= base + self.data.len());
        let offset = addr - base;
        self.used_bytes = self.used_bytes.saturating_sub(size);
        self.buckets[bucket_for(size)].push(FreeBlock { offset, size });
        self.invalidate_sorted_free();
    }

    /// Returns true if the given pointer falls within this old generation's storage.
    pub fn contains(&self, ptr: *const u8) -> bool {
        let base = self.data.as_ptr();
        let end = unsafe { base.add(self.data.len()) };
        ptr >= base && ptr < end
    }

    /// True when `ptr` lies inside an **allocated** span of old gen.
    ///
    /// [`Self::contains`] is a bare range check over the whole backing store,
    /// so it answers `true` for memory that has already been returned to the
    /// free list — the non-moving old-gen sweep (`old_gen_gc(compact = false)`)
    /// reclaims dead blocks IN PLACE and does not zero them, so the dead
    /// object's bytes stay put and the address keeps passing `contains`. This
    /// is the discriminator a liveness query needs; see
    /// `fixed-suite-bugs/gc-old-gen-mark-accepts-unvalidated-addresses-FIXED.md`.
    ///
    /// O(log n) in the free-block count, over the same offset-sorted view
    /// `walk_objects` already caches.
    pub fn is_allocated_addr(&self, ptr: *const u8) -> bool {
        let base = self.data.as_ptr() as usize;
        let addr = ptr as usize;
        if addr < base || addr >= base + self.data.len() {
            return false;
        }
        let off = addr - base;
        self.with_sorted_free_blocks(|sorted| {
            // Free blocks are disjoint and offset-sorted, so the last block
            // starting at or before `off` is the only one that can cover it.
            let i = sorted.partition_point(|b| b.offset <= off);
            i == 0 || {
                let b = &sorted[i - 1];
                off >= b.offset + b.size
            }
        })
    }

    /// Backing-storage extent as plain integers: `[lo, hi)`.
    ///
    /// `OldGen` is not `Sync` (it owns the storage), so a parallel young-sweep
    /// worker cannot hold a `&OldGen` just to run `contains`. This exposes the
    /// same range test as two `usize`s the workers can copy.
    pub fn extent(&self) -> (usize, usize) {
        let base = self.data.as_ptr() as usize;
        (base, base + self.data.len())
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
                // `max` because the free list is only guaranteed sorted by
                // OFFSET, not disjoint. A block nested inside its predecessor
                // (`[100,300)` then `[150,200)`) would otherwise pull the cursor
                // back to 200 and the next gap would be walked from inside the
                // first block — parsing free bytes as object headers, which is
                // how a phantom header comes to subsume live objects. Nested
                // blocks mean a double free (see `OLD_FREE_LIST_OVERLAPS`); this
                // makes the walk safe while that is being diagnosed rather than
                // silently mis-parsing.
                cursor = cursor.max(block.offset + block.size);
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

    /// Validate the `ObjectKind`/`ArrayElementType` tag bytes at `ptr` before
    /// any walker constructs a typed `&ObjectHeader` there.
    ///
    /// `scan_region`/`scan_region_filtered` derive object boundaries purely
    /// from arithmetic (`offset += total_size`), trusting that the cursor
    /// always lands on a real header. When that trust is violated — by any
    /// of the several walk-desync causes this file documents elsewhere, or
    /// one not yet found — the cursor lands on arbitrary payload bytes
    /// (a Java `int`/`long`/pointer field, leftover free-list bytes, ...).
    /// `ObjectHeader::kind`/`element_type` are `#[repr(u8)]` enums with only
    /// a handful of valid discriminants; loading an out-of-range byte into
    /// either as a *typed* enum is immediate undefined behaviour, and
    /// optimized code is free to lower that UB into a hardware trap rather
    /// than doing anything resembling "the wrong thing but not crashing" —
    /// which is exactly the `SIGILL` in `OldGen::compact` this fixes
    /// (`HIB-DCAST-LATEPHASE.1`; faulting RVA was identical across repeated
    /// crashes, i.e. a deterministic trap site, not stack-smash noise).
    ///
    /// Mirrors the fix already applied to the conservative-root validators
    /// in `gen_heap.rs`/`g1.rs` (see
    /// `fixed-suite-bugs/source-debug-jit-conservative-root-invalid-header-tag-sigill.md`):
    /// read the raw tag bytes and validate them through
    /// `object_kind_from_tag`/`array_element_type_from_tag` *before* ever
    /// forming a `&ObjectHeader` reference and touching the typed field.
    ///
    /// Returns the validated `ObjectKind` on success. Returns `None` — and
    /// bumps [`WALK_DESYNC_HITS`] — when either tag byte is not a valid
    /// discriminant; the caller must treat this exactly like the existing
    /// `HumongousFiller`/implausible-size guards and stop scanning the
    /// current stripe rather than trust the cursor further.
    fn validate_header_tags_or_desync(ptr: *mut u8, offset: usize) -> Option<ObjectKind> {
        // SAFETY: `ptr` is `base + offset` where `offset` falls inside the
        // `[start_offset, end_offset)` sub-range of an allocated region within
        // `self.data` that the caller is scanning, so the two single-byte tag
        // reads at the fixed header offsets are in-bounds. Reading a `u8`
        // through a raw pointer has no validity requirement beyond
        // in-bounds-and-readable, so this cannot itself be the UB the rest of
        // this function exists to avoid.
        let kind_tag = unsafe { cratonvm_types::kind_tag_at(ptr) };
        let elem_tag = unsafe { cratonvm_types::element_type_tag_at(ptr) };
        let kind = object_kind_from_tag(kind_tag);
        let element_type = array_element_type_from_tag(elem_tag);
        match (kind, element_type) {
            (Some(kind), Some(_)) => Some(kind),
            _ => {
                WALK_DESYNC_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(
                    offset,
                    kind_tag,
                    elem_tag,
                    "old-gen walk: invalid ObjectKind/ArrayElementType tag at a walk \
                     boundary — the walk has desynced from real object headers; \
                     stopping this scan stripe instead of reading a corrupt header as \
                     a typed enum. See \
                     fixed-suite-bugs/source-debug-jit-conservative-root-invalid-header-tag-sigill.md."
                );
                None
            }
        }
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
        // PERF 2026-08-02: `dirty_ranges` is sorted ascending, non-overlapping
        // and coalesced (see `gen_heap::scan_dirty_cards`, which sorts the card
        // indices and merges adjacent cards before calling here), and `offset`
        // only ever increases across this loop. So the range that could contain
        // `offset` can be tracked with a monotone cursor. This used to be a
        // `dirty_ranges.iter().any(..)` per object, making the dirty-card scan
        // O(old-gen objects x dirty ranges) — quadratic in the size of the old
        // generation once a workload promotes a lot and dirties a lot.
        //
        // Measured 2026-08-02, `perf record -F 199` against
        // `BeanRegistrationsAotContributionTests
        // #applyToWithVeryLargeBeanDefinitionsCreatesSeparateSourceFiles` (10001
        // generated bean definitions, so a large old gen and a large dirty set):
        // `scan_region_filtered` was **73.7% of ALL CPU samples** before this
        // change and does not appear in the profile at all after it.
        //
        // Scope the claim honestly: removing that 73.7% did NOT make that test
        // pass — at the time it still exhausted the heap in javac, it just got
        // there sooner. (The test passes as of 2026-08-06, for an unrelated
        // reason: see
        // `beanregistrations-verylarge-heap-footprint-FIXED-20260806.md`.)
        // And on the 1001-definition
        // sibling, whose old gen is small enough that the quadratic never bites,
        // an alternating 2-binary A/B is within noise (221 s vs 229 s). The
        // justification for this change is the algorithm and the profile, not a
        // wall-clock win on any particular test.
        let mut range_idx = dirty_ranges.partition_point(|&(_, end)| end <= offset);
        while offset < end_offset {
            let ptr = (base + offset) as *mut u8;
            let Some(kind) = Self::validate_header_tags_or_desync(ptr, offset) else {
                break;
            };
            // SAFETY: `offset` is a valid object boundary within an
            // allocated region of the data buffer (see `scan_region`), and
            // the kind/element_type tag bytes were just validated above.
            let header = unsafe { &*(ptr as *const ObjectHeader) };
            if kind == ObjectKind::HumongousFiller {
                break;
            }
            let raw_size = if kind == ObjectKind::Array {
                ARRAY_DATA_OFFSET
                    + array_data_size(header.array_length() as usize, header.element_type())
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
            // Advance the cursor past every range that ends at or before this
            // object; what remains is the only range that can contain it.
            while range_idx < dirty_ranges.len() && dirty_ranges[range_idx].1 <= offset {
                range_idx += 1;
            }
            let Some(&(range_start, _)) = dirty_ranges.get(range_idx) else {
                // Past the last dirty range: no later object in this region can
                // qualify, and the caller derives its own cursor from the free
                // list rather than from how far this walk got.
                return;
            };
            if offset >= range_start {
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
            let Some(kind) = Self::validate_header_tags_or_desync(ptr, offset) else {
                break;
            };
            // SAFETY: the kind/element_type tag bytes were just validated above.
            let header = unsafe { &*(ptr as *const ObjectHeader) };
            // Round-9 gc CRIT-1: HumongousFiller is a synthetic walker
            // sentinel installed by the regional GC (see `g1.rs` and
            // `region.rs`). It should never appear in the non-regional
            // old-gen layout, but defensively skip the rest of the
            // current scan stripe instead of mis-parsing it as a real
            // object (which would corrupt the offset cursor).
            if kind == ObjectKind::HumongousFiller {
                break;
            }
            let raw_size = if kind == ObjectKind::Array {
                ARRAY_DATA_OFFSET
                    + array_data_size(header.array_length() as usize, header.element_type())
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
                let n = SCAN_REGION_BREAK_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n < 8 {
                    // Raw header bytes, not the typed struct: the whole point
                    // is that this header may not be trustworthy to decode as
                    // one, and a `Debug` format on an out-of-range `kind`
                    // walked exactly this wild-pointer bug once already (see
                    // fixed-suite-bugs/gc-old-gen-mark-accepts-unvalidated-addresses-FIXED.md).
                    // SAFETY: `ptr` is inside the allocated region between
                    // `start_offset` and `end_offset`, both within `self.data`;
                    // HEADER_SIZE bytes at `ptr` are therefore in-bounds.
                    let raw_bytes: [u8; HEADER_SIZE] =
                        unsafe { std::ptr::read(ptr as *const [u8; HEADER_SIZE]) };
                    tracing::warn!(
                        offset,
                        end_offset,
                        total_size,
                        raw_size,
                        kind = ObjectHeader::kind_tag(header.mark_word.load(std::sync::atomic::Ordering::Relaxed)),
                        element_type = header.element_type() as u8,
                        array_length = header.array_length(),
                        num_slots = header.num_slots(),
                        bytes = ?raw_bytes,
                        "old-gen scan_region: BREAK on implausible header — dumping raw bytes \
                         so the corruption can finally be seen instead of inferred",
                    );
                }
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
    pub fn compact(&mut self) -> cratonvm_types::PointerMap {
        self.compact_with_drop_flags(&HashMap::new())
    }

    /// [`Self::compact`], plus the caller's per-block explanation of why the
    /// mark could have missed each block this compaction is about to drop.
    ///
    /// H2-CID0: the flags are threaded through to the old-gen reclamation ring
    /// so a `checkcast` failing on a stale reference minutes later says WHY the
    /// block was unmarked, not just that it was. See
    /// `gen_heap::OLD_FREED_FLAG_WATCHED`.
    pub fn compact_with_drop_flags(
        &mut self,
        drop_flags: &HashMap<usize, u8>,
    ) -> cratonvm_types::PointerMap {
        let base = self.data.as_mut_ptr();
        let objects = self.walk_objects();
        // GCAUD-4: bumped up front, so it covers both abandoned paths below
        // (this one and Phase 0's) — each clears mark bits, which is itself a
        // change no address-keyed snapshot taken earlier may assume away.
        self.reclaim_epoch = self.reclaim_epoch.wrapping_add(1);

        // GCAUD-9 (2026-08-03): `walk_objects`/`scan_region` deliberately
        // `break`s a region's scan early on an implausible header ("Sanity
        // check: if total_size is 0 or too large, stop scanning") rather than
        // re-syncing — a safe recovery for a diagnostic READ, but `compact`
        // does not just read `objects`: Phase 3 below PHYSICALLY OVERWRITES
        // memory outside it by sliding survivors into the freed space. Any
        // object `scan_region` silently dropped from a region it broke out of
        // early — including a correctly-marked, live one, if the ONLY thing
        // still pointing at it is a Rust-side overlay side table
        // (`lhm_overlay`/`ll_overlay`/etc., which Phase 0's escape check below
        // cannot see — it only walks ordinary header ref-slots) — never
        // enters `live_objects`, gets no `pointer_map` entry, and has its
        // memory handed to whatever survivor Phase 3 slides on top of it.
        // Every live reference to it (an overlay entry, in particular) then
        // points at that survivor's data instead — exactly the "unrelated
        // object's bytes read back through a stale-but-not-obviously-wrong
        // pointer" shape the residual corruption in
        // fixed-suite-bugs/hibernate/map-resize-unpinned-chain-cursors-nojit-segv-20260731-FIXED.md
        // keeps presenting as (Follow-up 4).
        //
        // `used_bytes` is independently maintained by `alloc`/`free` — it is
        // the ground truth for "how many bytes are currently allocated
        // (live or dead-but-unfreed)" and does not depend on re-parsing
        // headers. A complete, un-broken walk must account for exactly that
        // many bytes (every allocated byte belongs to exactly one walked
        // object; free bytes are excluded by construction — `scan_region` is
        // only ever called on the gaps BETWEEN free blocks). If it does not,
        // some region's scan broke early and there is live-or-dead-but-real
        // allocated memory this compaction cannot see — abandon it and
        // over-retain for one more cycle, the same fail-safe direction Phase
        // 0's escape check already takes below, rather than risk physically
        // overwriting memory whose occupant is unknown.
        let walked_bytes: usize = objects.iter().map(|&(_, sz)| sz).sum();
        if walked_bytes != self.used_bytes {
            let n = COMPACT_WALK_GAP_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 8 {
                tracing::warn!(
                    walked_bytes,
                    used_bytes = self.used_bytes,
                    walked_objects = objects.len(),
                    "old-gen compaction ABANDONED: walk_objects covered fewer bytes than are \
                     allocated -- a region's scan broke early on an implausible header and left \
                     live-or-dead memory outside the walked set. Sliding survivors into it would \
                     overwrite an object nothing in this walk can prove is dead. Retaining the \
                     whole generation for this cycle instead.",
                );
            }
            for &(obj_ptr, _size) in &objects {
                // SAFETY: `walk_objects` yielded this as a valid object start.
                let header = unsafe { &mut *(obj_ptr as *mut ObjectHeader) };
                header.clear_gc_flags(GC_FLAG_MARKED);
            }
            return cratonvm_types::PointerMap::default();
        }

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
        // GCAUD-2 (2026-08-01): Phase 0 also answers "can Phase 1 give every
        // reference a live object holds a forwarding address?". When it
        // reports an ESCAPE — a live referrer pointing at an in-old-gen
        // address that `walk_objects` did not yield — the answer is no, and
        // sliding anything is guaranteed to leave that slot dangling. Abandon
        // the compaction: clear the marks this cycle set (so the postcondition
        // "no survivor leaves with GC_FLAG_MARKED" still holds and the next
        // cycle starts clean), touch neither the free list nor `used_bytes`,
        // and return an empty map so the caller runs no fixups. Nothing is
        // reclaimed this cycle; over-retention is the only safe response.
        if Self::close_live_set_over_old_gen(&objects, &self.data).1 {
            let n = COMPACT_ESCAPE_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 8 {
                tracing::warn!(
                    walked_objects = objects.len(),
                    "old-gen compaction ABANDONED: a live object references an in-old-gen \
                     address the object walk did not yield (freed block or walk desync). \
                     Relocating would leave that slot dangling; retaining the whole \
                     generation for this cycle instead.",
                );
            }
            for &(obj_ptr, _size) in &objects {
                // SAFETY: `walk_objects` yielded this as a valid object start.
                let header = unsafe { &mut *(obj_ptr as *mut ObjectHeader) };
                header.clear_gc_flags(GC_FLAG_MARKED);
            }
            return cratonvm_types::PointerMap::default();
        }

        // Phase 1: Compute forwarding addresses for live objects.
        // `write_cursor` tracks the next available byte offset (8-byte aligned).
        let mut write_cursor: usize = 0;
        let mut pointer_map: cratonvm_types::PointerMap = cratonvm_types::PointerMap::default();
        // (src_ptr, total_size, dest_ptr, saved_mark) for each live object.
        //
        // `saved_mark` exists because of the 32 -> 24 header shrink. Forwarding
        // now lives in the mark word, and this is a SLIDING compactor: it
        // installs the forward in Phase 1 and copies in Phase 3, so the write
        // that says "forwarded" destroys the object's lock state BEFORE the copy
        // that would have carried it to the destination. A copying collector
        // does not have this problem (it copies first, then clobbers the
        // source); this one does, and the failure would be silent — every thin
        // lock in the old generation reset, and every INFLATED word's single
        // strong `Arc<Monitor>` reference dropped on the floor, leaving the slid
        // survivor with no monitor. Snapshot before clobbering, restore after
        // the copy. See `arch-2026-07-26/header-shrink.md` §4.3.
        let mut live_objects: Vec<(*mut u8, usize, *mut u8, u64)> = Vec::new();

        for &(obj_ptr, total_size) in &objects {
            let header = unsafe { &mut *(obj_ptr as *mut ObjectHeader) };
            if header.gc_flags() & GC_FLAG_MARKED == 0 {
                // H2-CID0 (2026-08-01): remember what this address held before
                // compaction drops it. The storage either ends up under a slid
                // survivor or inside the zeroed tail Phase 4 writes, and in the
                // latter case a still-live reference to it reads an all-zero
                // header — `ClassId(0)`, i.e. `java.lang.Object`. Recording the
                // class here is what lets the `checkcast` reporter say WHICH
                // object was reclaimed instead of only WHERE.
                crate::gen_heap::record_old_freed(
                    obj_ptr as usize,
                    total_size,
                    header.class_id.as_u32(),
                    ObjectHeader::kind_tag(
                        header.mark_word.load(std::sync::atomic::Ordering::Relaxed),
                    ),
                    crate::gen_heap::OLD_FREED_SITE_COMPACT,
                    drop_flags.get(&(obj_ptr as usize)).copied().unwrap_or(0),
                );
                continue; // Dead object — skip
            }
            let aligned = (write_cursor + 7) & !7;
            let dest = unsafe { base.add(aligned) };

            // Store forwarding address in header for Phase 2 reference updates.
            // Snapshot the mark word first — installing the forward overwrites
            // it (see the `live_objects` note above).
            let saved_mark = header.mark_word.load(std::sync::atomic::Ordering::Relaxed);
            header.set_forwarding_address(dest);

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
            live_objects.push((obj_ptr, total_size, dest, saved_mark));
            write_cursor = aligned + total_size;
        }

        // Phase 2: Update references within live old-gen objects.
        // Each reference slot that points to a moved old-gen object is rewritten
        // to the object's forwarding address.
        for &(obj_ptr, _, _, _) in &live_objects {
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            Self::update_refs_in_object(obj_ptr, header, &self.data);
        }

        // Phase 3: Slide objects to their forwarding addresses.
        // Processing order is low-to-high by source address, and dest ≤ src
        // for sliding compaction, so `std::ptr::copy` (memmove) is safe even
        // for overlapping regions.
        for &(src, size, dest, saved_mark) in &live_objects {
            if src != dest {
                unsafe { std::ptr::copy(src, dest, size) };
            }
            // Clear GC metadata on the (possibly moved) object. Restoring the
            // snapshot both retires the forward (the mark word IS the forwarding
            // slot now) and gives the survivor back the lock state Phase 1
            // overwrote — NEUTRAL, a thin lock's owner/recursion, or an INFLATED
            // word and the one strong `Arc<Monitor>` reference it owns.
            let final_header = unsafe { &mut *(dest as *mut ObjectHeader) };
            final_header
                .mark_word
                .store(saved_mark, std::sync::atomic::Ordering::Relaxed);
            final_header.clear_gc_flags(GC_FLAG_MARKED);
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
    /// `total_size` MUST be the size `walk_objects` computed for this exact
    /// object (`HEADER_SIZE + object_body_size(..)` at walk time) and is used
    /// to cap every arm's iteration count — the same pattern
    /// `mark_young_to_old_refs` already uses (`max_slots = body_bytes /
    /// SLOT_SIZE`). `HIB-DCAST-LATEPHASE.1`: this function used to re-read
    /// `header.array_length()`/`header.num_slots()` fresh at call time and
    /// trust them completely, with no cap at all. A caller may run this on an
    /// object well after `walk_objects` validated it — `close_live_set_over_old_gen`'s
    /// Phase 0 fixpoint, for one, calls this from a `for &(obj_ptr, _size) in
    /// objects` loop with `_size` unused — and a header field that reads
    /// differently on that later pass than it did during the walk (this file
    /// and its siblings document several distinct causes of exactly that) hits
    /// an UNBOUNDED `for slot_idx in 0..num_slots` stride into unmapped
    /// memory: the deterministic `SIGSEGV` inside this function's inlined
    /// legacy-object arm, reached via `close_live_set_over_old_gen`, against
    /// the real `DefaultCatalogAndSchemaTest` workload. Capping by the
    /// WALKED size — ground truth this function does not need to re-derive
    /// or guess at — closes that regardless of why the live re-read
    /// disagrees.
    ///
    /// The header is *not* borrowed across the `f` callback: the four scalar
    /// fields needed to drive the walk are copied out up front via
    /// `read_unaligned` on raw field pointers. This matters because a caller
    /// (e.g. the Phase 0 closure) may write to the *referent's* header from
    /// inside `f`, and for a self-referential object the referent is this very
    /// header — holding a live `&ObjectHeader` across that write would alias a
    /// `&mut` to the same bytes. Reading scalars up front keeps the borrow
    /// short and the walk sound under a self-loop.
    fn for_each_old_gen_ref(
        obj_ptr: *mut u8,
        total_size: usize,
        data: &[u8],
        mut f: impl FnMut(usize),
    ) {
        let data_start = data.as_ptr() as usize;
        let data_end = data_start + data.len();
        let body_bytes = total_size.saturating_sub(HEADER_SIZE);

        // Snapshot the layout-driving fields (incl. the compact oop-map Arc),
        // then drop the reference before any callback runs (see the aliasing
        // note above — `f` may mutate the object's header/fields).
        let (kind, element_type, array_length, num_slots, is_compact, compact) = unsafe {
            let h = &*(obj_ptr as *const ObjectHeader);
            (
                h.kind(),
                h.element_type(),
                h.array_length(),
                h.num_slots(),
                crate::is_compact_object(h),
                crate::heap::compact_oop_scan(h),
            )
        };

        if kind == ObjectKind::Array {
            if element_type == ArrayElementType::Reference {
                // Cap by the WALKED size, not the freshly-read `array_length`
                // — see this function's doc comment.
                let max_elems = body_bytes / ref_element_size();
                let elems = (array_length as usize).min(max_elems);
                for i in 0..elems {
                    let slot = unsafe { obj_ptr.add(ARRAY_DATA_OFFSET + i * ref_element_size()) };
                    let raw: u64 = unsafe { read_ref_slot(slot) };
                    if raw != 0 {
                        let ref_ptr = raw as usize;
                        if ref_ptr >= data_start && ref_ptr < data_end {
                            f(ref_ptr);
                        }
                    }
                }
            }
        } else if is_compact {
            // HIB-DCAST-LATEPHASE.1: `compact_oop_scan` returns `None` both
            // for "this is a legacy object" (its documented contract) and,
            // via its internal `class_layout_for_fields(..)?`, for "this IS
            // a compact object (`GC_FLAG_COMPACT` set) but its class's
            // layout is not registered right now". Gating on `is_compact`
            // (the header bit, independent of the registry) rather than
            // `compact.is_some()` keeps the second case from falling into
            // the legacy arm below, which would misread this object's
            // packed compact body under the legacy `num_slots * SLOT_SIZE`
            // formula and stride past its real extent — a `SIGSEGV` reached
            // this way against the real `DefaultCatalogAndSchemaTest`
            // workload. A compact object whose layout cannot be resolved has
            // no provably-safe reference slots to visit; skip it.
            if let Some((layout, body)) = compact {
                // Compact object: 8-byte reference slots at the oop-map offsets.
                for &off in &layout.ref_offsets {
                    let off = off as usize;
                    if off + ref_field_size() > body {
                        break;
                    }
                    let slot = unsafe { obj_ptr.add(HEADER_SIZE + off) };
                    let raw: u64 = unsafe { read_ref_slot(slot) };
                    if raw != 0 {
                        let ref_ptr = raw as usize;
                        if ref_ptr >= data_start && ref_ptr < data_end {
                            f(ref_ptr);
                        }
                    }
                }
            }
        } else {
            // Cap by the WALKED size, not the freshly-read `num_slots` — see
            // this function's doc comment.
            let max_slots = body_bytes / SLOT_SIZE;
            let slots = (num_slots as usize).min(max_slots);
            for slot_idx in 0..slots {
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
    /// so it can never drop a live object; the ordinary worst case is
    /// retaining a little floating garbage for one extra cycle.
    ///
    /// Cost: in the normal case (a transitively-correct marker) the first pass
    /// promotes nothing — every target of a live object is already live — so
    /// this is a single O(objects) walk and returns. Additional passes only
    /// occur when the marker under-marked (the exact failure this guards), and
    /// the fixpoint is bounded by the longest under-marked chain.
    ///
    /// # GCAUD-2 (2026-08-01) — the escape case, and why it must fail closed
    ///
    /// "`objects` is the complete set of old-gen objects" is an assumption,
    /// not a check, and the whole Phase 0 → Phase 1 → Phase 2 chain rests on
    /// it. A live object can hold an in-old-gen reference to an address that
    /// is **not** in `objects` for two reachable reasons:
    ///
    /// * the target's block is on the FREE list — `walk_objects` derives
    ///   allocated extents as the gaps between free blocks, so a freed block
    ///   is invisible to it. That is exactly the state the in-place sweep
    ///   leaves behind when the mark under-marks (defect 4 in
    ///   `fixed-suite-bugs/hibernate/map-resize-unpinned-chain-cursors-nojit-segv-20260731-FIXED.md`);
    /// * `scan_region` hit a header anomaly and `break`ed out of an allocated
    ///   region, dropping every object after it in that region.
    ///
    /// The pre-fix code responded to both by blindly `|=`-ing `GC_FLAG_MARKED`
    /// into `ref_ptr + cratonvm_types::GC_FLAGS_BYTE_OFFSET` — an unvalidated byte inside a free
    /// block or another object's payload — and then Phase 1, which iterates
    /// only `objects`, gave that address no forwarding pointer. Phase 2 leaves
    /// the referrer's slot on the pre-compaction address, and Phase 3 slides a
    /// different object onto it: a dangling pointer manufactured by the
    /// compactor itself, i.e. precisely the failure the whole of Phase 0
    /// exists to prevent.
    ///
    /// So an escaped referent is now (a) never written through, and (b)
    /// reported to the caller, which abandons the compaction. Over-retaining
    /// the entire old generation for one cycle is the fail-safe answer to "I
    /// cannot relocate this heap without leaving a dangling slot".
    ///
    /// Returns `(objects_promoted, escaped)`. `escaped` is `true` when the
    /// closure ran off `objects`; `objects_promoted` counts the under-marked
    /// objects this pass rescued and is the signal that the marker missed a
    /// push site (it is `0` on every healthy cycle).
    fn close_live_set_over_old_gen(objects: &[(*mut u8, usize)], data: &[u8]) -> (usize, bool) {
        // `walk_objects` yields ascending object starts, so a binary search
        // over the same slice is an exact membership test with no extra
        // allocation. Assert the ordering rather than assume it: this is the
        // predicate that decides whether a mark-bit write is legal.
        debug_assert!(
            objects.windows(2).all(|w| w[0].0 < w[1].0),
            "close_live_set_over_old_gen requires ascending object starts",
        );
        let is_walked_base = |addr: usize| {
            objects
                .binary_search_by_key(&addr, |&(p, _)| p as usize)
                .is_ok()
        };

        let mut escaped = false;
        let mut promoted_total = 0usize;
        // Fixpoint: keep re-scanning marked objects until a pass promotes
        // nothing. `objects` is finite and each pass can only flip flags
        // from 0→1, so this terminates in at most `objects.len()` passes.
        loop {
            let mut promoted_any = false;
            for &(obj_ptr, size) in objects {
                // Snapshot the marked bit; don't hold a header borrow while the
                // closure below may write the same header (self-loop case).
                let is_marked =
                    unsafe { (*(obj_ptr as *const ObjectHeader)).gc_flags() & GC_FLAG_MARKED != 0 };
                if !is_marked {
                    continue; // only trace *live* referrers
                }
                Self::for_each_old_gen_ref(obj_ptr, size, data, |ref_ptr| {
                    // GCAUD-2: `for_each_old_gen_ref` filters only on the
                    // backing store's [start, end) range — no alignment, no
                    // object-start validation. Writing a mark bit through an
                    // address the compactor cannot also FORWARD is what turns
                    // an under-marking bug into heap corruption, so refuse the
                    // write and record the escape instead.
                    if !is_walked_base(ref_ptr) {
                        escaped = true;
                        return;
                    }
                    // SAFETY: `ref_ptr` was just proven to be one of the object
                    // bases `walk_objects` yielded, so it is a valid old-gen
                    // object header. The pre-pass runs before any relocation,
                    // so the referent is still at its original address. No
                    // outstanding borrow of this header is live here (fields
                    // were copied out before the walk), so the `&mut` does not
                    // alias.
                    let ref_header = unsafe { &mut *(ref_ptr as *mut ObjectHeader) };
                    if ref_header.gc_flags() & GC_FLAG_MARKED == 0 {
                        ref_header.add_gc_flags(GC_FLAG_MARKED);
                        promoted_any = true;
                        promoted_total += 1;
                    }
                });
            }
            if !promoted_any {
                break;
            }
        }
        (promoted_total, escaped)
    }

    /// Close the marked set over "referenced by a live old-gen object" **in
    /// place**, without relocating anything.
    ///
    /// This is the same fixpoint [`Self::compact`]'s Phase 0 runs
    /// ([`Self::close_live_set_over_old_gen`]), exposed for the *other* old-gen
    /// reclaimer: the in-place sweep in
    /// `GenerationalHeap::old_gen_gc(compact = false)`, which is the collector
    /// that runs whenever a live JIT frame makes the root set conservative.
    ///
    /// # Why the in-place sweep needs it (GCAUD-8)
    ///
    /// That sweep decides "free this block" from `GC_FLAG_MARKED == 0` and
    /// nothing else, and its mark has eight push sites, each of which can fail
    /// *open* — dropping a genuine reference instead of over-retaining. When
    /// one does, the sweep hands a still-referenced block back to the free
    /// list. The compactor has been closed against exactly that under-marking
    /// since Phase 0 was written ("the compactor must not corrupt the heap when
    /// the marker under-marks"); the sweep was not, even though it is the arm
    /// that *creates* the freed-but-referenced block the compactor then refuses
    /// to relocate (`COMPACT_ESCAPE_HITS`).
    ///
    /// Running the closure before the free loop makes the two arms agree: an
    /// unmarked object that a marked object still points at is promoted to
    /// live and retained for one more cycle, transitively. Genuine garbage —
    /// unmarked and unreferenced — is untouched by the closure and is still
    /// freed, which is what the positive controls pin.
    ///
    /// `objects` must be the `walk_objects()` output for THIS old gen, taken
    /// under the same lock and with nothing allocated or freed since: the
    /// closure's admission proof is membership in that slice, so a stale slice
    /// would authorise a mark-bit write into a recycled address.
    ///
    /// Returns `(objects_promoted, escaped)`. `escaped` has the same meaning as
    /// in Phase 0 — a live referrer named an in-old-gen address the walk did
    /// not yield, i.e. a block that is *already* on the free list (or behind a
    /// `scan_region` anomaly). The sweep cannot un-free such a block, so it
    /// reports rather than aborts; see the call site.
    pub fn close_live_set(&self, objects: &[(*mut u8, usize)]) -> (usize, bool) {
        Self::close_live_set_over_old_gen(objects, &self.data)
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
                if ref_header.is_forwarded() {
                    if seedhunt_enabled() && (ref_header.forwarding_address() as usize) < 0x1000 {
                        eprintln!(
                            "[gcfwd] write small fwd: holder@0x{:x} cid={} \
                             referent@0x{:x} cid={} marked={} fwd=0x{:x}",
                            obj_ptr as usize,
                            header.class_id.as_u32(),
                            r,
                            ref_header.class_id.as_u32(),
                            ref_header.gc_flags() & GC_FLAG_MARKED != 0,
                            ref_header.forwarding_address() as usize,
                        );
                    }
                    Some(ref_header.forwarding_address())
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
    /// Total bytes on the free list, and the size of the LARGEST single
    /// free block. The pair is what distinguishes "the generation is full"
    /// from "the generation is mostly free but too fragmented to serve this
    /// request" — the failure mode that produced a spurious
    /// `OutOfMemoryError` before the free list learned to coalesce without
    /// compaction. Walks every bucket, so it is a diagnostic-path helper, not
    /// an allocator fast path.
    pub fn free_bytes_and_largest(&self) -> (usize, usize) {
        let mut total = 0usize;
        let mut largest = 0usize;
        for b in self.buckets.iter().flatten() {
            total += b.size;
            largest = largest.max(b.size);
        }
        (total, largest)
    }

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

    /// Lay out `count` fixed-size objects back to back in a fresh old gen
    /// and return their `(ptr, offset)` pairs. Headers must be real: the
    /// walker derives every object boundary by striding header-to-header.
    fn old_gen_with_objects(count: usize, slots: u32) -> (OldGen, Vec<usize>) {
        let mut og = OldGen::new(64 * 1024);
        let base = og.base_ptr() as usize;
        let mut offsets = Vec::new();
        for i in 0..count {
            let body = SLOT_SIZE * slots as usize;
            let ptr = og.alloc(HEADER_SIZE + body, 8).unwrap();
            // SAFETY: `ptr` is a fresh, correctly sized old-gen allocation.
            unsafe {
                std::ptr::write(
                    ptr as *mut ObjectHeader,
                    ObjectHeader::new(
                        cratonvm_types::ClassId::new(1),
                        ObjectKind::Object,
                        ArrayElementType::Reference,
                        0,
                        slots,
                    ),
                );
            }
            offsets.push(ptr as usize - base);
        }
        (og, offsets)
    }

    /// The reference semantics this walk had before it was given a monotone
    /// cursor: an object is collected iff its start offset lies in ANY dirty
    /// range. Kept here as the oracle the fast path is compared against.
    fn expected_in_ranges(offsets: &[usize], ranges: &[(usize, usize)]) -> Vec<usize> {
        offsets
            .iter()
            .copied()
            .filter(|off| ranges.iter().any(|&(s, e)| *off >= s && *off < e))
            .collect()
    }

    fn collected_offsets(og: &OldGen, ranges: &[(usize, usize)]) -> Vec<usize> {
        let base = og.base_ptr() as usize;
        og.walk_objects_in_card_ranges(ranges)
            .into_iter()
            .map(|(ptr, _)| ptr as usize - base)
            .collect()
    }

    #[test]
    fn card_range_walk_collects_exactly_the_objects_starting_in_a_dirty_range() {
        let (og, offsets) = old_gen_with_objects(8, 2);
        // One range covering objects 2..4 only.
        let ranges = vec![(offsets[2], offsets[4])];
        assert_eq!(
            collected_offsets(&og, &ranges),
            expected_in_ranges(&offsets, &ranges)
        );
        assert_eq!(
            collected_offsets(&og, &ranges),
            vec![offsets[2], offsets[3]]
        );
    }

    #[test]
    fn card_range_walk_handles_several_disjoint_ranges() {
        let (og, offsets) = old_gen_with_objects(10, 3);
        // Sorted, non-overlapping, with gaps — what the card table produces.
        let ranges = vec![
            (offsets[1], offsets[2]),
            (offsets[4], offsets[6]),
            (offsets[9], offsets[9] + 8),
        ];
        assert_eq!(
            collected_offsets(&og, &ranges),
            expected_in_ranges(&offsets, &ranges)
        );
    }

    #[test]
    fn card_range_walk_is_empty_when_no_object_starts_in_a_range() {
        let (og, offsets) = old_gen_with_objects(6, 2);
        // A range strictly inside object 3's body, so no object *starts* in it.
        let ranges = vec![(offsets[3] + 8, offsets[3] + 16)];
        assert!(collected_offsets(&og, &ranges).is_empty());
        assert!(expected_in_ranges(&offsets, &ranges).is_empty());
    }

    #[test]
    fn card_range_walk_matches_the_any_predicate_for_every_prefix_and_suffix() {
        // The monotone cursor added 2026-08-02 replaced a per-object
        // `ranges.iter().any(..)`, and returns early once the cursor passes
        // the last range. Sweep a family of range sets — including ones that
        // end before the last object, which is what exercises that early
        // return — and require the two to agree exactly.
        let (og, offsets) = old_gen_with_objects(12, 2);
        for lo in 0..offsets.len() {
            for hi in (lo + 1)..=offsets.len() {
                let end = if hi == offsets.len() {
                    offsets[hi - 1] + 8
                } else {
                    offsets[hi]
                };
                let ranges = vec![(offsets[lo], end)];
                assert_eq!(
                    collected_offsets(&og, &ranges),
                    expected_in_ranges(&offsets, &ranges),
                    "disagreement for objects {lo}..{hi}"
                );
            }
        }
    }

    #[test]
    fn card_range_walk_with_no_ranges_collects_nothing() {
        let (og, _offsets) = old_gen_with_objects(4, 2);
        assert!(collected_offsets(&og, &[]).is_empty());
    }

    /// gen-gc-five: a promotion buffer's unused tail goes back to the free
    /// list without stamping the reclaim epoch (it never held an object, so
    /// no remark snapshot can name it), and is servable again at once.
    #[test]
    fn releasing_an_unused_tail_keeps_the_reclaim_epoch() {
        let mut og = OldGen::new(64 * 1024);
        let epoch = og.reclaim_epoch();
        let p = og.alloc_unzeroed(4096, 8).unwrap();
        let used = og.used();
        // SAFETY: `[p + 1024, p + 4096)` is the untouched tail of the block just carved.
        unsafe { og.release_unused_tail(p.add(1024), 3072) };
        assert_eq!(og.reclaim_epoch(), epoch, "a never-used tail is not a reclaim");
        assert_eq!(og.used(), used - 3072);
        let again = og.alloc(3072, 8).expect("the tail is a servable free block");
        assert_eq!(again as usize, p as usize + 1024);
    }

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
        // 8-byte request occupies one production header after the 48-byte
        // aligned extent. Derived rather than restated: this number moved 80 ->
        // 72 with the 2026-08-06 header shrink, and a literal here would have
        // to be re-derived by hand every time the layout moves.
        assert_eq!(og.used(), 48 + cratonvm_types::HEADER_SIZE);
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

    /// xt-helper-window OOM regression: with compaction unavailable, freeing
    /// a run of adjacent blocks must not leave the generation unable to serve
    /// a request that the run's COMBINED bytes cover. Before the fix the size
    /// buckets held N isolated blocks of `S` bytes and an `S * 4` request
    /// returned `None` even though `N * S` contiguous bytes were free.
    ///
    /// The trailing free space is deliberately consumed first: without that,
    /// the request is served from the untouched tail and the test proves
    /// nothing.
    #[test]
    fn adjacent_frees_are_coalesced_so_a_larger_request_still_fits() {
        let mut og = OldGen::new(64 * 1024);
        const S: usize = 256;
        const N: usize = 16;

        let mut ptrs = Vec::new();
        for _ in 0..N {
            ptrs.push(og.alloc(S, 8).expect("fresh old gen must serve S bytes"));
        }
        let tail = og.capacity() - og.used();
        og.alloc(tail, 8)
            .expect("the trailing free block must be allocatable");
        assert_eq!(og.free_block_count(), 0, "no free space must remain");

        for p in ptrs.drain(..) {
            // SAFETY: each `p` came from `alloc(S, 8)` on this OldGen and is
            // freed exactly once, with its own size.
            unsafe { og.free(p, S) };
        }
        assert_eq!(og.free_block_count(), N, "N isolated blocks, all adjacent");

        let big = S * 4;
        assert!(
            og.alloc(big, 8).is_some(),
            "a {big}-byte request must be served out of {N} adjacent {S}-byte \
             free blocks — this is the old-gen half of the xt-helper-window \
             OutOfMemoryError (the free list never coalesces without compaction)"
        );
    }

    /// The merge itself: adjacent blocks collapse, and a second call reports
    /// `0` so `alloc`'s coalesce-and-retry cannot loop.
    #[test]
    fn coalesce_free_blocks_merges_adjacent_and_is_idempotent() {
        let mut og = OldGen::new(64 * 1024);
        const S: usize = 128;
        let mut ptrs = Vec::new();
        for _ in 0..8 {
            ptrs.push(og.alloc(S, 8).unwrap());
        }
        for p in ptrs.drain(..) {
            // SAFETY: see the test above.
            unsafe { og.free(p, S) };
        }
        let before = og.free_block_count();
        let merged = og.coalesce_free_blocks();
        assert!(merged > 0, "8 adjacent frees must merge (before={before})");
        assert!(
            og.free_block_count() < before,
            "the free-block count must drop ({before} -> {})",
            og.free_block_count()
        );
        assert_eq!(
            og.coalesce_free_blocks(),
            0,
            "a second merge must report no progress, so alloc's retry is one-shot"
        );
    }

    /// Two holes separated by a LIVE object must stay separate — merging them
    /// would hand out a span covering the live object.
    #[test]
    fn coalesce_free_blocks_never_merges_across_a_live_object() {
        let mut og = OldGen::new(64 * 1024);
        const S: usize = 128;
        let a = og.alloc(S, 8).unwrap();
        let live = og.alloc(S, 8).unwrap();
        let c = og.alloc(S, 8).unwrap();
        let tail = og.capacity() - og.used();
        og.alloc(tail, 8)
            .expect("the trailing free block must be allocatable");

        // SAFETY: `a` and `c` each came from `alloc(S, 8)` and are freed once;
        // `live` is deliberately left allocated between them.
        unsafe {
            og.free(a, S);
            og.free(c, S);
        }
        assert_eq!(og.free_block_count(), 2);

        og.coalesce_free_blocks();
        assert!(
            og.alloc(S * 2, 8).is_none(),
            "two {S}-byte holes separated by a LIVE object must not satisfy a \
             {}-byte request",
            S * 2
        );
        // The live object is untouched and its own hole is still usable.
        let _ = live;
        assert!(
            og.alloc(S, 8).is_some(),
            "each individual hole still serves S"
        );
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
            header.set_num_slots(2);
        }

        let obj_size2 = HEADER_SIZE + SLOT_SIZE; // 1 field
        let p2 = og.alloc(obj_size2, 8).unwrap();
        unsafe {
            let header = &mut *(p2 as *mut ObjectHeader);
            header.set_num_slots(1);
        }

        let objects = og.walk_objects();
        assert_eq!(objects.len(), 2);
        assert_eq!(objects[0].0, p1);
        assert_eq!(objects[0].1, obj_size1);
        assert_eq!(objects[1].0, p2);
        assert_eq!(objects[1].1, obj_size2);
    }

    /// `HIB-DCAST-LATEPHASE.1`: a walk that desyncs from real object
    /// boundaries lands on arbitrary bytes. Before this fix `scan_region`
    /// read those bytes as a typed `ObjectKind` unconditionally — instant UB
    /// for a tag outside `{0, 1, 2}`, which optimized code lowered to a
    /// `SIGILL` inside `OldGen::compact` (deterministic faulting RVA across
    /// repeated crashes against the real `DefaultCatalogAndSchemaTest`
    /// workload). This corrupts only the raw tag byte of an otherwise
    /// legitimately-allocated object, mirroring the regression style already
    /// used for the conservative-root validators (see
    /// `fixed-suite-bugs/source-debug-jit-conservative-root-invalid-header-tag-sigill.md`),
    /// and asserts the walk stops at the corrupted header instead of
    /// trusting it.
    #[test]
    fn walk_objects_stops_at_a_desynced_invalid_kind_tag_instead_of_trapping() {
        let mut og = OldGen::new(4096);

        let obj_size1 = HEADER_SIZE + 2 * SLOT_SIZE;
        let p1 = og.alloc(obj_size1, 8).unwrap();
        unsafe {
            let header = &mut *(p1 as *mut ObjectHeader);
            header.set_num_slots(2);
        }

        let obj_size2 = HEADER_SIZE + SLOT_SIZE;
        let p2 = og.alloc(obj_size2, 8).unwrap();
        unsafe {
            let header = &mut *(p2 as *mut ObjectHeader);
            header.set_num_slots(1);
        }

        // 0xFF is not a declared ObjectKind discriminant (0=Object, 1=Array,
        // 2=HumongousFiller).
        // SAFETY: `p2 + cratonvm_types::KIND_TAGS_BYTE_OFFSET` is the `kind` byte of a live
        // allocation from this OldGen; writing a raw `u8` there does not
        // require the resulting value to be a valid `ObjectKind`.
        unsafe {
            std::ptr::write(p2.add(cratonvm_types::KIND_TAGS_BYTE_OFFSET), 0xFFu8);
        }

        let before = WALK_DESYNC_HITS.load(std::sync::atomic::Ordering::Relaxed);
        let objects = og.walk_objects();
        let after = WALK_DESYNC_HITS.load(std::sync::atomic::Ordering::Relaxed);

        assert_eq!(
            objects,
            vec![(p1, obj_size1)],
            "the walk must stop at the corrupted header, not trust it"
        );
        assert!(
            after > before,
            "expected the desync guard to fire and bump WALK_DESYNC_HITS"
        );
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
            (*(filler as *mut ObjectHeader)).set_num_slots(4);
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
            a_hdr.set_num_slots(1);
            a_hdr.add_gc_flags(GC_FLAG_MARKED);
            let a_field0 = a.add(HEADER_SIZE) as *mut Value;
            std::ptr::write(a_field0, Value::Object(Some(ObjectRef::from_raw(b))));

            // B is left UNMARKED (would be floating garbage without the guard).
            let b_hdr = &mut *(b as *mut ObjectHeader);
            b_hdr.set_num_slots(1);
            b_hdr.mark_word.store(
                ObjectHeader::make_neutral_hashed(cratonvm_types::MARK_NEUTRAL, B_TAG),
                std::sync::atomic::Ordering::Relaxed,
            );
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
                ObjectHeader::neutral_hash(
                    b_hdr.mark_word.load(std::sync::atomic::Ordering::Relaxed)
                ),
                B_TAG,
                "B data lost across compaction"
            );
            // GC metadata cleared on the survivor.
            assert!(!b_hdr.is_forwarded());
            assert_eq!(b_hdr.gc_flags() & GC_FLAG_MARKED, 0);
        }
    }

    /// GCAUD-1 — `free` must return the SAME extent `alloc` reserved.
    ///
    /// `alloc_from_buckets` rounds the reservation up to a multiple of `align`
    /// and charges `used_bytes` that rounded amount; `free` mirrored only the
    /// `max(HEADER_SIZE, 8)` half of that expression. The bytes between the
    /// unrounded and rounded ends were therefore never returned to the free
    /// list: `used_bytes` drifted up, and — the part that matters —
    /// `walk_objects` derives allocated extents as the GAPS between free
    /// blocks, so the sliver read as an allocated non-object-start. The walk
    /// then mis-parses it as a header and `break`s out of the region, dropping
    /// every later object in it from the walk that drives compaction.
    #[test]
    fn free_returns_the_whole_extent_alloc_reserved() {
        let mut og = OldGen::new(4096);
        // 44 is not a multiple of 8: `alloc` reserves 48 (see
        // `alloc_alignment_reserves_compact_object_padding`).
        let a = og.alloc(44, 8).expect("fresh old gen serves 44 bytes");
        let b = og.alloc(64, 8).expect("fresh old gen serves 64 bytes");
        assert_eq!(
            b as usize - a as usize,
            48,
            "alloc must reserve the align-rounded extent",
        );
        assert_eq!(og.used(), 48 + 64);

        // SAFETY: `a` is a live block of this OldGen and 44 is the size it was
        // requested with — exactly the call shape the accounting must survive.
        unsafe { og.free(a, 44) };
        assert_eq!(
            og.used(),
            64,
            "freeing with the requested size must decrement the RESERVED size",
        );

        // The recovered hole must be the full 48 bytes, not 44: a 48-byte
        // request has to be servable from it, at the same address.
        let c = og
            .alloc(48, 8)
            .expect("the freed 48-byte hole must be reusable");
        assert_eq!(
            c, a,
            "a 48-byte request must land back in the freed block, not past the \
             live neighbour — a short free leaves an unusable 4-byte sliver",
        );
    }

    /// GCAUD-2 — the compactor must refuse to slide a heap it cannot fully
    /// forward.
    ///
    /// Phase 0 closes the live set over old gen so Phase 1 can stamp a
    /// forwarding address on every object a live referrer can reach. It
    /// assumed `walk_objects` yields EVERY old-gen object. It does not: a
    /// block that is on the free list is invisible to the walk (allocated
    /// extents are the gaps between free blocks), which is exactly the state
    /// an under-marking in-place sweep leaves behind. Phase 0 used to `|=`
    /// `GC_FLAG_MARKED` into that freed block anyway, Phase 1 gave it no
    /// forwarding address, and Phase 3 slid a different object onto it —
    /// the compactor manufacturing the dangling pointer it exists to prevent.
    ///
    /// The fail-safe answer is to reclaim nothing this cycle.
    #[test]
    fn compact_is_abandoned_when_a_live_ref_escapes_the_object_walk() {
        let mut og = OldGen::new(4096);
        const C_TAG: i32 = 0x0BAD_F00D_u32 as i32;

        // A: the live referrer.
        let a_size = HEADER_SIZE + SLOT_SIZE;
        let a = og.alloc(a_size, 8).unwrap();
        // B: the target, freed below so the object walk stops yielding it.
        let b_size = HEADER_SIZE + SLOT_SIZE;
        let b = og.alloc(b_size, 8).unwrap();
        // Dead filler, so a compaction that DID run would definitely move C.
        let filler_size = HEADER_SIZE + 4 * SLOT_SIZE;
        let filler = og.alloc(filler_size, 8).unwrap();
        // C: a second live object, the one whose non-movement proves the
        // compaction was abandoned rather than merely uneventful.
        let c_size = HEADER_SIZE + SLOT_SIZE;
        let c = og.alloc(c_size, 8).unwrap();

        // SAFETY: every pointer above is a live block of this OldGen, sized to
        // hold a header plus the slots written here.
        unsafe {
            (*(filler as *mut ObjectHeader)).set_num_slots(4);

            let a_hdr = &mut *(a as *mut ObjectHeader);
            a_hdr.set_num_slots(1);
            a_hdr.add_gc_flags(GC_FLAG_MARKED);
            std::ptr::write(
                a.add(HEADER_SIZE) as *mut Value,
                Value::Object(Some(ObjectRef::from_raw(b))),
            );

            (*(b as *mut ObjectHeader)).set_num_slots(1);

            let c_hdr = &mut *(c as *mut ObjectHeader);
            c_hdr.set_num_slots(1);
            c_hdr.add_gc_flags(GC_FLAG_MARKED);
            c_hdr.mark_word.store(
                ObjectHeader::make_neutral_hashed(cratonvm_types::MARK_NEUTRAL, C_TAG),
                std::sync::atomic::Ordering::Relaxed,
            );
        }

        // Return B's block to the free list WITHOUT zeroing it — the exact
        // shape the in-place old-gen sweep produces when the mark under-marks.
        // SAFETY: `(b, b_size)` is the pair `alloc` handed out.
        unsafe { og.free(b, b_size) };
        assert!(
            og.walk_objects().iter().all(|&(p, _)| p != b),
            "precondition: the freed block must be invisible to the object walk",
        );

        let used_before = og.used();
        let escapes_before = COMPACT_ESCAPE_HITS.load(std::sync::atomic::Ordering::Relaxed);
        let map = og.compact();

        assert!(
            map.is_empty(),
            "a compaction that cannot forward every live reference must relocate \
             nothing, so it can report no relocations",
        );
        assert!(
            COMPACT_ESCAPE_HITS.load(std::sync::atomic::Ordering::Relaxed) > escapes_before,
            "the abandoned compaction must be counted, not silent",
        );
        assert_eq!(
            og.used(),
            used_before,
            "an abandoned compaction must not rewrite the occupancy",
        );

        // SAFETY: nothing moved, so every pointer above still names its object.
        unsafe {
            // A's slot is untouched — still naming B, which is still where it
            // was. A rewrite here (or a slide over B) is the use-after-free.
            let Value::Object(Some(target)) = std::ptr::read(a.add(HEADER_SIZE) as *const Value)
            else {
                panic!("A's reference field was clobbered by an abandoned compaction");
            };
            assert_eq!(target.as_ptr(), b);

            // C did not slide over the filler, and its mark bit was cleared so
            // the next cycle starts from a clean slate.
            let c_hdr = &*(c as *const ObjectHeader);
            assert_eq!(
                ObjectHeader::neutral_hash(
                    c_hdr.mark_word.load(std::sync::atomic::Ordering::Relaxed)
                ),
                C_TAG,
                "C must not have moved"
            );
            assert_eq!(
                c_hdr.gc_flags() & GC_FLAG_MARKED,
                0,
                "marks must be cleared"
            );
            assert!(!c_hdr.is_forwarded());
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
            a_hdr.set_num_slots(1);
            a_hdr.add_gc_flags(GC_FLAG_MARKED);
        }

        // Object B: also stays in place (contiguous with A, nothing to
        // reclaim between them) — left UNWATCHED as a control.
        let b_size = HEADER_SIZE + SLOT_SIZE;
        let b = og.alloc(b_size, 8).unwrap();
        unsafe {
            let b_hdr = &mut *(b as *mut ObjectHeader);
            b_hdr.set_num_slots(1);
            b_hdr.add_gc_flags(GC_FLAG_MARKED);
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

    /// GCAUD-8 — [`OldGen::close_live_set`] is the in-place sweep's half of the
    /// guard the compactor has had since Phase 0 was written.
    ///
    /// Both halves must hold at once, so both are asserted here:
    ///
    /// * **the fix** — an UNMARKED object that a MARKED object still references
    ///   is promoted to live, so the sweep that runs next cannot free it under
    ///   a live pointer;
    /// * **the positive control** — an object that is unmarked *and*
    ///   unreferenced is left exactly as it was. A closure that promoted
    ///   everything would satisfy the first assertion and reclaim nothing ever
    ///   again, which is the failure mode this pairing exists to catch.
    #[test]
    fn close_live_set_promotes_a_referenced_target_and_leaves_real_garbage_dead() {
        let mut og = OldGen::new(4096);

        // A: live referrer.
        let a = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
        // B: A's target, deliberately left unmarked — the object a mark-phase
        // gap loses and the sweep would otherwise free.
        let b = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
        // C: unmarked AND unreferenced — genuine garbage, the control.
        let c = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();

        // SAFETY: all three are live blocks of this OldGen, each sized for a
        // header plus the single slot written below.
        unsafe {
            let a_hdr = &mut *(a as *mut ObjectHeader);
            a_hdr.set_num_slots(1);
            a_hdr.add_gc_flags(GC_FLAG_MARKED);
            std::ptr::write(
                a.add(HEADER_SIZE) as *mut Value,
                Value::Object(Some(ObjectRef::from_raw(b))),
            );
            (*(b as *mut ObjectHeader)).set_num_slots(1);
            (*(c as *mut ObjectHeader)).set_num_slots(1);
        }

        let objects = og.walk_objects();
        let (rescued, escaped) = og.close_live_set(&objects);

        assert_eq!(
            rescued, 1,
            "exactly the one referenced-but-unmarked object must be promoted",
        );
        assert!(
            !escaped,
            "every referent here is a walked base — nothing escaped the grid",
        );

        // SAFETY: nothing moved; both pointers still name their object.
        unsafe {
            assert_ne!(
                (*(b as *const ObjectHeader)).gc_flags() & GC_FLAG_MARKED,
                0,
                "an unmarked object a LIVE object references must be retained",
            );
            assert_eq!(
                (*(c as *const ObjectHeader)).gc_flags() & GC_FLAG_MARKED,
                0,
                "POSITIVE CONTROL: unreferenced garbage must stay dead, or the \
                 sweep this feeds would stop reclaiming anything at all",
            );
        }

        // Idempotent: a second run has nothing left to do.
        let (rescued_again, _) = og.close_live_set(&objects);
        assert_eq!(rescued_again, 0, "the closure must reach a fixpoint");
    }

    /// GCAUD-8 — the closure must also report the escape the in-place sweep
    /// cannot repair: a live object pointing at a block that is ALREADY on the
    /// free list. It must not write a mark bit through that address (it is
    /// unallocated memory the allocator may reissue at any moment) and it must
    /// not promote it into the walked set.
    #[test]
    fn close_live_set_reports_a_referent_that_is_already_freed() {
        let mut og = OldGen::new(4096);

        let a = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
        let b_size = HEADER_SIZE + SLOT_SIZE;
        let b = og.alloc(b_size, 8).unwrap();

        // SAFETY: both are live blocks of this OldGen.
        unsafe {
            let a_hdr = &mut *(a as *mut ObjectHeader);
            a_hdr.set_num_slots(1);
            a_hdr.add_gc_flags(GC_FLAG_MARKED);
            std::ptr::write(
                a.add(HEADER_SIZE) as *mut Value,
                Value::Object(Some(ObjectRef::from_raw(b))),
            );
            (*(b as *mut ObjectHeader)).set_num_slots(1);
        }

        // The state an under-marking sweep leaves behind: B freed, A still
        // naming it, B's bytes not zeroed.
        // SAFETY: `(b, b_size)` is exactly the pair `alloc` handed out.
        unsafe { og.free(b, b_size) };

        let objects = og.walk_objects();
        let (rescued, escaped) = og.close_live_set(&objects);

        assert!(
            escaped,
            "a referent outside the object walk must be reported"
        );
        assert_eq!(rescued, 0, "nothing in the walked set needed promoting");
        // SAFETY: `b`'s block is unallocated but still mapped inside `og`.
        unsafe {
            assert_eq!(
                (*(b as *const ObjectHeader)).gc_flags() & GC_FLAG_MARKED,
                0,
                "the closure must NOT write a mark bit into a free block",
            );
        }
    }
}
