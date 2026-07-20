// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Linear arena allocator for the semi-space copying GC.
//!
//! Each arena is a contiguous block of memory (`Vec<u8>`) with a cursor
//! that advances on each allocation. Objects cannot be individually freed;
//! instead, the entire arena is reset when GC swaps the semi-spaces.

/// A reclaimed (swept) region inside the arena, available for reuse by
/// the non-moving young-gen mark-sweep collector.
///
/// Free blocks are produced only by [`Arena::add_free_block`], which the
/// non-moving sweep calls for every dead object it reclaims. The bump
/// `cursor` continues to govern the high-water mark; free blocks let the
/// allocator satisfy requests from holes *below* the cursor without
/// relocating any survivor (which is what makes the collection
/// JIT-frame-safe — see `gen_heap::sweep_young_non_moving`).
#[derive(Debug, Clone, Copy)]
pub struct FreeBlock {
    /// Byte offset from the start of the backing buffer.
    pub offset: usize,
    /// Size of the free region in bytes.
    pub size: usize,
}

/// A linear (bump-pointer) arena allocator with an optional free list.
///
/// Allocates primarily via a monotonically advancing `cursor`. In
/// addition, the non-moving young-gen collector may hand reclaimed
/// regions back via [`Arena::add_free_block`]; subsequent allocations
/// prefer those holes (first sufficiently-large block) before bumping
/// the cursor. A full-arena [`Arena::reset`] clears both the cursor and
/// the free list.
/// Tier boundary for the segregated free lists: blocks smaller than this go
/// on `free_small`, the rest on `free_large`. Motivation (binarytrees-18
/// profile, 2026-07-06): a single first-fit free list mixes ~100k node-sized
/// holes with a handful of big coalesced spans, so TLAB-refill-sized requests
/// scanned the entire small-hole prefix before reaching a span (70% of wall
/// clock split between `Arena::alloc` and the young allocation probe). With
/// the split, object-sized requests first-fit the small list at ~index 0
/// (holes are node-sized) and chunk-sized requests consult ONLY the short
/// span list. 4 KiB sits far above any normal object (72 B nodes) and far
/// below the minimum TLAB refill (16 KiB+).
const LARGE_BLOCK_MIN: usize = 4096;

/// Per-allocation scan budget for the small-hole tier — see
/// [`Arena::first_fit`]. 16 keeps the uniform-hole case (hit at ~index 0)
/// untouched while capping the dust-prefix walk at a handful of compares;
/// a miss falls through to the bump tail / caller's old-gen spill.
const SMALL_TIER_SCAN_BUDGET: usize = 16;

pub struct Arena {
    /// Backing storage. Pre-allocated to `capacity` bytes.
    data: Vec<u8>,
    /// Next free byte offset within `data` (bump-allocation high-water mark).
    cursor: usize,
    /// Reclaimed regions below `cursor` SMALLER than [`LARGE_BLOCK_MIN`],
    /// produced by the non-moving sweep (typically object-sized holes).
    /// Empty unless a JIT-frame-safe mark-sweep has run.
    free_small: Vec<FreeBlock>,
    /// Reclaimed regions of at least [`LARGE_BLOCK_MIN`] bytes — the
    /// coalesced spans TLAB refills and array allocations carve from. Stays
    /// short (the post-sweep coalescer merges adjacent holes into a handful
    /// of spans), so scanning it is cheap.
    free_large: Vec<FreeBlock>,
    /// Conservative UPPER BOUND on the size of the largest free-list block
    /// (`actual_max <= max_free_upper` always). Maintained so hot callers can
    /// answer "no block of >= size exists" in O(1) instead of scanning the
    /// whole free list — the young-gen allocation probe runs once per JIT
    /// slow-path allocation and its former full `largest_free_block` scan was
    /// ~7% of a binarytrees-18 run (and 55% when probed with TLAB-refill
    /// sizes).
    ///
    /// Soundness of the bound:
    /// * [`Self::add_free_block`] raises it to at least the new block's size.
    /// * [`Self::alloc`] only shrinks/splits/removes blocks (the split
    ///   remainders it pushes are strictly smaller than the consumed block),
    ///   so the true max never grows there and the bound stays valid.
    /// * Failed full scans ([`Self::has_free_block_at_least`],
    ///   [`Self::largest_free_block_tightening`]) tighten it to the exact max
    ///   observed, so repeated "no" answers become O(1).
    /// * [`Self::clear_free_list`] / [`Self::reset`] / [`Self::reset_no_zero`]
    ///   zero it alongside the list.
    max_free_upper: usize,
    /// PERF (2026-07-15, round 2 of the RequestMappingMessageConversionIntegrationTests
    /// investigation): monotonic counter bumped on every free-list CONTENT
    /// change (a block added via `push_block_routed`, or removed/consumed by
    /// `alloc`'s free-list fast path; NOT bumped by the `max_free_upper`
    /// tightening on a failed scan, which doesn't touch list contents).
    /// Pairs with `free_bytes_cache` below to make `free_list_bytes()` O(1)
    /// on the (overwhelmingly common) case where nothing changed since the
    /// last call.
    free_list_epoch: u64,
    /// Cached `(epoch, bytes)` from the last `free_list_bytes()` computation.
    /// `Cell` (not a plain field) because the getter takes `&self`: `Arena`
    /// is always accessed through an external `Mutex` (see e.g.
    /// `GenerationalHeap::young_from`), so exclusive access is already
    /// guaranteed and a `Cell` cache is sound despite the shared-reference
    /// getter signature.
    free_bytes_cache: std::cell::Cell<(u64, usize)>,
}

/// Alignment tripwire (perf/halfgap residuals, 2026-07-18): every free-list
/// block must sit on the 8-aligned object grid — the non-moving sweep's walk
/// and the mark oracle both assume it. An unaligned block is upstream-bug
/// evidence (an alloc with an unrounded size minted an unaligned split
/// remainder, or a walk desync published an off-grid span); it later derails
/// the linear walk ("cursor overshot into free block" at +4 offsets). Warn
/// loudly (bounded) at the birth site so the producer is identifiable.
static FL_ALIGN_HITS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cold]
fn warn_unaligned_block(site: &str, offset: usize, size: usize) {
    use std::sync::atomic::Ordering;
    let n = FL_ALIGN_HITS.fetch_add(1, Ordering::Relaxed);
    if n < 16 {
        tracing::warn!(
            "free-list ALIGNMENT tripwire [{site}]: unaligned block off={offset}              (mod8={}) size={size} (mod8={}) — walk-grid hazard",
            offset & 7,
            size & 7,
        );
    }
}

impl Arena {
    /// Create a new arena with the given capacity in bytes.
    pub fn new(capacity: usize) -> Self {
        // Alignment invariant (perf/halfgap residuals, 2026-07-18): the
        // capacity must be a multiple of 8 — the heap-ergonomics young size
        // can arrive unaligned (observed live: 1 GiB - 4), and an unaligned
        // bump tail makes `remaining()` carry a permanent mod-8 dreg that
        // `refill_tlab`'s `requested.min(available)` then mints into
        // unaligned TLAB sizes and +4 free-list split remnants — the
        // trigger-ON bt18 walk-grid corruption. Rounding down loses at most
        // 7 bytes of arena.
        let capacity = capacity & !7;
        // We need the Vec to have length == capacity so we can
        // hand out pointers into it. We zero-initialize for safety.
        let data = vec![0u8; capacity];
        Self {
            data,
            cursor: 0,
            free_small: Vec::new(),
            free_large: Vec::new(),
            max_free_upper: 0,
            free_list_epoch: 0,
            free_bytes_cache: std::cell::Cell::new((0, 0)),
        }
    }

    /// Route a block to its size tier. Does NOT touch `max_free_upper` — the
    /// callers that can GROW the true maximum ([`Self::add_free_block`]) bump
    /// it themselves; split remainders are strictly smaller than the block
    /// they came from, so routing them leaves the bound valid.
    #[inline]
    fn push_block_routed(&mut self, block: FreeBlock) {
        if block.size == 0 {
            return;
        }
        if (block.offset | block.size) & 7 != 0 {
            warn_unaligned_block("route", block.offset, block.size);
        }
        if block.size < LARGE_BLOCK_MIN {
            self.free_small.push(block);
        } else {
            self.free_large.push(block);
        }
        self.free_list_epoch = self.free_list_epoch.wrapping_add(1);
    }

    /// First-fit scan of ONE tier, visiting at most `max_scan` blocks. On a
    /// fit: removes the block (O(1) `swap_remove`), returns the aligned
    /// allocation offset plus up to two remainder blocks (head alignment
    /// padding, tail leftover) for the caller to re-route by size. `None` =
    /// nothing within the scan budget fits.
    ///
    /// The budget exists for the SMALL tier: `swap_remove` back-fills with
    /// the most recently pushed block, so tiny split remainders ("dust")
    /// drift toward the scan prefix, and an unbounded first-fit paid an
    /// ever-growing dust walk on every object-sized allocation
    /// (binarytrees-18: `Arena::alloc` was 47% of wall). A bounded scan
    /// keeps the uniform-hole hit at ~index 0 while a dusty prefix gives up
    /// quickly — the caller falls through to the bump tail / old-gen spill,
    /// both valid homes for the object. Skipped blocks stay on the list, so
    /// the sweep walker's hole map (`free_blocks_sorted`) is unaffected.
    #[inline]
    fn first_fit(
        list: &mut Vec<FreeBlock>,
        base: usize,
        size: usize,
        align: usize,
        max_scan: usize,
    ) -> Option<(usize, [Option<FreeBlock>; 2])> {
        for i in 0..list.len().min(max_scan) {
            let block = list[i];
            let block_addr = base + block.offset;
            let aligned_addr = (block_addr + align - 1) & !(align - 1);
            let padding = aligned_addr - block_addr;
            // Overflow means this block can't satisfy the request; skip it
            // rather than aborting the whole `alloc` (the bump path below
            // may still succeed).
            let Some(total_needed) = padding.checked_add(size) else {
                continue;
            };
            if total_needed <= block.size {
                let alloc_offset = block.offset + padding;
                let remaining = block.size - padding - size;
                list.swap_remove(i);
                let head = (padding > 0).then_some(FreeBlock {
                    offset: block.offset,
                    size: padding,
                });
                let tail = (remaining > 0).then_some(FreeBlock {
                    offset: alloc_offset + size,
                    size: remaining,
                });
                return Some((alloc_offset, [head, tail]));
            }
        }
        None
    }

    /// Bump-allocate `size` bytes with the given alignment.
    ///
    /// Returns a pointer to the allocated region, or `None` if there
    /// isn't enough space.
    pub fn alloc(&mut self, size: usize, align: usize) -> Option<*mut u8> {
        debug_assert!(align.is_power_of_two(), "alignment must be a power of two");
        if size & 7 != 0 {
            // An unrounded size mints an unaligned split remainder / bump
            // cursor — the free-list alignment hazard above.
            warn_unaligned_block("alloc-size", self.cursor, size);
        }
        // Alignment invariant: consume whole 8-byte grid units so split
        // remainders and the bump cursor stay on the object grid no matter
        // what a caller passes. Every walker size formula already rounds the
        // same way, so this matches how the walk will stride the region.
        let size = size.checked_add(7)? & !7;

        // Reserve the allocation's aligned footprint, not merely an aligned
        // start. Compact object bodies can end 1..7 bytes before the next
        // object boundary; leaving that tail outside the allocation makes
        // linear heap walks see a phantom region between valid objects.
        let alloc_size = size.checked_add(align - 1).map(|v| v & !(align - 1))?;

        // Free-list fast path: if a prior non-moving sweep reclaimed any
        // holes, satisfy the request from the first block large enough to
        // hold `size` plus the alignment padding. The leftover (head
        // padding and/or tail) is returned to the free list so no space
        // is silently lost. This is checked first because the cursor may
        // already be at the arena's high-water mark after a sweep that
        // could not move survivors.
        // O(1) fail-fast: padding >= 0 means every block needs
        // `total_needed >= size`; if even the (upper bound of the) largest
        // block is smaller than `size`, no block can satisfy the request —
        // skip the scans entirely and go straight to the bump path.
        if (!self.free_small.is_empty() || !self.free_large.is_empty())
            && alloc_size <= self.max_free_upper
        {
            let base = self.data.as_ptr() as usize;
            // Tier selection: a request whose worst-case need (size + max
            // alignment padding) reaches LARGE_BLOCK_MIN can never be served
            // by a small block — skip the (potentially long) small list
            // entirely. Smaller requests try the small tier first: its
            // blocks are object-sized holes, so a same-shaped request
            // first-fits at ~index 0.
            let worst_need = alloc_size.saturating_add(align - 1);
            let hit = if worst_need < LARGE_BLOCK_MIN {
                Self::first_fit(
                    &mut self.free_small,
                    base,
                    alloc_size,
                    align,
                    SMALL_TIER_SCAN_BUDGET,
                )
                .or_else(|| Self::first_fit(&mut self.free_large, base, alloc_size, align, usize::MAX))
            } else {
                Self::first_fit(&mut self.free_large, base, alloc_size, align, usize::MAX)
            };
            if let Some((alloc_offset, remainders)) = hit {
                self.free_list_epoch = self.free_list_epoch.wrapping_add(1);
                for r in remainders.into_iter().flatten() {
                    self.push_block_routed(r);
                }
                // SAFETY: `alloc_offset + alloc_size` lies within the consumed
                // block, which came from a region inside the buffer.
                return Some(unsafe { self.data.as_mut_ptr().add(alloc_offset) });
            }
            // No fit. Tighten the upper bound only when the failure was a
            // FULL view of the relevant tiers (a bounded small-tier scan
            // may have skipped bigger blocks, so its miss proves nothing).
            if worst_need >= LARGE_BLOCK_MIN {
                // The whole span tier was scanned; the small tier caps below
                // LARGE_BLOCK_MIN <= worst_need by construction.
                let large_max = self.free_large.iter().map(|b| b.size).max().unwrap_or(0);
                let small_cap = if self.free_small.is_empty() {
                    0
                } else {
                    LARGE_BLOCK_MIN - 1
                };
                self.max_free_upper = self.max_free_upper.min(large_max.max(small_cap));
            }
        }

        // Bump-allocation path: align the cursor up (checked to prevent
        // overflow near usize::MAX).
        let aligned = self.cursor.checked_add(align - 1).map(|v| v & !(align - 1));
        let end = aligned.and_then(|a| a.checked_add(alloc_size));
        if let (Some(aligned), Some(end)) = (aligned, end) {
            if end <= self.data.len() {
                // SAFETY: `aligned` is within `[0, self.data.len())` because
                // `end <= self.data.len()` was just checked.
                let ptr = unsafe { self.data.as_mut_ptr().add(aligned) };
                self.cursor = end;
                return Some(ptr);
            }
        }

        // The bump tail `SMALL_TIER_SCAN_BUDGET`'s design assumed would
        // always be available as a fallback (see its doc comment: "a miss
        // falls through to the bump tail... both valid") is itself
        // exhausted. A bounded small-tier miss above proved nothing about
        // whether a fit exists further down the list — `swap_remove`
        // back-fills the scan prefix with freshly split "dust" remainders,
        // which can bury a genuinely-sized match past the scan budget
        // indefinitely once the arena stops growing (the non-moving young
        // collector never resets its cursor, so this is the steady state
        // for the rest of the process's life, not a transient blip). Before
        // declaring the allocation impossible, pay for one full, unbounded
        // scan of both tiers — this only costs anything in the
        // already-degenerate case where the bump tail is gone, which the
        // common case (tail available) never reaches.
        let base = self.data.as_ptr() as usize;
        let hit = Self::first_fit(&mut self.free_small, base, alloc_size, align, usize::MAX)
            .or_else(|| Self::first_fit(&mut self.free_large, base, alloc_size, align, usize::MAX));
        if let Some((alloc_offset, remainders)) = hit {
            self.free_list_epoch = self.free_list_epoch.wrapping_add(1);
            for r in remainders.into_iter().flatten() {
                self.push_block_routed(r);
            }
            // SAFETY: `alloc_offset + alloc_size` lies within the consumed
            // block, which came from a region inside the buffer.
            return Some(unsafe { self.data.as_mut_ptr().add(alloc_offset) });
        }
        None
    }

    /// Register a reclaimed `[offset, offset+size)` region as a free block.
    ///
    /// Called by the non-moving young-gen sweep for every dead object it
    /// reclaims. The region is **not** zeroed here — the sweep zeroes the
    /// reclaimed span itself so a later conservative root scan cannot
    /// observe a stale object header inside the hole.
    ///
    /// # Panics (debug only)
    /// Debug-asserts the block lies fully within the live (`< cursor`)
    /// region of the arena.
    pub fn add_free_block(&mut self, offset: usize, size: usize) {
        debug_assert!(
            offset + size <= self.cursor,
            "free block must lie within the bump region",
        );
        if size == 0 {
            return;
        }
        if (offset | size) & 7 != 0 {
            warn_unaligned_block("add_free_block", offset, size);
        }
        self.max_free_upper = self.max_free_upper.max(size);
        self.push_block_routed(FreeBlock { offset, size });
    }

    /// Drop every reclaimed region. Called when the arena is about to be
    /// swapped or reset so the next collection cycle starts clean.
    pub fn clear_free_list(&mut self) {
        self.free_small.clear();
        self.free_large.clear();
        self.max_free_upper = 0;
        self.free_list_epoch = self.free_list_epoch.wrapping_add(1);
    }

    /// Total bytes currently held on the free lists (reclaimed but unallocated).
    ///
    /// PERF (2026-07-15): this used to sum both tiers from scratch on EVERY
    /// call. Its only caller, `GenerationalHeap::needs_gc`, runs on every
    /// allocation attempt (not just ones that touch the free list), so a
    /// live workload with many allocations between free-list-changing events
    /// (sweeps, or an allocation actually consuming a free block) paid this
    /// O(free-list-size) cost far more often than the list's contents
    /// actually changed — and the list only grows over a session (the
    /// non-moving young sweep reclaims into it without ever fully draining
    /// it under steady churn). Measured contribution: `needs_gc` (which
    /// inlines this) was 24.5% of ALL sampled CPU time during a live
    /// `RequestMappingMessageConversionIntegrationTests` run — see
    /// docs/known-issues/CRATONVM-SPRING-GENUINE-BUGLIST.md. The
    /// epoch-gated cache below makes repeated calls between real changes
    /// O(1); the summation itself is unchanged (same tiers, same order),
    /// so a cache miss recomputes byte-identically to the old behavior.
    pub fn free_list_bytes(&self) -> usize {
        let (cached_epoch, cached_bytes) = self.free_bytes_cache.get();
        if cached_epoch == self.free_list_epoch {
            return cached_bytes;
        }
        let total = self.free_small.iter().map(|b| b.size).sum::<usize>()
            + self.free_large.iter().map(|b| b.size).sum::<usize>();
        self.free_bytes_cache.set((self.free_list_epoch, total));
        total
    }

    /// Size of the largest single free-list block (0 if the free list is
    /// empty). Used by the young-gen allocation probe to decide whether a
    /// request can be satisfied from reclaimed space when the bump cursor has
    /// reached capacity (a non-moving sweep cannot retreat the cursor, so a
    /// cursor-only probe would wrongly report OOM with the free list full).
    /// Unlike [`Self::free_list_bytes`], this reflects what a *single*
    /// allocation can actually use (the free list is non-coalescing across
    /// blocks within one request).
    pub fn largest_free_block(&self) -> usize {
        self.free_small
            .iter()
            .chain(self.free_large.iter())
            .map(|b| b.size)
            .max()
            .unwrap_or(0)
    }

    /// Early-exit probe: is there any single free block of at least `size`
    /// bytes?
    ///
    /// Semantically `largest_free_block() >= size`, but cheap on the hot
    /// allocation-probe path:
    /// * O(1) "no" when `size > max_free_upper` (the cached upper bound).
    /// * O(1) "yes" for any sub-[`LARGE_BLOCK_MIN`] request while the span
    ///   tier is non-empty (every span is bigger by definition).
    /// * `size >= LARGE_BLOCK_MIN` consults ONLY the short span tier — the
    ///   long small-hole tier can never satisfy it.
    /// * A full failed scan tightens `max_free_upper` to the exact maximum,
    ///   so subsequent same-or-larger probes become O(1).
    pub fn has_free_block_at_least(&mut self, size: usize) -> bool {
        if size == 0 {
            return !self.free_small.is_empty() || !self.free_large.is_empty();
        }
        if size > self.max_free_upper {
            return false;
        }
        if size < LARGE_BLOCK_MIN {
            if !self.free_large.is_empty() {
                return true; // every span is >= LARGE_BLOCK_MIN > size
            }
            let mut scan_max = 0usize;
            for b in &self.free_small {
                if b.size >= size {
                    return true;
                }
                scan_max = scan_max.max(b.size);
            }
            self.max_free_upper = scan_max;
            false
        } else {
            let mut scan_max = 0usize;
            for b in &self.free_large {
                if b.size >= size {
                    return true;
                }
                scan_max = scan_max.max(b.size);
            }
            // The small tier caps below LARGE_BLOCK_MIN; fold it into the
            // tightened bound conservatively rather than scanning it.
            self.max_free_upper = scan_max.max(if self.free_small.is_empty() {
                0
            } else {
                LARGE_BLOCK_MIN - 1
            });
            false
        }
    }

    /// Snapshot of the current free list as `(offset, size)` pairs,
    /// sorted by ascending offset. Used by the non-moving sweep's object
    /// walker to skip holes the same way `OldGen::walk_objects` does.
    pub fn free_blocks_sorted(&self) -> Vec<(usize, usize)> {
        let mut v: Vec<(usize, usize)> = self
            .free_small
            .iter()
            .chain(self.free_large.iter())
            .map(|b| (b.offset, b.size))
            .collect();
        v.sort_by_key(|&(off, _)| off);
        v
    }

    /// Reset the arena, logically freeing all allocations.
    /// The backing memory is zeroed for safety.
    ///
    /// # When is it safe to use [`Self::reset_no_zero`] instead?
    ///
    /// The unsafe `reset_no_zero` variant is only sound when **both** of the
    /// following hold:
    ///
    /// 1. The allocator path zero-initialises every byte before handing the
    ///    pointer to the caller (e.g. `try_alloc_young` calls
    ///    `ptr::write_bytes(ptr, 0, size)`), OR the immediate caller fully
    ///    overwrites the region (e.g. Cheney `copy_nonoverlapping` writing
    ///    `total_size` bytes into the to-space slot). This guarantees no
    ///    legitimate read ever observes a stale byte.
    /// 2. No external code can read bytes past the cursor between the reset
    ///    and the next allocation. In CratonVM this is **not** true for
    ///    young-gen arenas: `GenerationalHeap::is_object_address`
    ///    (gen_heap.rs:465) performs conservative pointer validation that
    ///    accepts any 8-byte-aligned address within `[base, base+capacity)`
    ///    of either young space and then dereferences it as an
    ///    `ObjectHeader`. The VM's conservative root scanner
    ///    (vm/src/vm/vm_exec.rs:679) feeds ambiguous JVM long values through
    ///    this check, so leftover bytes in a reset young arena can be
    ///    misidentified as live objects and forwarded as garbage.
    ///
    /// Therefore: do **not** swap `reset` for `reset_no_zero` on the young
    /// from-space hot path until `is_object_address` is bounded by the live
    /// cursor (or the conservative root scanner is replaced with a precise
    /// stack map). The audit-flagged "perf bug" is a real cost, but the
    /// correctness hazard outweighs it.
    pub fn reset(&mut self) {
        // Zero out used region for safety (prevents stale data reads)
        self.data[..self.cursor].fill(0);
        self.cursor = 0;
        self.free_small.clear();
        self.free_large.clear();
        self.max_free_upper = 0;
        self.free_list_epoch = self.free_list_epoch.wrapping_add(1);
    }

    /// Reset the arena without zeroing memory.
    ///
    /// # Safety
    /// Callers must ensure all subsequent allocations are fully
    /// initialized before any reads, AND that no external scanner can
    /// observe bytes past the (now-zero) cursor before they are
    /// re-allocated. See the doc comment on [`Self::reset`] for the
    /// full safety contract.
    ///
    /// Currently unused: the young-gen reset path in `gen_heap.rs` cannot
    /// satisfy condition (2) because conservative root scanning may
    /// dereference any 8-byte-aligned address in an arena's capacity
    /// range. Retained for future use when the GC switches to precise
    /// stack maps or when `is_object_address` is tightened to honour the
    /// cursor bound.
    #[allow(dead_code)]
    pub unsafe fn reset_no_zero(&mut self) {
        self.cursor = 0;
        self.free_small.clear();
        self.free_large.clear();
        self.max_free_upper = 0;
        self.free_list_epoch = self.free_list_epoch.wrapping_add(1);
    }

    /// Returns true if the given pointer falls within this arena's storage.
    pub fn contains(&self, ptr: *const u8) -> bool {
        let base = self.data.as_ptr();
        let end = unsafe { base.add(self.data.len()) };
        ptr >= base && ptr < end
    }

    /// The number of bytes currently allocated (cursor position).
    pub fn used(&self) -> usize {
        self.cursor
    }

    /// The total capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.data.len()
    }

    /// Get the base pointer of the arena's backing storage.
    pub fn base_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    /// Get a mutable base pointer of the arena's backing storage.
    pub fn base_ptr_mut(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }

    /// Grow the arena to at least `new_capacity` bytes, preserving existing data.
    ///
    /// If `new_capacity` <= current capacity, this is a no-op.
    ///
    /// # Panics
    ///
    /// `Vec::resize` may reallocate the backing buffer, which **invalidates
    /// every raw pointer** previously handed out by [`Self::alloc`]. Nothing
    /// in the type system enforces that callers fix up those pointers, so
    /// growing a non-empty arena is almost always a silent heap-corruption
    /// bug. To make the "safe by accident" usage explicit, this method
    /// **panics** if the arena has any live allocations (`cursor != 0`).
    ///
    /// Only an empty arena (cursor at 0, e.g. a freshly-reset to-space) may
    /// be grown. If a future caller genuinely needs to grow a populated
    /// arena it must first relocate every object and reset the cursor.
    ///
    /// Returns the old base pointer so callers can compute relocation offsets.
    pub fn grow(&mut self, new_capacity: usize) -> *const u8 {
        // Same alignment invariant as `new()` — see the constructor note.
        let new_capacity = new_capacity & !7;
        if new_capacity <= self.data.len() {
            return self.data.as_ptr();
        }
        assert_eq!(
            self.cursor, 0,
            "Arena::grow called on a non-empty arena ({} bytes live): \
             Vec::resize may reallocate and invalidate every pointer into \
             the arena. Only an empty (reset) arena may be grown.",
            self.cursor,
        );
        let old_base = self.data.as_ptr();
        let old_capacity = self.data.len();
        self.data.resize(new_capacity, 0);
        if std::env::var_os("CRATONVM_DBG_YOUNGSTATE").is_some() {
            eprintln!("[youngstate] arena-grow {old_capacity} -> {new_capacity} bytes");
        }
        old_base
    }

    /// The remaining free bytes in this arena.
    ///
    /// Counts both the un-bumped tail (`capacity - cursor`) and any holes
    /// reclaimed by a non-moving sweep. Note that free-list space is
    /// fragmented: a single allocation can only use one block, so this is
    /// an upper bound on the largest satisfiable request.
    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.cursor) + self.free_list_bytes()
    }
}

impl std::fmt::Debug for Arena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Arena")
            .field("used", &self.cursor)
            .field("capacity", &self.data.len())
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
    fn arena_basic_alloc() {
        let mut arena = Arena::new(1024);
        assert_eq!(arena.used(), 0);
        assert_eq!(arena.capacity(), 1024);

        let ptr = arena.alloc(64, 8).unwrap();
        assert!(!ptr.is_null());
        assert_eq!(arena.used(), 64);
    }

    /// dohead-oom (2026-07-19): a bounded small-tier scan miss must not be
    /// the final word once the bump tail is also exhausted. Simulate the
    /// non-moving young collector's steady state (cursor pinned at
    /// capacity, so every further allocation must come from the free list):
    /// register `SMALL_TIER_SCAN_BUDGET` too-small holes followed by one
    /// genuinely-sized hole past the scan budget, then request exactly that
    /// size. Without the post-bump fallback scan in `alloc`, this returns
    /// `None` despite a valid block existing.
    #[test]
    fn arena_alloc_finds_fit_past_scan_budget_when_bump_tail_exhausted() {
        let dust_count = SMALL_TIER_SCAN_BUDGET + 4;
        let dust_size = 8usize;
        let fit_size = 32usize;
        let capacity = dust_count * dust_size + fit_size + 256;
        let mut arena = Arena::new(capacity);

        // Carve out `dust_count` tiny live objects, then a fit-sized one.
        let mut dust_offsets = Vec::new();
        for _ in 0..dust_count {
            let ptr = arena.alloc(dust_size, 8).unwrap();
            dust_offsets.push(unsafe { ptr.offset_from(arena.base_ptr_mut()) } as usize);
        }
        let fit_ptr = arena.alloc(fit_size, 8).unwrap();
        let fit_offset = unsafe { fit_ptr.offset_from(arena.base_ptr_mut()) } as usize;

        // Reclaim them in the same order a real sweep would walk them: dust
        // first (fills the `swap_remove`-backed scan prefix), fit-sized hole
        // last (lands past the scan budget).
        for &off in &dust_offsets {
            arena.add_free_block(off, dust_size);
        }
        arena.add_free_block(fit_offset, fit_size);

        // Exhaust the bump tail so the free list is the only remaining path.
        let remaining = arena.capacity() - arena.used();
        arena.alloc(remaining, 8).unwrap();
        assert_eq!(arena.used(), arena.capacity());

        // A bounded scan alone would only see `dust_size`-sized holes here
        // and report failure; the fallback full scan must find the
        // fit-sized hole regardless of its position in the list.
        let request_size = 24;
        assert!(
            fit_size >= request_size,
            "test fixture invariant: the reclaimed hole must satisfy the request"
        );
        assert!(
            arena.alloc(request_size, 8).is_some(),
            "alloc must fall back to a full free-list scan once the bump tail \
             is exhausted, instead of trusting a bounded scan's miss"
        );
    }

    #[test]
    fn arena_multiple_allocs() {
        let mut arena = Arena::new(256);

        let p1 = arena.alloc(32, 8).unwrap();
        let p2 = arena.alloc(32, 8).unwrap();
        assert_ne!(p1, p2);
        assert_eq!(arena.used(), 64);
    }

    #[test]
    fn arena_alignment() {
        let mut arena = Arena::new(256);

        // Allocate 3 bytes with alignment 1 — the arena consumes whole
        // 8-byte grid units (alignment invariant), so used() advances to 8.
        arena.alloc(3, 1).unwrap();
        assert_eq!(arena.used(), 8);

        // Next allocation with alignment 8 lands on the grid at offset 8.
        let p2 = arena.alloc(8, 8).unwrap();
        let offset = unsafe { p2.offset_from(arena.base_ptr_mut()) } as usize;
        assert_eq!(offset, 8); // aligned to 8
        assert_eq!(arena.used(), 16);
    }

    #[test]
    fn arena_full_returns_none() {
        let mut arena = Arena::new(64);
        assert!(arena.alloc(64, 8).is_some());
        assert!(arena.alloc(1, 1).is_none());
    }

    #[test]
    fn arena_alignment_reserves_trailing_padding() {
        let mut arena = Arena::new(64);
        let first = arena.alloc(44, 8).unwrap();
        let second = arena.alloc(8, 8).unwrap();

        assert_eq!(unsafe { first.offset_from(arena.base_ptr_mut()) }, 0);
        assert_eq!(unsafe { second.offset_from(arena.base_ptr_mut()) }, 48);
        assert_eq!(arena.used(), 56);
    }

    #[test]
    fn arena_reset() {
        let mut arena = Arena::new(128);
        arena.alloc(64, 8).unwrap();
        assert_eq!(arena.used(), 64);

        arena.reset();
        assert_eq!(arena.used(), 0);

        // Can allocate again
        assert!(arena.alloc(128, 8).is_some());
    }

    #[test]
    fn arena_contains() {
        let mut arena = Arena::new(256);
        let ptr = arena.alloc(32, 8).unwrap();

        assert!(arena.contains(ptr));
        assert!(arena.contains(unsafe { ptr.add(16) }));
        // Just outside the arena
        assert!(!arena.contains(std::ptr::null()));
    }

    #[test]
    fn arena_zero_size_alloc() {
        let mut arena = Arena::new(64);
        let ptr = arena.alloc(0, 8).unwrap();
        assert!(!ptr.is_null());
        assert_eq!(arena.used(), 0);
    }

    #[test]
    fn arena_alignment_overflow_returns_none() {
        let mut arena = Arena::new(64);
        // Push cursor near usize::MAX so alignment arithmetic would overflow
        arena.cursor = usize::MAX - 2;
        // align=8 means cursor + 7 would overflow usize
        assert!(arena.alloc(1, 8).is_none());
    }

    #[test]
    fn arena_free_list_alloc_and_probe() {
        let mut arena = Arena::new(256);
        // Consume the whole bump region so only the free list can serve.
        assert!(arena.alloc(256, 8).is_some());
        assert!(arena.alloc(8, 8).is_none());

        // Reclaim two holes: 24B and 64B.
        arena.add_free_block(0, 24);
        arena.add_free_block(64, 64);
        assert_eq!(arena.largest_free_block(), 64);
        assert!(arena.has_free_block_at_least(24));
        assert!(arena.has_free_block_at_least(64));
        assert!(!arena.has_free_block_at_least(65));

        // Allocation from the 64B hole succeeds and leaves the remainder.
        assert!(arena.alloc(48, 8).is_some());
        assert_eq!(arena.largest_free_block(), 24);
    }

    #[test]
    fn arena_max_free_upper_bound_tightens_and_fails_fast() {
        let mut arena = Arena::new(256);
        assert!(arena.alloc(256, 8).is_some()); // exhaust bump tail
        arena.add_free_block(0, 32);
        arena.add_free_block(64, 16);

        // Upper bound reflects the largest add (32).
        assert!(!arena.has_free_block_at_least(33)); // full scan → tightens to 32
        assert!(arena.has_free_block_at_least(32));

        // Consume the 32B block entirely; the cached bound (32) is now an
        // over-estimate but stays a SOUND upper bound: a 20B probe scans,
        // finds only the 16B block, answers false, and tightens to 16.
        assert!(arena.alloc(32, 8).is_some());
        assert!(!arena.has_free_block_at_least(20));
        assert!(arena.has_free_block_at_least(16));

        // A fresh add raises the bound again.
        arena.add_free_block(96, 40);
        assert!(arena.has_free_block_at_least(40));

        // clear/reset zero the bound → O(1) false.
        arena.clear_free_list();
        assert!(!arena.has_free_block_at_least(1));
        assert!(arena.has_free_block_at_least(0) == false);
    }

    #[test]
    fn arena_tiered_free_lists_route_and_serve_by_size() {
        let mut arena = Arena::new(64 * 1024);
        let cap = arena.capacity();
        assert!(arena.alloc(cap, 8).is_some()); // exhaust bump tail

        // A big coalesced span + a pile of node-sized holes.
        arena.add_free_block(0, 32 * 1024); // → free_large
        for i in 0..100 {
            arena.add_free_block(40 * 1024 + i * 72, 72); // → free_small
        }

        // Chunk-sized request is served from the span tier (the small holes
        // can never satisfy it and must not be scanned — behaviorally: it
        // just succeeds).
        assert!(arena.alloc(16 * 1024, 8).is_some());
        // Object-sized requests are served from the hole tier.
        assert!(arena.alloc(72, 8).is_some());
        assert!(arena.alloc(72, 8).is_some());

        // Probes: sub-tier-boundary probe answers O(1)-yes while a span
        // remains; chunk-sized probe consults spans only.
        assert!(arena.has_free_block_at_least(72));
        assert!(arena.has_free_block_at_least(8 * 1024));
        assert!(!arena.has_free_block_at_least(33 * 1024));

        // Consume the span remainder; chunk probes now answer false while
        // hole-sized probes still answer true.
        assert!(arena.alloc(16 * 1024 - 16, 8).is_some());
        assert!(!arena.has_free_block_at_least(LARGE_BLOCK_MIN));
        assert!(arena.has_free_block_at_least(72));

        // Splitting a span routes a sub-tier remainder to the small list:
        // reclaim a 5 KiB block, carve 4.5 KiB → ~0.5 KiB remainder must
        // still be findable by a small request.
        let mut a2 = Arena::new(8 * 1024);
        let c2 = a2.capacity();
        assert!(a2.alloc(c2, 8).is_some());
        a2.add_free_block(0, 5 * 1024);
        assert!(a2.alloc(4 * 1024 + 512, 8).is_some());
        assert!(a2.has_free_block_at_least(256));
        assert!(a2.alloc(256, 8).is_some());
    }

    #[test]
    fn arena_alloc_fail_fast_skips_scan_for_oversized_requests() {
        let mut arena = Arena::new(128);
        assert!(arena.alloc(128, 8).is_some()); // exhaust bump tail
        arena.add_free_block(0, 24);
        // Larger than any block AND no bump room → None (via the O(1)
        // fail-fast; behaviorally identical to the historical full scan).
        assert!(arena.alloc(64, 8).is_none());
        // Fits the hole → allocates from the free list.
        assert!(arena.alloc(16, 8).is_some());
    }
}
