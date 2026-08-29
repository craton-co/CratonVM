// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Generational ZGC remembered set — per-page bitmaps over reference-field
//! slots, maintained by the store barrier.
//!
//! # Why this module exists
//!
//! [`crate::zgc::ZgcRealHeap`] has no generational split: every collection is
//! a whole-heap stop-the-world mark-sweep over one arena. Allocation-churn
//! workloads (Spring's `*AutoConfigurationTests`, which build and tear down an
//! `ApplicationContext` per test method) therefore pay a full-heap trace for
//! every short-lived object they drop, which is the shape that turned 35
//! passing classes into 300 s timeouts under `-XX:+UseZGC`
//! (`fixed-suite-bugs/springboot/zgc-real-fullsuite-regression-RETIRED-20260807.md`).
//!
//! The fix is generational collection, and a **remembered set is its hard
//! prerequisite**: a young-only collection must find every old→young reference
//! or it will free objects that are still reachable from the old generation.
//! This module is that mechanism. It is deliberately mechanism-only: it holds
//! no policy, schedules no collections, and does not touch
//! [`GenerationalZgc`](crate::zgc::GenerationalZgc)'s JEP-439-shaped
//! minor/major/promotion-age policy layer.
//!
//! # Design: per-page bitmaps, not a card table
//!
//! OpenJDK's Generational ZGC does not use a classic card table. Each *old*
//! page carries a pair of bitmaps with one bit per potential reference-field
//! position in the page; the store barrier sets the bit for the exact field it
//! wrote. The two bitmaps are swapped at cycle start so the collector scans a
//! stable snapshot while mutators keep dirtying the other.
//!
//! ## Granularity and the resulting memory overhead
//!
//! The grain is [`Z_REMSET_GRAIN_BYTES`] = 8 bytes, which is exactly
//! `cratonvm_types::REF_FIELD_SIZE` / `REF_ELEMENT_SIZE` — the width of a
//! reference field and of a reference array element in every CratonVM layout.
//!
//! **Re-derived 2026-08-07 against the current constants.** This module was
//! first written when [`HEADER_SIZE`](cratonvm_types::HEADER_SIZE) was believed
//! to be 24. It is **16** (`types/src/heap_types.rs:19`) — the header was shrunk
//! 32 → 24 → 16, the last step landing 2026-08-07 alongside the removal of
//! `identity_hash_code` from `ObjectHeader`. The measured values today are:
//!
//! | constant | value | anchor |
//! |---|---|---|
//! | `HEADER_SIZE` | **16** | `types/src/heap_types.rs:19` |
//! | `SLOT_SIZE` (legacy tagged cell) | 16 | `types/src/heap_types.rs:171` |
//! | `REF_FIELD_SIZE` (compact ref field) | 8 | `types/src/heap_types.rs:185` |
//! | `REF_ELEMENT_SIZE` (ref array element) | 8 | `types/src/heap_types.rs:176` |
//! | `ARRAY_DATA_OFFSET` | `HEADER_SIZE` = **16** | `types/src/heap_types.rs:300` |
//!
//! **The conclusion survives the shrink, and it survives it for the same
//! reason**: 16 is a multiple of 8, exactly as 24 and 32 were. Every reference
//! word in the heap is therefore still 8-byte aligned, and one bit still names
//! exactly one reference slot. Walking all four slot shapes
//! (`docs/feature-designs/zgc-reference-slot-representation.md` §1.3-§1.6):
//!
//! ```text
//! compact ref field   obj + 16 + field_offsets[i]      8-aligned: 16 is, and the
//!                                                      layout builder aligns every
//!                                                      field to its own width
//!                                                      (classloading/src/class.rs:1571)
//! legacy ref field    obj + 16 + i*16 + 8              8-aligned: 16 + 16i + 8
//! ref array element   obj + 16 + i*8                   8-aligned
//! static ref field    statics + i*16 + 8               8-aligned
//! ```
//!
//! Note the legacy shape: the reference *word* is at `cell + 8`, not at the
//! cell base — the low 8 bytes are the `Value` tag/pay32 half and hold no
//! pointer. A legacy reference cell therefore occupies **two** bits of this
//! bitmap, of which only the second one is ever a reference slot. That is not
//! imprecision (the barrier sets the bit for the word it actually wrote); it
//! only means a bit-count over an all-legacy page tops out at half the bits.
//!
//! So: **there is no spatial imprecision at all**: one bit ⇔ one reference
//! slot. The claim was true at `HEADER_SIZE = 24` and it is true at 16; what
//! would break it is a header or slot size that is *not* a multiple of 8, and
//! the const-assertions below are what would catch that.
//!
//! ```text
//! bits per page      = page_size / 8
//! bytes per bitmap   = page_size / 8 / 8   = page_size / 64   = 1.5625 %
//! bytes per PAGE     = 2 x that            = page_size / 32   = 3.1250 %
//!                      (double-buffered: current + previous)
//!
//! ZGC small page, 2 MiB:  32 KiB per bitmap, 64 KiB for the pair
//! 2 GiB of old generation:                  64 MiB of bitmaps
//! ```
//!
//! versus [`crate::card_table::CardTable`], which is one `AtomicU8` per
//! `CARD_SIZE` = 512 bytes:
//!
//! ```text
//! bytes per page     = page_size / 512      = 0.1953 %
//! ZGC small page, 2 MiB:                      4 KiB
//! 2 GiB of old generation:                    4 MiB
//! ```
//!
//! So the bitmap pair costs **16x** the card table's byte-map. That is the
//! honest headline number and it is not small.
//!
//! ## Scan cost: identical, by arithmetic
//!
//! One `u64` word of an 8-byte-grain bitmap covers `64 * 8` =
//! [`Z_REMSET_BYTES_PER_WORD`] = **512 bytes of page — exactly one card**.
//! [`ZRememberedSet::iterate`] loads one word, tests it against zero, and skips
//! it whole when clean (`u64::trailing_zeros` walks only the set bits of a
//! non-zero word, never bit-by-bit). A sparse old page therefore costs the
//! collector *the same number of memory touches* as a card-table byte scan over
//! the same page. This is why the zero-word skip is load-bearing rather than a
//! micro-optimisation: without it the scan is 64 loads per card's worth of
//! page, and a minor GC degenerates back into the whole-heap cost this work
//! exists to escape.
//!
//! ## False positives
//!
//! * **Bitmap:** zero *spatial* false positives — a set bit names one 8-byte
//!   slot and the collector reads that slot directly, with no object-header
//!   parsing. The only imprecision is *temporal*: a bit stays set until the
//!   next [`ZRememberedSet::swap`] even if the mutator subsequently overwrote
//!   the field with `null` or an old-generation reference.
//! * **Card table:** a dirty card forces a re-walk of every object starting in
//!   its 512 bytes and a re-read of *every* reference slot of those objects. In
//!   `gen_heap` the barrier marks the card of the object's *base*
//!   (`gen_heap.rs::write_barrier` → `thread_local_dirty_addr(src_addr)`), and
//!   `scan_dirty_cards_inner` then scans the whole object, so a single edge
//!   into a 60-field object costs 60 slot reads plus a header decode.
//!
//! ## Why ZGC chose bitmaps
//!
//! Three reasons, none of which is "bitmaps are smaller":
//!
//! 1. **The remembered set must be per-page, not per-heap.** ZGC relocates and
//!    frees whole pages concurrently. A per-page bitmap is discarded with its
//!    page in O(1); a flat card table indexed by absolute address has no such
//!    unit and must be scrubbed by range.
//! 2. **The heap is a sparse multi-mapped virtual address space.** ZGC pages
//!    are handed out of a reserved-but-not-contiguous range, so there is no
//!    single `(base_addr, region_size)` pair to index a flat table by — the
//!    thing `CardTable` is built around. Any card table over that address space
//!    needs a two-level page table, at which point it *is* a per-page structure.
//! 3. **Exact offsets, because old pages have no object-start table.** The
//!    young collector wants to go straight to the slot. Card-granularity would
//!    force it to parse object headers backwards from a card boundary, which
//!    ZGC's old pages do not support.
//!
//! ## Recommendation for CratonVM (read this before wiring it up)
//!
//! Every one of those three reasons is a property `ZgcRealHeap` **does not yet
//! have**. It is one contiguous [`Arena`](crate::arena::Arena), non-moving, and
//! stop-the-world. For a *first* generational landing — the one that has to fix
//! the 35 hanging classes — reusing [`crate::card_table::CardTable`] over an
//! old-generation sub-range of that arena is the better engineering call:
//! it is 16x cheaper in memory, it already has a JIT inline post-write barrier
//! (`jit_cards_addr`: a plain release byte store, against the `lock or` an
//! atomic bitmap needs on every reference store), and it already carries the
//! cross-thread completeness fixes (V6 table-id scoping, dying-thread orphan
//! retention) that this module would have to re-earn.
//!
//! This module is the right shape for the ZGC that is being built — once pages
//! are real, relocation is concurrent, and the old generation stops being one
//! contiguous range, points 1–3 above become load-bearing and the card table's
//! single-region assumption breaks. Both representations expose the same
//! consumption shape (iterate offsets, take a snapshot, clear), so the swap is
//! a contained change. See the agent report accompanying this file.
//!
//! ### The suite went the other way (recorded 2026-08-07, question still open)
//!
//! The recommendation above is **left standing deliberately** — it was a
//! considered call and its three premises have not been refuted. But it is no
//! longer what the tree does, and a reader deserves both halves:
//!
//! * `gc/tests/zgc_module_integration.rs` wires *this* module into a real
//!   [`ZGenerationalHeap`](crate::zgc::generation::ZGenerationalHeap) over real
//!   `page.rs` pages (seam 4, `remembered ↔ generation`), and
//!   `gc/src/zgc/adapters.rs` ships the two production bridges —
//!   `ZHeapGenerationContext` and `ZTableRememberedSetView` — for the same
//!   pairing. No equivalent `CardTable`-over-ZGC path was built.
//! * That also moved premise 2 out from under the recommendation for the *page*
//!   heap: `ZPageAllocator` hands young and old pages out of **one interleaved
//!   granule pool**, and `ZGenerationalHeap`'s young/old split is a byte
//!   *budget*, not an address partition (`adapters.rs`'s "why `remembered`'s own
//!   context cannot do this"). A flat card table keyed by
//!   `(base_addr, region_size)` cannot describe that heap either. The
//!   recommendation's premise — "it is one contiguous `Arena`" — still holds for
//!   `ZgcRealHeap`, which is a *different* heap from the one the suite wires.
//!
//! So the open question is not "bitmap or card table" in the abstract; it is
//! **which heap the first generational landing targets**. Against `ZgcRealHeap`
//! (one arena, non-moving, STW) the recommendation above is very likely still
//! right. Against `ZGenerationalHeap` (real pages, interleaved) it does not
//! apply. Nobody has written down which one ships first, and that — not this
//! module's representation — is the decision to make.
//!
//! # Concurrency contract
//!
//! * [`ZRememberedSet::remember`] is the mutator fast path: lock-free, one
//!   atomic `fetch_or`, safe from any number of threads.
//! * [`ZRememberedSet::swap`] and [`ZRememberedSetTable::swap_all`] are
//!   **stop-the-world only** — see the invariant on [`ZRememberedSet::swap`].
//! * [`ZRememberedSetTable`] hands back cloned `Arc`s and drops its lock guard
//!   before the caller does anything with them; never hold the registry guard
//!   across page work.

use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use rustc_hash::{FxHashMap, FxHashSet};

// ---------------------------------------------------------------------------
// Granularity
// ---------------------------------------------------------------------------

/// `log2` of the remembered-set grain: one bit per `1 << shift` bytes of page.
///
/// Fixed at 3 (8 bytes) because that is the width of a reference field and of
/// a reference array element. Raising it trades exactness for memory at a
/// known rate: shift 9 (512 bytes) makes this structure a *bit-map card table*
/// costing `page_size / 4096` per bitmap — one eighth of
/// [`crate::card_table::CardTable`]'s byte-map — at the cost of the exact
/// field offsets that let the young collector skip header parsing. The
/// tradeoff is discussed in the module header; the constant is deliberately a
/// single place so an A/B is a one-line change.
pub const Z_REMSET_GRAIN_SHIFT: u32 = 3;

/// Bytes of page covered by one remembered-set bit. See
/// [`Z_REMSET_GRAIN_SHIFT`].
pub const Z_REMSET_GRAIN_BYTES: usize = 1usize << Z_REMSET_GRAIN_SHIFT;

// The grain must equal the reference-slot width, otherwise a set bit no longer
// names exactly one reference field and the "zero spatial false positives"
// claim in the module header is false.
const _: () = assert!(Z_REMSET_GRAIN_BYTES == cratonvm_types::REF_FIELD_SIZE);
const _: () = assert!(Z_REMSET_GRAIN_BYTES == cratonvm_types::REF_ELEMENT_SIZE);

// ... and every reference slot must LAND on that grain. The module header's
// one-bit-per-slot conclusion depends on the object header, the legacy tagged
// cell and the array data offset all being multiples of the grain — nothing
// else about their values matters. `HEADER_SIZE` has already been 32, then 24,
// then 16 (2026-08-07) without breaking this; these assertions are what turn
// the next change into a compile error rather than a prose drift.
const _: () = assert!(cratonvm_types::HEADER_SIZE % Z_REMSET_GRAIN_BYTES == 0);
const _: () = assert!(cratonvm_types::SLOT_SIZE % Z_REMSET_GRAIN_BYTES == 0);
const _: () = assert!(cratonvm_types::ARRAY_DATA_OFFSET % Z_REMSET_GRAIN_BYTES == 0);
// The legacy reference WORD sits at `cell + FIELD_CELL_PAYLOAD64_OFFSET`, not
// at the cell base, so that displacement must be on the grid too.
const _: () = assert!(cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET % Z_REMSET_GRAIN_BYTES == 0);

/// Bits in one bitmap word.
pub const Z_REMSET_BITS_PER_WORD: usize = 64;

/// Bytes of page covered by one bitmap word: `8 * 64` = 512.
///
/// Deliberately equal to [`crate::card_table::CARD_SIZE`]. That equality is
/// what makes a zero-word skip cost the collector exactly what a clean-card
/// byte test costs it — see the module header's scan-cost note.
pub const Z_REMSET_BYTES_PER_WORD: usize = Z_REMSET_GRAIN_BYTES * Z_REMSET_BITS_PER_WORD;

// ---------------------------------------------------------------------------
// ZRememberedSet
// ---------------------------------------------------------------------------

/// Point-in-time counters for one [`ZRememberedSet`]. Diagnostic only; nothing
/// in the collector branches on these.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ZRememberedSetStats {
    /// Identity of the old page this set belongs to.
    pub page_id: u64,
    /// Size of that page in bytes.
    pub page_size: usize,
    /// Bytes covered by one bit ([`Z_REMSET_GRAIN_BYTES`]).
    pub grain_bytes: usize,
    /// Words in one of the two bitmaps.
    pub word_count: usize,
    /// Bits currently set in the buffer mutators are dirtying.
    pub bits_set: usize,
    /// Bits currently set in the snapshot the collector scans.
    pub bits_set_snapshot: usize,
    /// Lifetime count of words examined by [`ZRememberedSet::iterate`] and
    /// friends.
    pub words_scanned: u64,
    /// Lifetime count of those words that were zero and were skipped whole.
    /// `words_skipped / words_scanned` is the sparsity of this page; a ratio
    /// near 1.0 is what makes a minor GC cheap.
    pub words_skipped: u64,
    /// Lifetime count of [`ZRememberedSet::remember`] calls that set a bit
    /// which was previously clear.
    pub bits_newly_set: u64,
    /// Total bytes of bitmap storage retained (both buffers plus the struct).
    pub retained_bytes: usize,
}

/// The remembered set of a single **old** page: a double-buffered bitmap over
/// the page's reference-slot positions.
///
/// Young pages get no remembered set — a young→anything store creates no edge
/// a young collection could miss, because the young collection traces the
/// whole young generation anyway.
///
/// # Double buffering
///
/// Two bitmaps are held. `current` is the one mutators dirty; the other is the
/// *snapshot* the collector scans. [`Self::swap`] flips the roles. The
/// invariant is stated on that method.
pub struct ZRememberedSet {
    /// Identity of the old page. Plain data on purpose: this module must not
    /// import the ZGC page types (they are being written in parallel), so the
    /// coupling to them is a `u64` and a `usize` and nothing more.
    ///
    /// # Widened `u32` → `u64`, 2026-08-07
    ///
    /// **Verified against `page.rs` before landing**: `ZPageReal::id` is `u64`
    /// (`page.rs:441`), `ZPageReal::id()` returns `u64` (`page.rs:481`), and
    /// `ZPageAllocatorState::next_page_id` is a `u64` initialised to 1
    /// (`page.rs:973`, `:1156`) that is only ever incremented. The claim
    /// checked out; the text below is the reasoning, not a guess.
    ///
    /// This was a `u32` while the page modules were in flight, on the guess
    /// that a page id would be a small dense index. It is not.
    /// [`crate::zgc::page::ZPageReal::id`] is a `u64` handed out of
    /// `ZPageAllocator`'s `next_page_id` counter, which starts at 1, is
    /// incremented on every page allocation and is **never recycled downward**
    /// when a page is freed (`page.rs`'s `free_page` returns the memory to the
    /// granule pool and drops the id). `forwarding::PageCandidate::page_id`,
    /// `generation::ZScopeEntry::page_id` and
    /// `generation::ZGenerationAllocation::page_id` are all `u64` for the same
    /// reason.
    ///
    /// So the narrowing was not a representation choice, it was an aliasing
    /// bug on a timer: past `u32::MAX` page allocations every remembered set
    /// would land on `id % 2^32`, i.e. on **a wrong page's bitmap**. The
    /// consequence is the worst one this module has — the young collection
    /// reads its roots out of the wrong page, misses genuine old→young edges,
    /// and frees live objects, with no error anywhere. There is no observable
    /// symptom until the use-after-free, and nothing about it is repeatable.
    ///
    /// **The width was the smaller half of the problem** (added 2026-08-07,
    /// audit finding N7). This note fixed how *wide* the id is and said nothing
    /// about *which numbering* it is in — and the module's own shipped
    /// [`ZGenerationContext`] answered in a different one. The failure mode is
    /// the same one described above, minus the `u32::MAX` timer. See
    /// [`ZPageIdNamespace`].
    ///
    /// The widening is module-wide: [`ZRememberedSetTable`]'s map key, its
    /// coarse set, [`ZGenerationContext::page_of`]'s return and
    /// [`ZStoreBarrier::record`]'s parameter are all `u64` for the same reason,
    /// and there is no longer any narrowing check anywhere in this file. A
    /// narrowing check would only have converted the aliasing into a coarsened
    /// page (correct but ruinously slow); removing the narrow type removes the
    /// need for either.
    page_id: u64,
    /// Page size in bytes. Offsets at or beyond this are rejected.
    page_size: usize,
    /// Number of `u64` words in each bitmap.
    word_count: usize,
    /// The two bitmaps. Index selected by `current`.
    ///
    /// `Box<[AtomicU64]>` rather than `Vec`: the length is fixed at
    /// construction, and a never-resized allocation means a future JIT inline
    /// barrier can bake in the base address the way
    /// `CardTable::jit_cards_addr` does.
    buffers: [Box<[AtomicU64]>; 2],
    /// Index (0 or 1) of the buffer mutators are currently dirtying.
    current: AtomicUsize,
    /// See [`ZRememberedSetStats::words_scanned`].
    words_scanned: AtomicU64,
    /// See [`ZRememberedSetStats::words_skipped`].
    words_skipped: AtomicU64,
    /// See [`ZRememberedSetStats::bits_newly_set`].
    bits_newly_set: AtomicU64,
}

impl ZRememberedSet {
    /// Create an empty remembered set covering `page_size` bytes of the old
    /// page identified by `page_id`.
    ///
    /// Both buffers are allocated up front. Lazy allocation would put a
    /// branch-and-maybe-allocate on the store barrier's fast path, which is
    /// the last place in the VM that can afford one.
    pub fn new(page_id: u64, page_size: usize) -> Self {
        // `div_ceil` twice rather than one combined shift so a page size that
        // is not a multiple of the grain (or of 512) still gets a bit for its
        // tail bytes instead of silently losing them.
        let grains = page_size.div_ceil(Z_REMSET_GRAIN_BYTES);
        let word_count = grains.div_ceil(Z_REMSET_BITS_PER_WORD).max(1);

        let make = || -> Box<[AtomicU64]> {
            let mut v: Vec<AtomicU64> = Vec::with_capacity(word_count);
            for _ in 0..word_count {
                v.push(AtomicU64::new(0));
            }
            v.into_boxed_slice()
        };

        Self {
            page_id,
            page_size,
            word_count,
            buffers: [make(), make()],
            current: AtomicUsize::new(0),
            words_scanned: AtomicU64::new(0),
            words_skipped: AtomicU64::new(0),
            bits_newly_set: AtomicU64::new(0),
        }
    }

    /// The old page this set belongs to.
    #[inline]
    pub fn page_id(&self) -> u64 {
        self.page_id
    }

    /// Bytes of page this set covers.
    #[inline]
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    /// Words in one bitmap.
    #[inline]
    pub fn word_count(&self) -> usize {
        self.word_count
    }

    /// Index of the buffer mutators are dirtying.
    ///
    /// `Acquire`: the swap publishes a freshly-zeroed buffer with a `Release`
    /// store of this index, so a mutator that reads the new index must also
    /// observe the zeroing — otherwise it could `fetch_or` a bit and then have
    /// the wipe erase it, losing a genuine old→young edge.
    #[inline]
    fn current_index(&self) -> usize {
        self.current.load(Ordering::Acquire) & 1
    }

    /// Index of the snapshot buffer the collector scans (the other one).
    #[inline]
    fn snapshot_index(&self) -> usize {
        self.current_index() ^ 1
    }

    /// Translate a page-relative byte offset into `(word index, bit shift)`.
    ///
    /// Returns `None` for an offset outside the page. Out-of-range offsets are
    /// *ignored* rather than panicking, matching
    /// [`CardTable::mark_dirty`](crate::card_table::CardTable::mark_dirty):
    /// the barrier can legitimately fire for an address that is being
    /// concurrently promoted or unmapped, and a GC barrier must never be the
    /// thing that aborts the VM. The caller learns about the drop through the
    /// `bool` return of [`Self::remember`], which
    /// [`ZRememberedSetTable::remember`] turns into a page coarsening rather
    /// than a silent loss.
    #[inline]
    fn locate(&self, page_relative_offset: usize) -> Option<(usize, u32)> {
        if page_relative_offset >= self.page_size {
            return None;
        }
        let bit = page_relative_offset >> Z_REMSET_GRAIN_SHIFT;
        let word = bit / Z_REMSET_BITS_PER_WORD;
        if word >= self.word_count {
            return None;
        }
        Some((word, (bit % Z_REMSET_BITS_PER_WORD) as u32))
    }

    /// Record that the reference slot at `page_relative_offset` may point into
    /// the young generation. **The mutator fast path.**
    ///
    /// Returns `true` if the offset was in range and is now remembered,
    /// `false` if it was out of range (and therefore *not* recorded — the
    /// caller must coarsen; see [`Self::locate`]).
    ///
    /// # Why `fetch_or` needs no CAS loop
    ///
    /// `fetch_or` is a single atomic read-modify-write. It cannot fail, so
    /// there is nothing to retry. The operation is idempotent (setting a set
    /// bit is a no-op) and commutative (two threads setting different bits in
    /// the same word compose in either order to the same word), so no thread
    /// ever needs to observe another's result before deciding what to write —
    /// which is precisely the condition under which a `compare_exchange` loop
    /// is required. A CAS loop here would be strictly worse: it costs a
    /// separate load plus a branch, and under contention on a hot page it
    /// retries, whereas `fetch_or` lowers to one `lock or` on x86-64 and one
    /// `ldset`/`stset` on AArch64 LSE.
    ///
    /// Contrast [`CardTable::mark_dirty`](crate::card_table::CardTable::mark_dirty),
    /// which *does* use `compare_exchange` — because it needs to know whether
    /// the card transitioned clean→dirty in order to push the index onto a
    /// tracking list. This bitmap has no tracking list (the zero-word skip in
    /// [`Self::iterate`] replaces it), so it needs no transition detection on
    /// the fast path.
    ///
    /// # Ordering
    ///
    /// `Release`. The bit is a *publication* of the reference store that
    /// preceded it: a collector that acquire-loads this word and sees the bit
    /// must also see the stored reference in the field, or it would scan the
    /// slot and read the pre-store value. `Release` on the RMW plus `Acquire`
    /// on [`Self::iterate`]'s load is the same release/acquire pairing the
    /// card table uses (`compare_exchange(Release, ..)` / `load(Acquire)`).
    /// On x86-64 this is free — `lock or` is already sequentially consistent.
    #[inline]
    pub fn remember(&self, page_relative_offset: usize) -> bool {
        let (word, shift) = match self.locate(page_relative_offset) {
            Some(loc) => loc,
            None => return false,
        };
        let mask = 1u64 << shift;
        let buffer = &self.buffers[self.current_index()];
        let previous = buffer[word].fetch_or(mask, Ordering::Release);
        if previous & mask == 0 {
            // Relaxed: a pure diagnostic counter. It orders no other memory
            // and nothing reads it to make a decision.
            self.bits_newly_set.fetch_add(1, Ordering::Relaxed);
        }
        true
    }

    /// Is the slot at `page_relative_offset` remembered in the buffer mutators
    /// are currently dirtying?
    #[inline]
    pub fn is_remembered(&self, page_relative_offset: usize) -> bool {
        self.is_remembered_in(self.current_index(), page_relative_offset)
    }

    /// Is the slot remembered in the snapshot the collector scans (the buffer
    /// that was `current` before the last [`Self::swap`])?
    #[inline]
    pub fn is_remembered_in_snapshot(&self, page_relative_offset: usize) -> bool {
        self.is_remembered_in(self.snapshot_index(), page_relative_offset)
    }

    #[inline]
    fn is_remembered_in(&self, buffer_index: usize, page_relative_offset: usize) -> bool {
        match self.locate(page_relative_offset) {
            // `Acquire`: pairs with `remember`'s `Release`. See that method.
            Some((word, shift)) => {
                self.buffers[buffer_index][word].load(Ordering::Acquire) & (1u64 << shift) != 0
            }
            None => false,
        }
    }

    /// Visit every remembered slot in the buffer mutators are dirtying,
    /// passing each one's **grain-aligned page-relative byte offset**.
    ///
    /// Prefer [`Self::iterate_snapshot`] in the collector: this variant races
    /// with concurrent `remember` calls and is here for tests, diagnostics,
    /// and the non-double-buffered (fully stop-the-world) case.
    pub fn iterate<F: FnMut(usize)>(&self, f: F) {
        self.iterate_buffer(self.current_index(), f);
    }

    /// Visit every remembered slot in the collector's stable snapshot — the
    /// buffer that was `current` before the last [`Self::swap`].
    ///
    /// This is the young collection's root-scan entry point: each yielded
    /// offset is a slot in this old page that may hold a young reference, so
    /// `page_base + offset` is a root to trace from.
    pub fn iterate_snapshot<F: FnMut(usize)>(&self, f: F) {
        self.iterate_buffer(self.snapshot_index(), f);
    }

    /// The scan itself.
    ///
    /// **Skips zero words whole.** This is the difference between a minor GC
    /// that is cheap and one that costs what the whole-heap mark-sweep it is
    /// replacing costs. A naive `for bit in 0..capacity` loop touches 64
    /// positions per 512 bytes of page; this touches one. On a page with a
    /// handful of old→young edges — the overwhelmingly common case, which is
    /// exactly why generational collection pays — the loop is one load, one
    /// test, one branch per 512 bytes, i.e. the same number of memory touches
    /// a card-table byte scan would make over the same span.
    ///
    /// Within a non-zero word, `trailing_zeros` + `w &= w - 1` (clear the
    /// lowest set bit) walks *only* the set bits: k iterations for k edges,
    /// never 64.
    fn iterate_buffer<F: FnMut(usize)>(&self, buffer_index: usize, mut f: F) {
        let buffer = &self.buffers[buffer_index];
        let mut scanned: u64 = 0;
        let mut skipped: u64 = 0;

        for (word_index, cell) in buffer.iter().enumerate() {
            // `Acquire`: pairs with `remember`'s `Release` store so a bit we
            // observe implies we also observe the reference store it
            // published.
            let mut word = cell.load(Ordering::Acquire);
            scanned += 1;
            if word == 0 {
                skipped += 1;
                continue;
            }
            let word_base = word_index * Z_REMSET_BYTES_PER_WORD;
            while word != 0 {
                let bit = word.trailing_zeros() as usize;
                let offset = word_base + (bit << Z_REMSET_GRAIN_SHIFT);
                // Defensive: bits past the page's tail can only exist if a
                // caller reached around `remember`, but a bogus offset handed
                // to the collector becomes a wild root, so screen it here too.
                if offset < self.page_size {
                    f(offset);
                }
                // Clear the lowest set bit and continue: k iterations for k
                // set bits, not 64.
                word &= word - 1;
            }
        }

        // Relaxed: diagnostics, ordering nothing.
        self.words_scanned.fetch_add(scanned, Ordering::Relaxed);
        self.words_skipped.fetch_add(skipped, Ordering::Relaxed);
    }

    /// Collect every remembered offset in the snapshot into a `Vec`.
    ///
    /// Convenience for callers that want to drop all GC-side borrows before
    /// walking the edges. Prefer [`Self::iterate_snapshot`] on the hot path —
    /// this allocates.
    pub fn snapshot_offsets(&self) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::new();
        self.iterate_snapshot(|offset| out.push(offset));
        out
    }

    /// Flip the roles of the two bitmaps: the buffer mutators have been
    /// dirtying becomes the collector's snapshot, and a freshly-zeroed buffer
    /// becomes the mutators' target.
    ///
    /// # INVARIANT: stop-the-world only
    ///
    /// This must run at a safepoint with every mutator parked, for the same
    /// reason [`CardTable::flush_all`](crate::card_table::CardTable::flush_all)
    /// must. A mutator that has already executed
    /// [`Self::current_index`] but not yet its `fetch_or` holds a stale buffer
    /// index across the flip, and its bit then lands in the buffer we just
    /// wiped and handed to the collector. Two outcomes, one of them fatal:
    ///
    /// * the bit lands *before* the wipe → it is erased. A genuine old→young
    ///   edge is lost, the young collection does not see the root, and a live
    ///   object is freed. **Use-after-free.**
    /// * the bit lands *after* the wipe, into the buffer being scanned → the
    ///   collector may or may not see it. Seeing it is harmless (a remembered
    ///   set is allowed to over-approximate); not seeing it is the same
    ///   use-after-free.
    ///
    /// There is no cheap barrier-side fix — that is exactly why ZGC swaps at
    /// mark start, inside the pause.
    ///
    /// # Ordering
    ///
    /// The incoming buffer is zeroed with `Relaxed` stores and then published
    /// by a single `Release` store of the index. `Release` is what makes the
    /// zeroing visible to any mutator that subsequently `Acquire`-loads the
    /// index in [`Self::current_index`]. Strictly, the safepoint's own
    /// park/unpark handshake already provides that edge; the pairing is kept
    /// anyway so this type is sound on its own terms rather than only in the
    /// presence of a correct safepoint, and it costs one store.
    pub fn swap(&self) {
        let current = self.current.load(Ordering::Relaxed) & 1;
        let incoming = current ^ 1;

        // `incoming` is last cycle's snapshot. Under the STW invariant nobody
        // is reading or writing it: mutators are parked and the collector
        // finished scanning it before the previous cycle ended.
        for cell in self.buffers[incoming].iter() {
            cell.store(0, Ordering::Relaxed);
        }

        self.current.store(incoming, Ordering::Release);
    }

    /// Zero both bitmaps and reset nothing else (the lifetime scan counters
    /// are deliberately kept — they are cumulative diagnostics).
    ///
    /// STW only, for the same reason as [`Self::swap`]: a concurrent
    /// `remember` racing a clear loses its edge.
    pub fn clear(&self) {
        for buffer in self.buffers.iter() {
            for cell in buffer.iter() {
                cell.store(0, Ordering::Relaxed);
            }
        }
    }

    /// Zero only the collector's snapshot buffer, after it has been scanned.
    ///
    /// Optional: [`Self::swap`] wipes the incoming buffer anyway. Calling this
    /// as soon as the scan finishes just returns the pages to a clean state
    /// earlier, which makes [`Self::stats`] readable mid-cycle.
    pub fn clear_snapshot(&self) {
        for cell in self.buffers[self.snapshot_index()].iter() {
            cell.store(0, Ordering::Relaxed);
        }
    }

    /// Bits currently set in the buffer mutators are dirtying.
    ///
    /// Computed by popcount over the bitmap rather than kept as a live
    /// counter, deliberately: a counter would be a process-shared cache line
    /// incremented by every reference store in the VM.
    pub fn bits_set(&self) -> usize {
        self.count_bits(self.current_index())
    }

    /// Bits currently set in the collector's snapshot.
    pub fn bits_set_in_snapshot(&self) -> usize {
        self.count_bits(self.snapshot_index())
    }

    fn count_bits(&self, buffer_index: usize) -> usize {
        let mut total: usize = 0;
        for cell in self.buffers[buffer_index].iter() {
            total += cell.load(Ordering::Acquire).count_ones() as usize;
        }
        total
    }

    /// Bytes of remembered-set metadata this set retains: both bitmaps plus
    /// the struct itself. Compare
    /// [`CardTable::retained_bytes`](crate::card_table::CardTable::retained_bytes),
    /// which measures the same thing for the card-table representation.
    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + 2 * self.word_count * std::mem::size_of::<AtomicU64>()
    }

    /// Snapshot of this set's counters.
    pub fn stats(&self) -> ZRememberedSetStats {
        ZRememberedSetStats {
            page_id: self.page_id,
            page_size: self.page_size,
            grain_bytes: Z_REMSET_GRAIN_BYTES,
            word_count: self.word_count,
            bits_set: self.bits_set(),
            bits_set_snapshot: self.bits_set_in_snapshot(),
            words_scanned: self.words_scanned.load(Ordering::Relaxed),
            words_skipped: self.words_skipped.load(Ordering::Relaxed),
            bits_newly_set: self.bits_newly_set.load(Ordering::Relaxed),
            retained_bytes: self.retained_bytes(),
        }
    }
}

impl std::fmt::Debug for ZRememberedSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZRememberedSet")
            .field("page_id", &self.page_id)
            .field("page_size", &self.page_size)
            .field("word_count", &self.word_count)
            .field("current", &self.current_index())
            .field("bits_set", &self.bits_set())
            .field("bits_set_snapshot", &self.bits_set_in_snapshot())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Page-id namespace
// ---------------------------------------------------------------------------

/// Which numbering a `page_id` belongs to.
///
/// # Why this type exists (2026-08-07, audit finding N7)
///
/// A page id in this module is **only** a hash key. Nothing here can look at
/// one and tell where it came from, and two different numberings of the same
/// pages are numerically indistinguishable:
/// [`ZPageAllocator`](crate::zgc::page::ZPageAllocator)'s ids start at 1
/// and count allocations, a range model's indices start at 0 and count
/// addresses, and both are small integers. Pair a table registered from one
/// with a context that answers in the other and `register_old_page` and
/// [`ZStoreBarrier::record`] address **different sets for the same page**. The
/// edge is written under one key and read under another; the minor cycle never
/// sees it; the young object it protected is freed while live. There is no
/// error, no coarsening (the ids collide rather than miss — that is what makes
/// it silent) and no repeatable symptom.
///
/// The audit found the module in exactly that state: [`ZGenerationContext`]'s
/// doc required allocator ids, and the module's only shipped implementation
/// ([`ZRangeGenerationContext`]) returned dense indices from its own model. The
/// mismatch was harmless in fact — the range context is constructed nowhere but
/// this module's own tests, which register the same dense ids they later look
/// up — and unexpressible only by convention. This enum makes it expressible,
/// which is the point: a numbering that can be *named* can be *checked*.
///
/// # How it is enforced
///
/// * [`ZGenerationContext::page_id_namespace`] declares what an implementation
///   answers in. It is a **provided** method defaulting to [`Self::Allocator`],
///   so every existing implementation keeps compiling and every existing
///   implementation's default is the truth (`adapters::ZHeapGenerationContext`
///   and the integration suite's `HeapGenerationContext` both return
///   `ZPageReal::id()`).
/// * [`ZRememberedSetTable`] latches the namespace of its first registration.
///   [`ZRememberedSetTable::register_old_page`] declares `Allocator` — which is
///   what all four of its production call sites mean — and
///   [`ZRememberedSetTable::register_old_page_in`] is the explicit form.
/// * [`ZStoreBarrier::context_namespace_matches`] compares the two, and the
///   barrier's hot path `debug_assert!`s it. A test can call it directly, so
///   the pin holds in release builds too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ZPageIdNamespace {
    /// [`ZPageAllocator`]'s monotonic ids — the value
    /// [`crate::zgc::page::ZPageReal::id`] reports. Starts at **1**, increments
    /// once per page allocation in allocation order (not address order), and is
    /// never recycled downward when a page is freed. This is the production
    /// namespace: every id that reaches this module from a real heap is one of
    /// these.
    ///
    /// [`ZPageAllocator`]: crate::zgc::page::ZPageAllocator
    Allocator,
    /// Ids a [`ZGenerationContext`] derives for itself — typically a dense index
    /// from 0 obtained by address arithmetic over a model of the heap.
    ///
    /// Valid **only** against a [`ZRememberedSetTable`] registered from the same
    /// context. Such a context must register its own pages (see
    /// [`ZRangeGenerationContext::register_pages`]) rather than let anything
    /// else populate the table, because nothing else knows this numbering.
    ContextLocal,
}

impl ZPageIdNamespace {
    /// Latch encoding. `0` is reserved for "nothing registered yet", so these
    /// start at 1.
    const fn as_u8(self) -> u8 {
        match self {
            ZPageIdNamespace::Allocator => 1,
            ZPageIdNamespace::ContextLocal => 2,
        }
    }

    /// Inverse of [`Self::as_u8`]; `None` for the unset latch value `0`.
    const fn from_u8(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(ZPageIdNamespace::Allocator),
            2 => Some(ZPageIdNamespace::ContextLocal),
            _ => None,
        }
    }

    /// Short name, for logs and assertion messages.
    pub const fn as_str(self) -> &'static str {
        match self {
            ZPageIdNamespace::Allocator => "allocator",
            ZPageIdNamespace::ContextLocal => "context-local",
        }
    }
}

// ---------------------------------------------------------------------------
// ZRememberedSetTable
// ---------------------------------------------------------------------------

/// What [`ZRememberedSetTable::remember`] managed to record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZRememberOutcome {
    /// The exact slot offset was recorded in the page's bitmap.
    Precise,
    /// The page had no registered remembered set, or the offset fell outside
    /// its bitmap. The page id was added to the coarse set instead: the next
    /// young collection must scan **the whole page** conservatively.
    ///
    /// This exists so a barrier can never *silently drop* an old→young edge.
    /// Dropping one frees a live object; over-approximating one costs a scan.
    Coarsened,
}

/// `page_id -> ZRememberedSet` for the **old** generation's pages.
///
/// # Lock discipline (this is a real bug this codebase has already hit)
///
/// Every accessor takes the `RwLock`, clones the `Arc` it needs, and **drops
/// the guard before returning**. Callers therefore never hold the registry
/// guard while doing page work. Holding a registry guard across work that can
/// block is a documented lock-cycle source in this tree: a registry read lock
/// held across a call that itself acquires a heap/arena lock, while another
/// thread holds that heap lock and reaches for the registry write lock, is a
/// classic ABBA deadlock — and a GC barrier is exactly the code that runs with
/// arbitrary VM locks already held. Cloning an `Arc` costs one relaxed
/// increment; do that instead.
///
/// The same rule applies to bulk operations: [`Self::swap_all`] snapshots the
/// `Arc`s into a `Vec` under the read lock, releases it, and only then touches
/// the sets.
pub struct ZRememberedSetTable {
    /// Registered old pages. Young pages are absent by construction.
    ///
    /// Keyed by `u64` — see the widening note on [`ZRememberedSet::page_id`].
    /// A `u32` key here is an aliasing bug on a timer, not a space saving:
    /// `FxHashMap`'s bucket count is driven by the entry count, not by the key
    /// width, so the narrow key bought nothing and cost correctness.
    sets: RwLock<FxHashMap<u64, Arc<ZRememberedSet>>>,
    /// Pages that must be scanned in full because an edge could not be
    /// recorded precisely (see [`ZRememberOutcome::Coarsened`]).
    ///
    /// A `Mutex<FxHashSet>` rather than a lock-free structure because this is
    /// the *cold* path — it fires only when the page allocator failed to
    /// pre-register a page, which should be never.
    coarse: Mutex<FxHashSet<u64>>,
    /// Which numbering the keys of `sets` and `coarse` are in.
    ///
    /// [`ZPageIdNamespace::as_u8`] encoding, `0` = nothing registered yet.
    /// Latched by the first registration and never changed afterwards; a second
    /// registration in a different namespace is a wiring bug, not a state
    /// change, so it logs and `debug_assert!`s rather than overwriting.
    ///
    /// `Relaxed` throughout, deliberately: this word guards no data and
    /// publishes none. It is read only by diagnostics
    /// ([`ZStoreBarrier::context_namespace_matches`]) and by the mismatch
    /// warning; the bitmaps' `fetch_or`/`Release` argument on
    /// [`ZRememberedSet::remember`] and [`ZRememberedSet::swap`] is untouched by
    /// it and must stay that way — nothing here may become a synchronisation
    /// edge that the swap invariant then depends on.
    namespace: AtomicU8,
}

impl Default for ZRememberedSetTable {
    fn default() -> Self {
        Self::new()
    }
}

impl ZRememberedSetTable {
    /// An empty table.
    pub fn new() -> Self {
        Self {
            sets: RwLock::new(FxHashMap::default()),
            coarse: Mutex::new(FxHashSet::default()),
            namespace: AtomicU8::new(0),
        }
    }

    /// Register (or look up) the remembered set for an old page, whose id is in
    /// the [`ZPageIdNamespace::Allocator`] namespace.
    ///
    /// Call this when a page enters the old generation — at promotion, or when
    /// a page is allocated directly into old. `page_size` is plain data: this
    /// module deliberately does not import the ZGC page types, so the caller
    /// passes the size it already knows.
    ///
    /// Idempotent: a second call with the same `page_id` returns the existing
    /// set and ignores `page_size` (the page cannot change size under us).
    ///
    /// # The namespace this declares (2026-08-07)
    ///
    /// `page_id` is asserted to be [`crate::zgc::page::ZPageReal::id`]'s value,
    /// which is what every production caller passes (`adapters::register_old_pages`
    /// and the integration suite both call `page.id()`). A context that answers
    /// in its own numbering must use [`Self::register_old_page_in`] instead — see
    /// [`ZPageIdNamespace`] for what goes wrong when the two are mixed.
    pub fn register_old_page(&self, page_id: u64, page_size: usize) -> Arc<ZRememberedSet> {
        self.register_old_page_in(ZPageIdNamespace::Allocator, page_id, page_size)
    }

    /// [`Self::register_old_page`], with the id's namespace stated explicitly.
    ///
    /// The first call on a table latches `namespace`; every later call must
    /// agree. A disagreement is the N7 defect happening — one numbering writing
    /// the bits and another reading them — so it logs at `error` and
    /// `debug_assert!`s. It does *not* refuse the registration: coarsening a
    /// page is recoverable, and in a release build the collection is still
    /// correct-if-slow rather than aborted.
    pub fn register_old_page_in(
        &self,
        namespace: ZPageIdNamespace,
        page_id: u64,
        page_size: usize,
    ) -> Arc<ZRememberedSet> {
        self.latch_namespace(namespace);
        // Fast path: already registered. Read lock, clone, drop.
        if let Some(existing) = self.get(page_id) {
            return existing;
        }
        let created = Arc::new(ZRememberedSet::new(page_id, page_size));
        let mut guard = self.sets.write();
        // Re-check under the write lock: another thread may have registered
        // the page between our read and our write.
        let handle = Arc::clone(guard.entry(page_id).or_insert(created));
        drop(guard);
        // A page that had been coarsened now has precise storage again.
        self.coarse.lock().remove(&page_id);
        tracing::debug!(
            target: "zgc::remembered",
            page_id,
            page_size,
            retained_bytes = handle.retained_bytes(),
            "registered old-page remembered set",
        );
        handle
    }

    /// The namespace this table's keys are in, or `None` if nothing has been
    /// registered yet.
    ///
    /// An empty table is compatible with *any* context: there is no key to be
    /// wrong about yet. That is why this is an `Option` rather than a default of
    /// [`ZPageIdNamespace::Allocator`] — a default would make
    /// [`ZStoreBarrier::context_namespace_matches`] report a conflict for a
    /// perfectly legal empty-table + context-local-context pairing, which is
    /// exactly the configuration [`ZRangeGenerationContext::register_pages`]
    /// starts from.
    pub fn page_id_namespace(&self) -> Option<ZPageIdNamespace> {
        ZPageIdNamespace::from_u8(self.namespace.load(Ordering::Relaxed))
    }

    /// Latch `namespace` on first registration; complain about a later
    /// disagreement.
    ///
    /// `Relaxed` compare-exchange: this word orders nothing (see the field's
    /// note). A race between two first-registrations in the *same* namespace
    /// resolves to that namespace either way; a race between two *different*
    /// namespaces is the bug being reported, and which of them wins the latch
    /// does not change that it is reported.
    fn latch_namespace(&self, namespace: ZPageIdNamespace) {
        let want = namespace.as_u8();
        match self
            .namespace
            .compare_exchange(0, want, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => {}
            Err(existing) if existing == want => {}
            Err(existing) => {
                let held = ZPageIdNamespace::from_u8(existing)
                    .map(ZPageIdNamespace::as_str)
                    .unwrap_or("<corrupt>");
                tracing::error!(
                    target: "zgc::remembered",
                    latched = held,
                    offered = namespace.as_str(),
                    page_ids = self.len(),
                    "remembered-set table registered in TWO page-id namespaces: \
                     the same page is keyed two ways, so an old->young edge \
                     recorded under one key is looked up under the other and \
                     the young object it protects is freed while live. See \
                     ZPageIdNamespace.",
                );
                debug_assert!(
                    false,
                    "ZRememberedSetTable page-id namespace conflict: latched \
                     {held}, offered {}",
                    namespace.as_str(),
                );
            }
        }
    }

    /// The remembered set for `page_id`, if it is a registered old page.
    ///
    /// Returns a cloned `Arc` with the guard already dropped — see the type's
    /// lock-discipline note.
    pub fn get(&self, page_id: u64) -> Option<Arc<ZRememberedSet>> {
        let guard = self.sets.read();
        let found = guard.get(&page_id).map(Arc::clone);
        drop(guard);
        found
    }

    /// Forget a page: it was freed, or relocated into a fresh page id, or
    /// demoted. Returns the removed set so the caller can inspect it.
    ///
    /// This is the O(1) discard that a per-page remembered set buys and a flat
    /// card table cannot offer — see the module header's point 1.
    pub fn remove(&self, page_id: u64) -> Option<Arc<ZRememberedSet>> {
        let mut guard = self.sets.write();
        let removed = guard.remove(&page_id);
        drop(guard);
        self.coarse.lock().remove(&page_id);
        if removed.is_some() {
            tracing::debug!(
                target: "zgc::remembered",
                page_id,
                "dropped old-page remembered set",
            );
        }
        removed
    }

    /// Record a possible old→young reference at `page_relative_offset` in
    /// `page_id`.
    ///
    /// Never silently drops an edge: an unregistered page or an out-of-range
    /// offset coarsens the whole page instead (see [`ZRememberOutcome`]).
    pub fn remember(&self, page_id: u64, page_relative_offset: usize) -> ZRememberOutcome {
        if let Some(set) = self.get(page_id) {
            // The guard is already dropped here — `set` is an owned Arc.
            if set.remember(page_relative_offset) {
                return ZRememberOutcome::Precise;
            }
        }
        self.coarse.lock().insert(page_id);
        ZRememberOutcome::Coarsened
    }

    /// Registered old page ids, as an owned snapshot.
    ///
    /// Owned rather than an iterator, so the caller cannot accidentally hold
    /// the registry guard while walking pages.
    pub fn page_ids(&self) -> Vec<u64> {
        let guard = self.sets.read();
        let ids: Vec<u64> = guard.keys().copied().collect();
        drop(guard);
        ids
    }

    /// Every registered set, as an owned snapshot of cloned `Arc`s.
    pub fn snapshot(&self) -> Vec<Arc<ZRememberedSet>> {
        let guard = self.sets.read();
        let sets: Vec<Arc<ZRememberedSet>> = guard.values().map(Arc::clone).collect();
        drop(guard);
        sets
    }

    /// Flip every registered page's bitmaps. **Stop-the-world only** — see
    /// [`ZRememberedSet::swap`].
    ///
    /// Called once at young-mark start. After this returns, every page's
    /// snapshot holds exactly the edges dirtied since the previous swap, and
    /// mutators resume into empty current buffers.
    pub fn swap_all(&self) {
        // Snapshot under the read lock, release it, then work. Never hold the
        // registry guard across per-page work.
        let sets = self.snapshot();
        for set in &sets {
            set.swap();
        }
        tracing::debug!(
            target: "zgc::remembered",
            pages = sets.len(),
            "swapped remembered-set buffers for young-mark start",
        );
    }

    /// Zero every registered page's bitmaps. STW only.
    pub fn clear_all(&self) {
        for set in self.snapshot() {
            set.clear();
        }
        self.coarse.lock().clear();
    }

    /// Take the set of pages that must be scanned in full this cycle, clearing
    /// it.
    ///
    /// A non-empty result is a **wiring bug, not a heap bug**: it means the
    /// store barrier saw a store into an old page that was never registered
    /// with [`Self::register_old_page`]. The collection stays correct (the
    /// page is scanned conservatively), but the page pays a full scan.
    pub fn take_coarse_pages(&self) -> Vec<u64> {
        let mut guard = self.coarse.lock();
        let pages: Vec<u64> = guard.iter().copied().collect();
        guard.clear();
        drop(guard);
        if !pages.is_empty() {
            tracing::warn!(
                target: "zgc::remembered",
                pages = pages.len(),
                "old pages coarsened: a store barrier hit an old page with no \
                 registered remembered set, so the whole page must be scanned. \
                 The page allocator should call register_old_page at promotion.",
            );
        }
        pages
    }

    /// Number of registered old pages.
    pub fn len(&self) -> usize {
        let guard = self.sets.read();
        let n = guard.len();
        drop(guard);
        n
    }

    /// Are there no registered old pages?
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total bitmap memory across every registered page.
    ///
    /// This is the number to compare against
    /// [`CardTable::retained_bytes`](crate::card_table::CardTable::retained_bytes)
    /// when deciding whether the bitmap representation is earning its 16x.
    pub fn total_retained_bytes(&self) -> usize {
        self.snapshot().iter().map(|s| s.retained_bytes()).sum()
    }

    /// Total bits set across every registered page's current buffer.
    pub fn total_bits_set(&self) -> usize {
        self.snapshot().iter().map(|s| s.bits_set()).sum()
    }

    /// Total bits set across every registered page's snapshot buffer — i.e.
    /// the number of old→young edges this young collection will trace.
    pub fn total_bits_set_in_snapshot(&self) -> usize {
        self.snapshot()
            .iter()
            .map(|s| s.bits_set_in_snapshot())
            .sum()
    }
}

impl std::fmt::Debug for ZRememberedSetTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZRememberedSetTable")
            .field("pages", &self.len())
            .field("retained_bytes", &self.total_retained_bytes())
            .field("page_id_namespace", &self.page_id_namespace())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// ZGenerationContext
// ---------------------------------------------------------------------------

/// The generation/page questions the store barrier must ask about a raw
/// address.
///
/// Deliberately a trait, not a concrete type: the ZGC page, virtual-address
/// and barrier modules are being written in parallel with this one, so this
/// module depends on a *contract* rather than on their types. Wiring it up is
/// a matter of implementing three methods on whatever ends up owning the
/// generation split.
///
/// # ⚠ Address domain: these `u64`s are MACHINE ADDRESSES, not heap offsets
///
/// *Recorded 2026-08-07, when the ZGC submodules were first cross-checked
/// against one another.*
///
/// Every `addr: u64` on this trait is a **raw process address** — the thing you
/// could dereference. It is emphatically **not** the domain
/// [`crate::zgc::barrier::ZBarrierContext`] traffics in: a colored reference
/// word under [`crate::zgc::vaddr`] carries a **42-bit heap offset** from
/// [`ZVirtualAddressSpace::base`](crate::zgc::vaddr::ZVirtualAddressSpace::base),
/// and the load barrier's fast path hands that offset back after masking with
/// [`Z_OFFSET_MASK`](crate::zgc::vaddr::Z_OFFSET_MASK). The two are separated by
/// the heap base, which on Linux is around `0x7f…` (≈2^47) and on Windows is
/// small — which is exactly why feeding one where the other is expected is
/// invisible on the Windows dev host and catastrophic on Linux.
///
/// This module is on the machine-address side **correctly and deliberately**:
/// [`ZRangeGenerationContext`] range-compares against real heap bounds, the
/// store barrier is handed a *field slot address* (something the mutator just
/// wrote through), and `ZgcRealHeap` is today one contiguous
/// [`Arena`](crate::arena::Arena) whose addresses are the only names it has.
/// Nothing here needs to change.
///
/// What that costs is a **conversion obligation at the seam**: a caller holding
/// a value that came out of the load barrier holds an *offset* and must add the
/// heap base — [`ZVirtualAddressSpace::address_for_offset`](crate::zgc::vaddr::ZVirtualAddressSpace::address_for_offset)
/// is that function — before calling [`Self::is_old`], [`Self::is_young`] or
/// [`Self::page_of`], or before passing it as
/// [`ZStoreBarrier::on_reference_store`]'s `stored_value`. Skipping the
/// conversion does not fail: `is_old` and `is_young` both answer `false` for an
/// offset that lands below the heap base, so every cross-generation edge is
/// silently *filtered* and the remembered set stays empty. An empty remembered
/// set is the precise shape of the use-after-free this whole module exists to
/// prevent, and it produces no warning, no coarsening, and no counter — the
/// only visible trace would be `ZStoreBarrierStats::filtered_source_not_old`
/// equal to `stores_seen`.
///
/// If a future implementation *does* answer in the offset domain (a
/// colored-metadata-bit `is_old`, per the recommendation below), it must say so
/// on the impl and every caller must be re-audited: this trait's `u64` carries
/// no evidence of which domain it holds.
///
/// # Implementing the fast path
///
/// [`Self::is_old`] and [`Self::is_young`] sit on **every reference store in
/// the VM**. They must be a handful of instructions with no memory load, no
/// lock, and no allocation. Two viable shapes:
///
/// * **Address-range compare** — two integer comparisons against cached
///   generation bounds. This is what `gen_heap::write_barrier` does (round-5
///   #11: it replaced two object-header dereferences with two range tests
///   against the card table's cached `base_addr`/`region_size`, turning two
///   cold cache misses into two compares). Correct only while each generation
///   is *one contiguous range*. [`ZRangeGenerationContext`] implements this,
///   and it fits `ZgcRealHeap` as it stands today: one arena, which a
///   generational split would carve into a young sub-range and an old
///   sub-range.
/// * **Colored-pointer metadata bit** — ZGC encodes generation in the
///   pointer's high metadata bits. The test becomes a single `and` + branch
///   against a register, with **no memory reference at all**, and it keeps
///   working when the old generation is a sparse set of pages scattered
///   through a multi-mapped virtual address space — which is what ZGC pages
///   actually are, and which the range compare cannot express.
///
/// **Recommendation (revised 2026-08-07): the metadata-bit test is the right
/// destination, but the landed encoding cannot express it yet.** The original
/// text here pointed at `zgc.rs`'s `ZGC_METADATA_SHIFT` / colour masks; those
/// belong to the *simulation* and are superseded. The real encoding is
/// [`crate::zgc::vaddr`], and it deliberately reproduces OpenJDK's
/// **non-generational** layout: 42-bit offset, then exactly four metadata bits
/// — `Z_MARKED0`, `Z_MARKED1`, `Z_REMAPPED`, `Z_FINALIZABLE` — plus
/// `Z_COLORED_TAG` at bit 63. **All four metadata bits are spoken for; there is
/// no free generation bit.** OpenJDK's Generational ZGC widens the metadata
/// field (young/old marked pairs and remapped variants) precisely to make room,
/// so adopting the metadata-bit test here is a change to `vaddr.rs`'s layout —
/// bits 62-46 are reserved-must-be-zero and are where that room would come
/// from — not a change to this module.
///
/// Until that happens the range implementation below is the honest interim,
/// and it is what the tests use.
///
/// # Object semantics
///
/// The barrier asks these questions about a **field slot address** (for
/// [`Self::is_old`] / [`Self::page_of`]) and about a **referent address** (for
/// [`Self::is_young`]). An implementation that answers by object header must
/// therefore be able to go from an interior field address to its containing
/// object; an implementation that answers by address range or metadata bit
/// does not care, which is another reason to prefer those.
pub trait ZGenerationContext {
    /// Does `addr` fall in the old generation?
    fn is_old(&self, addr: u64) -> bool;

    /// Does `addr` fall in the young generation?
    fn is_young(&self, addr: u64) -> bool;

    /// Map `addr` to `(page_id, page_relative_offset)`.
    ///
    /// Returns `None` for an address that is not in a page this context knows
    /// about. Only *old* pages need to be mappable — the barrier calls this
    /// only after [`Self::is_old`] has already said yes.
    ///
    /// The page id is a `u64`, widened from `u32` on 2026-08-07; see
    /// [`ZRememberedSet::page_id`] for why the narrow key was an aliasing bug.
    ///
    /// # Which numbering the id is in (corrected 2026-08-07, audit finding N7)
    ///
    /// This doc used to read: *"must be **the same id
    /// [`crate::zgc::page::ZPageReal::id`] reports** for that page"*. That was
    /// the right requirement for a context over a real heap and the wrong
    /// requirement for the trait, and it was written eighty lines above an
    /// implementation ([`ZRangeGenerationContext`]) that could not satisfy it —
    /// by the same authoring pass, which fixed the id's *width* and did not
    /// notice its *namespace*.
    ///
    /// The correct rule is a pairing rule, and it is now stated by a type:
    ///
    /// > The ids returned here are in the namespace this implementation declares
    /// > via [`Self::page_id_namespace`], and they must be in the **same
    /// > namespace** as the ids used to register pages on the
    /// > [`ZRememberedSetTable`] this context is driven against. It is the
    /// > *pair* that has to agree; neither half is meaningful alone.
    ///
    /// For anything backed by a real [`crate::zgc::page::ZPageAllocator`] that
    /// namespace is [`ZPageIdNamespace::Allocator`] and the id is exactly
    /// `ZPageReal::id()` — the default, and what `adapters::ZHeapGenerationContext`
    /// does. For a self-contained model it is [`ZPageIdNamespace::ContextLocal`]
    /// and the context must register its own pages.
    fn page_of(&self, addr: u64) -> Option<(u64, usize)>;

    /// Which numbering [`Self::page_of`]'s ids are in.
    ///
    /// Defaults to [`ZPageIdNamespace::Allocator`], which is correct for every
    /// implementation that resolves an address through a real
    /// [`ZPageAllocator`](crate::zgc::page::ZPageAllocator) and returns
    /// [`ZPageReal::id`](crate::zgc::page::ZPageReal::id) — i.e. for every
    /// implementation in the tree except [`ZRangeGenerationContext`]. It is a
    /// *provided* method precisely so that stays true without any existing impl
    /// having to be edited: getting the default is not silence, it is the
    /// common case being the default.
    ///
    /// Override it if — and only if — [`Self::page_of`] invents its own ids.
    /// Then the table must be registered through
    /// [`ZRememberedSetTable::register_old_page_in`] with the same value, and
    /// [`ZStoreBarrier::context_namespace_matches`] will say so if it is not.
    fn page_id_namespace(&self) -> ZPageIdNamespace {
        ZPageIdNamespace::Allocator
    }
}

/// A [`ZGenerationContext`] backed by two contiguous address ranges and a
/// fixed old-page stride.
///
/// This is the address-range fast path described on the trait: `is_old` and
/// `is_young` are two integer comparisons each, touching no memory beyond this
/// struct's own (hot, immutable) fields.
///
/// It is a genuine implementation, not a test double — it is the shape that
/// fits `ZgcRealHeap`'s single-arena storage if that arena is split into a
/// young sub-range and an old sub-range. It is also what the tests in this
/// module use, precisely because it has no dependencies on the page modules.
///
/// # ⚠ Its page ids are its OWN (recorded 2026-08-07, audit finding N7)
///
/// [`Self::page_of`] returns a **dense index from 0** into this context's model
/// of the old range: `(addr - old_start) / old_page_size`. That is
/// [`ZPageIdNamespace::ContextLocal`], and it is *not* what
/// [`crate::zgc::page::ZPageReal::id`] reports — allocator ids start at 1, count
/// allocations rather than addresses, and are handed out in allocation order,
/// so index 3 here and page id 3 there are different pages that happen to be the
/// same integer. Numeric collision is the normal case, not the corner: a
/// mismatched pairing therefore *aliases* silently instead of coarsening
/// loudly.
///
/// So the one rule: **a table this context drives must be registered by this
/// context.** [`Self::register_pages`] is that call, and
/// [`ZStoreBarrier::context_namespace_matches`] is the check. Anything else —
/// `adapters::register_old_pages`, or a bare
/// [`ZRememberedSetTable::register_old_page`] — populates the table in the
/// allocator namespace and must not be combined with this type.
///
/// This is also why it is not usable against a `ZGenerationalHeap` for a second,
/// independent reason (`adapters.rs`'s "why `remembered`'s own context cannot do
/// this"): that heap interleaves young and old pages in one granule pool, which
/// no pair of ranges and no stride can describe. `adapters::ZHeapGenerationContext`
/// is the implementation for a real heap.
#[derive(Debug, Clone, Copy)]
pub struct ZRangeGenerationContext {
    young_start: u64,
    young_end: u64,
    old_start: u64,
    old_end: u64,
    old_page_size: usize,
}

impl ZRangeGenerationContext {
    /// Build a context over `[young_start, young_start + young_size)` and
    /// `[old_start, old_start + old_size)`, with the old range divided into
    /// pages of `old_page_size` bytes starting at `old_start`.
    ///
    /// `old_page_size` is clamped to at least 1 so page arithmetic cannot
    /// divide by zero.
    pub fn new(
        young_start: u64,
        young_size: u64,
        old_start: u64,
        old_size: u64,
        old_page_size: usize,
    ) -> Self {
        Self {
            young_start,
            young_end: young_start.saturating_add(young_size),
            old_start,
            old_end: old_start.saturating_add(old_size),
            old_page_size: old_page_size.max(1),
        }
    }

    /// Size of each old page, in bytes. Pass this to
    /// [`Self::register_pages`].
    pub fn old_page_size(&self) -> usize {
        self.old_page_size
    }

    /// Register every page of this context's old range on `table`, in **this
    /// context's** namespace ([`ZPageIdNamespace::ContextLocal`]). Returns the
    /// number of pages registered.
    ///
    /// This is the only supported way to populate a table that this context will
    /// drive — see the namespace warning on the type. Calling
    /// [`ZRememberedSetTable::register_old_page`] by hand instead declares the
    /// *allocator* namespace, and the resulting table + context pair is the N7
    /// defect: `register` writes bit *k* of page *n* and `record` reads bit *k*
    /// of a different page *n*.
    ///
    /// Idempotent, because `register_old_page_in` is.
    ///
    /// # Cost
    ///
    /// One registration — one bitmap pair, `page_size / 32` bytes — per page in
    /// [`Self::old_page_count`], eagerly. That is fine for a context whose old
    /// range is a real heap's old range and catastrophic for one whose range is
    /// a synthetic address-space probe (the `1 << 36`-byte, 8-byte-page context
    /// in this module's tests has 2^33 pages). It is a deliberate loop rather
    /// than lazy registration so that a table paired with this context is
    /// *complete* — an unregistered old page coarsens, and a context that
    /// registers lazily could never distinguish "not yet" from "never".
    pub fn register_pages(&self, table: &ZRememberedSetTable) -> usize {
        let count = self.old_page_count();
        for index in 0..count as u64 {
            table.register_old_page_in(ZPageIdNamespace::ContextLocal, index, self.old_page_size);
        }
        count
    }

    /// Base address of `page_id` under this context's page division.
    ///
    /// `page_id` is in this context's own namespace — see the warning on the
    /// type. This returns a **machine address** — see the address-domain warning
    /// on [`ZGenerationContext`].
    pub fn page_base(&self, page_id: u64) -> u64 {
        self.old_start
            .saturating_add(page_id.saturating_mul(self.old_page_size as u64))
    }

    /// Number of old pages this range covers.
    pub fn old_page_count(&self) -> usize {
        let span = self.old_end.saturating_sub(self.old_start) as usize;
        span.div_ceil(self.old_page_size)
    }
}

impl ZGenerationContext for ZRangeGenerationContext {
    #[inline]
    fn is_old(&self, addr: u64) -> bool {
        addr >= self.old_start && addr < self.old_end
    }

    #[inline]
    fn is_young(&self, addr: u64) -> bool {
        addr >= self.young_start && addr < self.young_end
    }

    /// This context invents its own ids: they are dense indices from 0, not
    /// [`crate::zgc::page::ZPageReal::id`] values. See the type's namespace
    /// warning and [`Self::page_id_namespace`].
    #[inline]
    fn page_id_namespace(&self) -> ZPageIdNamespace {
        ZPageIdNamespace::ContextLocal
    }

    /// Returns a **dense index from 0** into this context's own model of the old
    /// range — [`ZPageIdNamespace::ContextLocal`], declared above. Valid only
    /// against a table registered by [`Self::register_pages`].
    #[inline]
    fn page_of(&self, addr: u64) -> Option<(u64, usize)> {
        if !self.is_old(addr) {
            return None;
        }
        let delta = addr - self.old_start;
        let page = delta / (self.old_page_size as u64);
        // No `u32` range guard any more (removed 2026-08-07 with the page-id
        // widening). It used to read:
        //
        //     if page > u64::from(u32::MAX) { return None; }
        //
        // and it was the *right* handling of the wrong type: reporting "unknown
        // page" made the table coarsen instead of aliasing, so it turned a
        // silent use-after-free into a whole-page scan. With a `u64` key the
        // index is representable and the page is remembered precisely, so the
        // guard has nothing left to defend against — a page index simply cannot
        // exceed `u64::MAX` here, because `delta` is a `u64` and
        // `old_page_size >= 1`.
        let offset = (delta % (self.old_page_size as u64)) as usize;
        Some((page, offset))
    }
}

// ---------------------------------------------------------------------------
// ZStoreBarrier
// ---------------------------------------------------------------------------

/// What [`ZStoreBarrier::on_reference_store`] did with a store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZStoreBarrierOutcome {
    /// Not a cross-generation edge (young→anything, or old→old, or a null
    /// store). Nothing was recorded and nothing needed to be.
    Filtered,
    /// An old→young edge, recorded precisely in the source page's bitmap.
    Precise,
    /// An old→young edge whose source page had no registered remembered set
    /// (or whose offset fell outside it). The page was coarsened; see
    /// [`ZRememberOutcome::Coarsened`].
    Coarsened,
}

/// Counters for a [`ZStoreBarrier`]. Diagnostics only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ZStoreBarrierStats {
    /// Reference stores the barrier examined.
    pub stores_seen: u64,
    /// Stores rejected because the destination slot is not in the old
    /// generation. In an allocation-churn workload this should dominate
    /// everything else — see the ordering note on
    /// [`ZStoreBarrier::on_reference_store`].
    pub filtered_source_not_old: u64,
    /// Stores rejected because the stored value is not young (null, or an
    /// old→old edge).
    pub filtered_value_not_young: u64,
    /// Old→young edges recorded precisely.
    pub remembered_precise: u64,
    /// Old→young edges that had to coarsen their page.
    pub remembered_coarse: u64,
    /// Reference fields remembered by [`ZStoreBarrier::on_promote`].
    pub promotion_fields_remembered: u64,
    /// Promotions processed by [`ZStoreBarrier::on_promote`].
    pub promotions: u64,
}

/// The write-side barrier that populates the remembered set.
///
/// One instance per heap, shared by every mutator. It is cheap to clone the
/// `Arc` it holds and it takes no lock on the fast path beyond the table's
/// read lock (which is released before the bitmap is touched).
pub struct ZStoreBarrier {
    table: Arc<ZRememberedSetTable>,
    stores_seen: AtomicU64,
    filtered_source_not_old: AtomicU64,
    filtered_value_not_young: AtomicU64,
    remembered_precise: AtomicU64,
    remembered_coarse: AtomicU64,
    promotion_fields_remembered: AtomicU64,
    promotions: AtomicU64,
}

impl ZStoreBarrier {
    /// Build a barrier over `table`.
    pub fn new(table: Arc<ZRememberedSetTable>) -> Self {
        Self {
            table,
            stores_seen: AtomicU64::new(0),
            filtered_source_not_old: AtomicU64::new(0),
            filtered_value_not_young: AtomicU64::new(0),
            remembered_precise: AtomicU64::new(0),
            remembered_coarse: AtomicU64::new(0),
            promotion_fields_remembered: AtomicU64::new(0),
            promotions: AtomicU64::new(0),
        }
    }

    /// The remembered-set table this barrier feeds.
    pub fn table(&self) -> &Arc<ZRememberedSetTable> {
        &self.table
    }

    /// Do `ctx`'s page ids and this barrier's table speak the same numbering?
    ///
    /// # Why the barrier is where this can be asked (2026-08-07, finding N7)
    ///
    /// A [`ZRememberedSetTable`] sees registrations and a
    /// [`ZGenerationContext`] sees addresses; neither can tell that the other is
    /// numbering the same pages differently. The barrier is the only object that
    /// holds both, and [`Self::on_reference_store`] is the exact instruction at
    /// which an id produced by one is used as a key into the other. So this is
    /// the check's only possible home.
    ///
    /// `true` when the table is still empty: an unregistered table has no key to
    /// disagree about, and [`ZRangeGenerationContext::register_pages`] legally
    /// starts from one.
    ///
    /// # Why it is a method rather than only a `debug_assert!`
    ///
    /// [`Self::on_reference_store`] `debug_assert!`s this, which costs nothing in
    /// release — and would therefore pin nothing in a release test run, a trap
    /// this tree has hit before. Exposing the predicate lets a test assert it
    /// directly in both profiles.
    pub fn context_namespace_matches(&self, ctx: &dyn ZGenerationContext) -> bool {
        match self.table.page_id_namespace() {
            None => true,
            Some(table_ns) => table_ns == ctx.page_id_namespace(),
        }
    }

    /// The message [`Self::on_reference_store`]'s `debug_assert!` carries. Kept
    /// beside the predicate so the two cannot drift.
    #[cold]
    fn namespace_mismatch_message(&self, ctx: &dyn ZGenerationContext) -> String {
        format!(
            "page-id namespace mismatch: the table is keyed by {}, the \
             generation context answers in {}. Every old->young edge is \
             recorded under one key and looked up under another; the young \
             objects they protect will be freed while live. See \
             ZPageIdNamespace.",
            self.table
                .page_id_namespace()
                .map(ZPageIdNamespace::as_str)
                .unwrap_or("<nothing registered>"),
            ctx.page_id_namespace().as_str(),
        )
    }

    /// Called after a reference store of `stored_value` into the slot at
    /// `field_addr`.
    ///
    /// `field_addr` is the address of the **field slot itself**, not the
    /// object base. That is the one interface difference from
    /// `gen_heap::write_barrier`, which passes the object base because a
    /// 512-byte card cannot distinguish fields anyway. A per-field bitmap can,
    /// so it wants the slot — and giving it the object base instead would
    /// throw away the entire precision advantage that justifies the 16x memory
    /// cost.
    ///
    /// # Ordering of the two tests, and why it is this way round
    ///
    /// The destination check (`is_old(field_addr)`) comes first. In the
    /// workload this whole effort exists to fix — Spring contexts built and
    /// torn down in a loop — the overwhelming majority of reference stores are
    /// into freshly allocated, still-young objects during graph construction.
    /// Testing the *destination* first rejects all of those in one compare,
    /// before touching the stored value at all. `gen_heap::write_barrier`
    /// orders its two range tests the same way, for the same reason.
    ///
    /// A null store (`stored_value == 0`) is filtered before either test: it
    /// creates no edge, and it is common (field clearing during teardown).
    ///
    /// # What this barrier does NOT do
    ///
    /// It records only the *forward* direction (old slot now points at a young
    /// object). It does not log the overwritten value — that is the SATB
    /// barrier's job (see [`crate::satb`]) and a different invariant.
    #[inline]
    pub fn on_reference_store(
        &self,
        ctx: &dyn ZGenerationContext,
        field_addr: u64,
        stored_value: u64,
    ) -> ZStoreBarrierOutcome {
        // Finding N7: the id `ctx` is about to produce is a key into
        // `self.table`, and nothing about a `u64` says which numbering it is in.
        // `debug_assert!` is `if cfg!(debug_assertions)`, so this is compiled but
        // never executed in release — the hot path keeps its "handful of
        // instructions" contract, and `context_namespace_matches` stays callable
        // in both profiles for tests that need to pin it.
        debug_assert!(
            self.context_namespace_matches(ctx),
            "{}",
            self.namespace_mismatch_message(ctx),
        );

        self.stores_seen.fetch_add(1, Ordering::Relaxed);

        // A null store creates no edge.
        if stored_value == 0 {
            self.filtered_value_not_young
                .fetch_add(1, Ordering::Relaxed);
            return ZStoreBarrierOutcome::Filtered;
        }

        // Cheapest rejection first: most stores are into young objects.
        if !ctx.is_old(field_addr) {
            self.filtered_source_not_old.fetch_add(1, Ordering::Relaxed);
            return ZStoreBarrierOutcome::Filtered;
        }

        // Old→old edges are found by the old-generation trace, not by the
        // remembered set.
        if !ctx.is_young(stored_value) {
            self.filtered_value_not_young
                .fetch_add(1, Ordering::Relaxed);
            return ZStoreBarrierOutcome::Filtered;
        }

        match ctx.page_of(field_addr) {
            Some((page_id, offset)) => self.record(page_id, offset),
            None => {
                // `is_old` said yes but the context cannot name the page. Do
                // not drop the edge: there is no page id to coarsen either, so
                // this is the one case a caller must treat as a hard wiring
                // error. Report Coarsened and log once per occurrence at warn
                // level — this path should never execute.
                tracing::warn!(
                    target: "zgc::remembered",
                    field_addr,
                    "store barrier: address is in the old generation but maps \
                     to no page; the old->young edge cannot be recorded",
                );
                self.remembered_coarse.fetch_add(1, Ordering::Relaxed);
                ZStoreBarrierOutcome::Coarsened
            }
        }
    }

    /// Convenience wrapper for callers that hold an object base and a field
    /// offset rather than a slot address.
    #[inline]
    pub fn on_reference_store_in_object(
        &self,
        ctx: &dyn ZGenerationContext,
        object_addr: u64,
        field_offset: usize,
        stored_value: u64,
    ) -> ZStoreBarrierOutcome {
        self.on_reference_store(
            ctx,
            object_addr.saturating_add(field_offset as u64),
            stored_value,
        )
    }

    /// Record an old→young edge whose generation test has already been done.
    ///
    /// Exposed for the collector's own paths ([`Self::on_promote`], and any
    /// re-establishment of edges after a sweep, mirroring
    /// `CardTable::mark_dirty_bulk`'s role in `gen_heap`'s non-moving sweep).
    /// The mutator must go through [`Self::on_reference_store`] so the
    /// generation gating actually happens.
    ///
    /// `page_id` must be in the table's [`ZPageIdNamespace`] — this entry point
    /// has no context to check against, so it is the caller's obligation. There
    /// is nothing this function could assert: a wrong-namespace id is a
    /// perfectly well-formed `u64` that names a real, different page.
    pub fn record(&self, page_id: u64, page_relative_offset: usize) -> ZStoreBarrierOutcome {
        match self.table.remember(page_id, page_relative_offset) {
            ZRememberOutcome::Precise => {
                self.remembered_precise.fetch_add(1, Ordering::Relaxed);
                ZStoreBarrierOutcome::Precise
            }
            ZRememberOutcome::Coarsened => {
                self.remembered_coarse.fetch_add(1, Ordering::Relaxed);
                ZStoreBarrierOutcome::Coarsened
            }
        }
    }

    /// Re-examine a young object's outgoing references after it has been
    /// promoted into the old generation, adding a remembered-set entry for
    /// every field that still points at a young object.
    ///
    /// `promoted_addr` is the object's **final** address in the old
    /// generation. `outgoing` is `(field offset within the object, referent
    /// address)` for each reference-typed field — decoded by the caller,
    /// because object layout lives in `heap.rs`/`gen_heap.rs` and this module
    /// deliberately holds no layout knowledge. Returns the number of fields
    /// that were remembered.
    ///
    /// # Why this call is mandatory, not an optimisation
    ///
    /// Before promotion, every one of this object's references was a
    /// young→young edge, and the store barrier filtered all of them out —
    /// correctly, because a young collection traces the whole young generation
    /// and needs no help finding them. The instant the object becomes old,
    /// those same edges become old→young and the *only* record of them is this
    /// call. Skip it and the next young collection frees every young object
    /// reachable solely through a just-promoted parent.
    ///
    /// # Ordering constraints
    ///
    /// 1. **After the object's final address is fixed.** If promotion copies
    ///    the object, `promoted_addr` must be the destination, not the source.
    ///    Remembering against the source address writes bits into a page the
    ///    object no longer occupies — those bits are then either wiped with the
    ///    dead page or, worse, interpreted as slots of whatever is allocated
    ///    there next.
    /// 2. **After the destination page is registered** with
    ///    [`ZRememberedSetTable::register_old_page`]. Otherwise every field
    ///    coarsens the page (correct, but it costs a full page scan).
    /// 3. **With post-relocation referent addresses.** If promotion happens
    ///    inside a relocating cycle, `outgoing` must carry the referents'
    ///    forwarded addresses. Feeding stale ones records bits for slots whose
    ///    values will be remapped afterwards; the bit is still *set* on the
    ///    right slot, so the edge is not lost, but a referent test done here
    ///    against a stale address can wrongly decide the referent is not young.
    ///    When in doubt, remember the field unconditionally — an
    ///    over-approximated remembered set is always safe.
    /// 4. **Before mutators resume.** A mutator that stores into the promoted
    ///    object between the copy and this call takes the barrier's old→young
    ///    path and sets the bit itself, which is fine; but a mutator that
    ///    stores into it *before* the promotion is published as old takes the
    ///    young→young path and is filtered, and this call — which reads the
    ///    fields — may then race that store. Promotion is a
    ///    stop-the-world/collector-owned operation for exactly this reason.
    pub fn on_promote(
        &self,
        ctx: &dyn ZGenerationContext,
        promoted_addr: u64,
        outgoing: &[(usize, u64)],
    ) -> usize {
        // See the same assertion on `on_reference_store`. This path is cold
        // (collector-driven), but it keys the table the same way.
        debug_assert!(
            self.context_namespace_matches(ctx),
            "{}",
            self.namespace_mismatch_message(ctx),
        );

        self.promotions.fetch_add(1, Ordering::Relaxed);

        let (page_id, base_offset) = match ctx.page_of(promoted_addr) {
            Some(located) => located,
            None => {
                tracing::warn!(
                    target: "zgc::remembered",
                    promoted_addr,
                    fields = outgoing.len(),
                    "on_promote: promoted object maps to no old page — its \
                     old->young edges cannot be recorded (constraints 1 and 2 \
                     on ZStoreBarrier::on_promote)",
                );
                return 0;
            }
        };

        let mut remembered: usize = 0;
        for &(field_offset, referent) in outgoing {
            if referent == 0 {
                continue;
            }
            if !ctx.is_young(referent) {
                continue;
            }
            self.record(page_id, base_offset.saturating_add(field_offset));
            remembered += 1;
        }

        self.promotion_fields_remembered
            .fetch_add(remembered as u64, Ordering::Relaxed);
        remembered
    }

    /// Snapshot of this barrier's counters.
    pub fn stats(&self) -> ZStoreBarrierStats {
        ZStoreBarrierStats {
            stores_seen: self.stores_seen.load(Ordering::Relaxed),
            filtered_source_not_old: self.filtered_source_not_old.load(Ordering::Relaxed),
            filtered_value_not_young: self.filtered_value_not_young.load(Ordering::Relaxed),
            remembered_precise: self.remembered_precise.load(Ordering::Relaxed),
            remembered_coarse: self.remembered_coarse.load(Ordering::Relaxed),
            promotion_fields_remembered: self.promotion_fields_remembered.load(Ordering::Relaxed),
            promotions: self.promotions.load(Ordering::Relaxed),
        }
    }
}

impl std::fmt::Debug for ZStoreBarrier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZStoreBarrier")
            .field("table", &*self.table)
            .field("stats", &self.stats())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const YOUNG_BASE: u64 = 0x1000_0000;
    const YOUNG_SIZE: u64 = 1 << 20; // 1 MiB
    const OLD_BASE: u64 = 0x2000_0000;
    const OLD_PAGE: usize = 1 << 16; // 64 KiB pages, 4 of them
    const OLD_SIZE: u64 = (OLD_PAGE as u64) * 4;

    fn ctx() -> ZRangeGenerationContext {
        ZRangeGenerationContext::new(YOUNG_BASE, YOUNG_SIZE, OLD_BASE, OLD_SIZE, OLD_PAGE)
    }

    fn barrier_with_all_pages_registered() -> (ZStoreBarrier, ZRangeGenerationContext) {
        let table = Arc::new(ZRememberedSetTable::new());
        let context = ctx();
        // The context registers its own pages: its ids are ContextLocal, and a
        // hand-rolled `register_old_page` loop here would declare them to be
        // allocator ids. See ZPageIdNamespace.
        assert_eq!(context.register_pages(&table), 4);
        (ZStoreBarrier::new(table), context)
    }

    // -----------------------------------------------------------------
    // ZRememberedSet — set / query
    // -----------------------------------------------------------------

    /// The grain must be exactly one reference slot, and a word must cover
    /// exactly one card's worth of page. Both are load-bearing claims in the
    /// module header's cost analysis, so they are asserted rather than
    /// asserted-in-prose.
    #[test]
    fn granularity_matches_the_reference_slot_and_the_card_span() {
        assert_eq!(Z_REMSET_GRAIN_BYTES, 8);
        assert_eq!(Z_REMSET_GRAIN_BYTES, cratonvm_types::REF_FIELD_SIZE);
        assert_eq!(Z_REMSET_BYTES_PER_WORD, 512);
        assert_eq!(Z_REMSET_BYTES_PER_WORD, crate::card_table::CARD_SIZE);
    }

    /// The 16x figure from the module header, computed rather than asserted in
    /// prose: two bitmaps at one bit per 8 bytes vs. one byte per 512 bytes.
    #[test]
    fn memory_overhead_is_sixteen_times_the_card_table() {
        let page_size = 2 * 1024 * 1024; // ZGC small page
        let rs = ZRememberedSet::new(0, page_size);
        let bitmaps = 2 * rs.word_count() * std::mem::size_of::<AtomicU64>();
        assert_eq!(bitmaps, page_size / 32, "page_size/32 = 3.125%");
        assert_eq!(bitmaps, 64 * 1024, "64 KiB per 2 MiB page");

        let card_bytes = page_size / crate::card_table::CARD_SIZE;
        assert_eq!(card_bytes, 4 * 1024, "4 KiB per 2 MiB page");
        assert_eq!(bitmaps / card_bytes, 16);
    }

    #[test]
    fn set_and_query_round_trip_across_word_boundaries() {
        let page_size = 4096;
        let rs = ZRememberedSet::new(3, page_size);
        assert_eq!(rs.page_id(), 3);
        assert_eq!(rs.page_size(), page_size);
        // 4096 / 8 = 512 grains = 8 words.
        assert_eq!(rs.word_count(), 8);

        // Straddle the word-0/word-1 boundary: bit 63 is offset 504, bit 64 is
        // offset 512 (the first bit of word 1), bit 65 is 520. Every entry is
        // a DISTINCT grain (0, 1, 63, 64, 65, 256, 511) so the dedup below
        // cannot mask a collision.
        let offsets = [0usize, 8, 504, 512, 520, 2048, page_size - 8];
        for &off in &offsets {
            assert!(!rs.is_remembered(off), "clean before remember: {off}");
        }
        for &off in &offsets {
            assert!(rs.remember(off), "in-range remember must succeed: {off}");
        }
        for &off in &offsets {
            assert!(rs.is_remembered(off), "remembered after remember: {off}");
        }
        // Neighbouring grains must NOT have been set — one bit is one slot.
        assert!(!rs.is_remembered(496));
        assert!(!rs.is_remembered(528));

        let mut seen: Vec<usize> = Vec::new();
        rs.iterate(|off| seen.push(off));
        let mut expected: Vec<usize> = offsets.to_vec();
        expected.sort_unstable();
        expected.dedup();
        assert_eq!(seen, expected, "iterate yields ascending, deduplicated");
        assert_eq!(rs.bits_set(), expected.len());
    }

    #[test]
    fn offsets_within_one_grain_collapse_to_one_bit() {
        let rs = ZRememberedSet::new(0, 4096);
        // 8-byte grain: 16, 17 ... 23 are all the same slot.
        assert!(rs.remember(16));
        assert!(rs.remember(19));
        assert!(rs.remember(23));
        assert_eq!(rs.bits_set(), 1);
        let mut seen: Vec<usize> = Vec::new();
        rs.iterate(|off| seen.push(off));
        assert_eq!(seen, vec![16], "iterate reports the grain-aligned offset");
    }

    #[test]
    fn out_of_range_offset_is_rejected_not_silently_recorded() {
        let rs = ZRememberedSet::new(0, 4096);
        assert!(
            !rs.remember(4096),
            "exactly at the page end is out of range"
        );
        assert!(!rs.remember(999_999));
        assert_eq!(rs.bits_set(), 0);
        assert!(!rs.is_remembered(4096));
    }

    #[test]
    fn a_page_smaller_than_one_word_still_gets_a_word() {
        let rs = ZRememberedSet::new(0, 24);
        assert_eq!(rs.word_count(), 1);
        assert!(rs.remember(16));
        assert!(!rs.remember(24));
        assert_eq!(rs.bits_set(), 1);
    }

    // -----------------------------------------------------------------
    // ZRememberedSet — sparse iteration
    // -----------------------------------------------------------------

    /// The zero-word skip is the whole reason this structure can be scanned
    /// every young collection. Assert the SKIP COUNTER, not just the yielded
    /// offsets: a bit-by-bit loop would produce the same offsets while doing
    /// 64x the work, and only the counter can tell the two apart.
    #[test]
    fn sparse_iteration_skips_zero_words() {
        let page_size = 64 * 1024; // 8192 grains = 128 words
        let rs = ZRememberedSet::new(1, page_size);
        assert_eq!(rs.word_count(), 128);

        // One bit in the first word, one in the last: 126 words are zero.
        rs.remember(0);
        rs.remember(page_size - Z_REMSET_GRAIN_BYTES);

        let mut seen: Vec<usize> = Vec::new();
        rs.iterate(|off| seen.push(off));
        assert_eq!(seen, vec![0, page_size - Z_REMSET_GRAIN_BYTES]);

        let stats = rs.stats();
        assert_eq!(stats.words_scanned, 128, "one load per 512 bytes of page");
        assert_eq!(stats.words_skipped, 126, "126 zero words skipped whole");
        assert_eq!(stats.bits_set, 2);
        assert_eq!(stats.bits_newly_set, 2);
        assert_eq!(stats.grain_bytes, Z_REMSET_GRAIN_BYTES);

        // A second scan accumulates: these are lifetime counters.
        rs.iterate(|_| {});
        assert_eq!(rs.stats().words_scanned, 256);
        assert_eq!(rs.stats().words_skipped, 252);
    }

    #[test]
    fn a_dense_word_is_walked_once_per_set_bit_not_once_per_bit() {
        let rs = ZRememberedSet::new(0, 4096);
        // Fill word 0 completely: 64 bits, offsets 0..512 step 8.
        for i in 0..64usize {
            rs.remember(i * Z_REMSET_GRAIN_BYTES);
        }
        let mut count = 0usize;
        rs.iterate(|_| count += 1);
        assert_eq!(count, 64);
        let stats = rs.stats();
        assert_eq!(stats.words_scanned, 8);
        assert_eq!(stats.words_skipped, 7, "only word 0 is non-zero");
    }

    // -----------------------------------------------------------------
    // ZRememberedSet — concurrency
    // -----------------------------------------------------------------

    /// Every bit set by any thread must be observed. The interleaving
    /// (`grain = i * THREADS + t`) deliberately puts adjacent grains from
    /// DIFFERENT threads in the SAME `u64` word — that is the case a
    /// non-atomic read-modify-write silently loses bits on, and a lost bit
    /// here is a lost old->young edge, i.e. a freed live object.
    #[test]
    fn concurrent_remember_from_many_threads_loses_no_bit() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 512;
        let total = THREADS * PER_THREAD;
        let page_size = total * Z_REMSET_GRAIN_BYTES;

        let rs = Arc::new(ZRememberedSet::new(9, page_size));
        let start = Arc::new(std::sync::Barrier::new(THREADS));

        let mut handles = Vec::with_capacity(THREADS);
        for t in 0..THREADS {
            let rs = Arc::clone(&rs);
            let start = Arc::clone(&start);
            handles.push(std::thread::spawn(move || {
                // Maximise the overlap: everyone starts hammering at once.
                start.wait();
                for i in 0..PER_THREAD {
                    let grain = i * THREADS + t;
                    assert!(rs.remember(grain * Z_REMSET_GRAIN_BYTES));
                }
            }));
        }
        for h in handles {
            h.join().expect("mutator thread panicked");
        }

        assert_eq!(rs.bits_set(), total, "no bit may be lost to a race");
        assert_eq!(rs.stats().bits_newly_set, total as u64);
        for t in 0..THREADS {
            for i in 0..PER_THREAD {
                let grain = i * THREADS + t;
                assert!(
                    rs.is_remembered(grain * Z_REMSET_GRAIN_BYTES),
                    "thread {t} bit {i} (grain {grain}) was lost",
                );
            }
        }
        let mut seen = 0usize;
        rs.iterate(|_| seen += 1);
        assert_eq!(seen, total);
    }

    /// Two threads hammering the SAME bit is the idempotence half of the
    /// `fetch_or`-needs-no-CAS-loop argument: the bit ends up set exactly
    /// once, whichever order they land in.
    #[test]
    fn concurrent_remember_of_the_same_bit_is_idempotent() {
        const THREADS: usize = 8;
        let rs = Arc::new(ZRememberedSet::new(0, 4096));
        let start = Arc::new(std::sync::Barrier::new(THREADS));
        let mut handles = Vec::with_capacity(THREADS);
        for _ in 0..THREADS {
            let rs = Arc::clone(&rs);
            let start = Arc::clone(&start);
            handles.push(std::thread::spawn(move || {
                start.wait();
                for _ in 0..1000 {
                    rs.remember(64);
                }
            }));
        }
        for h in handles {
            h.join().expect("mutator thread panicked");
        }
        assert_eq!(rs.bits_set(), 1);
        assert_eq!(
            rs.stats().bits_newly_set,
            1,
            "exactly one clean->set transition across 8000 stores",
        );
    }

    // -----------------------------------------------------------------
    // ZRememberedSet — double buffering
    // -----------------------------------------------------------------

    #[test]
    fn swap_isolates_the_collector_snapshot_from_concurrent_dirtying() {
        let rs = ZRememberedSet::new(2, 4096);
        rs.remember(0);
        rs.remember(64);

        rs.swap();

        // Everything dirtied before the swap is in the collector's snapshot...
        assert_eq!(rs.snapshot_offsets(), vec![0, 64]);
        assert!(rs.is_remembered_in_snapshot(0));
        assert!(rs.is_remembered_in_snapshot(64));
        // ...and the buffer mutators now dirty starts empty.
        assert_eq!(rs.bits_set(), 0);
        assert!(!rs.is_remembered(0));

        // A store made while the collector scans must NOT contaminate the
        // snapshot — that is the entire point of double buffering.
        rs.remember(128);
        assert_eq!(rs.bits_set(), 1);
        assert!(rs.is_remembered(128));
        assert!(!rs.is_remembered_in_snapshot(128));
        assert_eq!(rs.snapshot_offsets(), vec![0, 64]);

        // Next cycle: roles flip again, and the consumed snapshot is wiped
        // rather than accumulating forever.
        rs.swap();
        assert_eq!(rs.snapshot_offsets(), vec![128]);
        assert_eq!(rs.bits_set(), 0);

        // Third swap: nothing was dirtied, so the snapshot is empty. If the
        // wipe were missing, {0, 64} would reappear here.
        rs.swap();
        assert!(rs.snapshot_offsets().is_empty());
        assert_eq!(rs.bits_set(), 0);
    }

    #[test]
    fn clear_snapshot_releases_the_scanned_buffer_only() {
        let rs = ZRememberedSet::new(0, 4096);
        rs.remember(0);
        rs.swap();
        rs.remember(256);
        assert_eq!(rs.snapshot_offsets(), vec![0]);
        assert_eq!(rs.bits_set(), 1);

        rs.clear_snapshot();
        assert!(rs.snapshot_offsets().is_empty());
        assert_eq!(rs.bits_set(), 1, "the mutators' buffer is untouched");
        assert!(rs.is_remembered(256));
    }

    #[test]
    fn clear_resets_both_buffers() {
        let rs = ZRememberedSet::new(0, 4096);
        rs.remember(0);
        rs.swap();
        rs.remember(256);
        assert_eq!(rs.bits_set(), 1);
        assert_eq!(rs.bits_set_in_snapshot(), 1);

        rs.clear();
        assert_eq!(rs.bits_set(), 0);
        assert_eq!(rs.bits_set_in_snapshot(), 0);
        assert!(rs.snapshot_offsets().is_empty());
        // Re-dirtying after a clear works normally.
        rs.remember(512);
        assert!(rs.is_remembered(512));
        assert_eq!(rs.bits_set(), 1);
    }

    // -----------------------------------------------------------------
    // ZRememberedSetTable
    // -----------------------------------------------------------------

    #[test]
    fn table_registers_looks_up_and_removes_pages() {
        let table = ZRememberedSetTable::new();
        assert!(table.is_empty());
        assert!(table.get(0).is_none());

        let a = table.register_old_page(0, OLD_PAGE);
        let b = table.register_old_page(1, OLD_PAGE);
        assert_eq!(table.len(), 2);
        assert_eq!(a.page_id(), 0);
        assert_eq!(b.page_id(), 1);

        // Idempotent: the same page id returns the SAME set, not a fresh one.
        a.remember(64);
        let a_again = table.register_old_page(0, OLD_PAGE);
        assert!(a_again.is_remembered(64));
        assert!(Arc::ptr_eq(&a, &a_again));
        assert_eq!(table.len(), 2);

        // `get` hands back a cloned Arc that stays valid after the guard is
        // gone (the whole point of the lock-discipline note on the type).
        let fetched = table.get(0).expect("page 0 is registered");
        assert!(fetched.is_remembered(64));

        let mut ids = table.page_ids();
        ids.sort_unstable();
        assert_eq!(ids, vec![0, 1]);

        let removed = table.remove(0).expect("page 0 was registered");
        assert_eq!(removed.page_id(), 0);
        assert!(table.get(0).is_none());
        assert_eq!(table.len(), 1);
        assert!(table.remove(0).is_none(), "removing twice is a no-op");
    }

    #[test]
    fn table_remember_coarsens_an_unregistered_page_rather_than_dropping_the_edge() {
        let table = ZRememberedSetTable::new();
        table.register_old_page(0, OLD_PAGE);

        assert_eq!(table.remember(0, 64), ZRememberOutcome::Precise);
        // Page 7 was never registered: the edge must survive as a coarsening.
        assert_eq!(table.remember(7, 64), ZRememberOutcome::Coarsened);
        // An in-range page with an out-of-range offset coarsens too.
        assert_eq!(table.remember(0, OLD_PAGE), ZRememberOutcome::Coarsened);

        let mut coarse = table.take_coarse_pages();
        coarse.sort_unstable();
        assert_eq!(coarse, vec![0, 7]);
        assert!(
            table.take_coarse_pages().is_empty(),
            "taking the coarse set clears it",
        );

        // Registering a coarsened page gives it precise storage again and
        // retires the coarsening.
        table.remember(7, 64);
        table.register_old_page(7, OLD_PAGE);
        assert!(table.take_coarse_pages().is_empty());
        assert_eq!(table.remember(7, 64), ZRememberOutcome::Precise);
    }

    /// FINDING A (2026-08-07). Two page ids that are congruent modulo `2^32`
    /// must be two different pages.
    ///
    /// While the key was a `u32` this could not even be *expressed*: the caller
    /// narrowed, `1 << 32` became `0`, and the remembered set for the young
    /// collection's roots was read out of a completely unrelated page. Nothing
    /// errored, nothing was counted, and the first symptom was a use-after-free
    /// on an object the collector had every right to think was garbage.
    ///
    /// `ZPageAllocator`'s `next_page_id` starts at 1, is bumped once per page
    /// allocation and is never recycled downward (`page.rs:973`, `:1156`), so
    /// this is not a hypothetical id — it is the id a long-running VM reaches
    /// on its 4-billionth page.
    #[test]
    fn page_ids_congruent_modulo_two_to_the_thirty_two_are_different_pages() {
        const LOW: u64 = 7;
        const HIGH: u64 = (1u64 << 32) + 7; // truncates to LOW in a u32 key

        let table = ZRememberedSetTable::new();
        let low = table.register_old_page(LOW, OLD_PAGE);
        let high = table.register_old_page(HIGH, OLD_PAGE);

        assert_eq!(table.len(), 2, "the two ids must not collapse into one key");
        assert!(
            !Arc::ptr_eq(&low, &high),
            "the high id aliased onto the low one's bitmap",
        );
        assert_eq!(low.page_id(), LOW);
        assert_eq!(high.page_id(), HIGH);

        // Dirty a slot in each, at different offsets, and prove neither is
        // visible through the other.
        assert_eq!(table.remember(LOW, 64), ZRememberOutcome::Precise);
        assert_eq!(table.remember(HIGH, 128), ZRememberOutcome::Precise);
        assert!(low.is_remembered(64) && !low.is_remembered(128));
        assert!(high.is_remembered(128) && !high.is_remembered(64));
        assert_eq!(table.total_bits_set(), 2);

        // ... and removing one leaves the other alone.
        let removed = table.remove(HIGH).expect("the high page was registered");
        assert_eq!(removed.page_id(), HIGH);
        assert!(table.get(LOW).is_some(), "removing 2^32+7 dropped 7");
        assert_eq!(table.len(), 1);

        let mut ids = table.page_ids();
        ids.sort_unstable();
        assert_eq!(ids, vec![LOW]);
    }

    /// FINDING A, at the context seam: a page index past `u32::MAX` must be
    /// *paged*, not refused.
    ///
    /// The old implementation returned `None` above `u32::MAX`, which made the
    /// table coarsen — safe, but it turned every store into a whole-page scan
    /// from that point on, i.e. it re-created exactly the full-heap cost this
    /// module exists to escape. With a `u64` id there is nothing to refuse.
    #[test]
    fn range_context_pages_an_index_that_does_not_fit_a_u32() {
        // 8-byte pages so a modest address reaches a huge page index.
        let c = ZRangeGenerationContext::new(0, 0, 0, 1u64 << 36, 8);
        let past_u32 = (u64::from(u32::MAX) + 1) * 8; // page index 2^32 exactly

        assert_eq!(
            c.page_of(past_u32),
            Some((1u64 << 32, 0)),
            "a page index past u32::MAX must be reported, not refused as \
             unknown and not truncated to 0",
        );
        assert_eq!(c.page_of(past_u32 + 4), Some((1u64 << 32, 4)));
        assert_eq!(c.page_base(1u64 << 32), past_u32);

        // The neighbouring index and its 32-bit truncation are distinct pages.
        assert_eq!(c.page_of(0), Some((0u64, 0)));
        assert_ne!(c.page_of(0), c.page_of(past_u32));

        // End to end through the table: the high page is remembered PRECISELY.
        // Registered in the context's own namespace — `c` has 2^33 pages, so
        // `register_pages` is exactly the call NOT to make here (see its cost
        // note); this registers the single page under test by hand, in the right
        // namespace.
        let table = ZRememberedSetTable::new();
        let (page_id, offset) = c.page_of(past_u32).expect("in range");
        table.register_old_page_in(ZPageIdNamespace::ContextLocal, page_id, 8);
        assert_eq!(table.remember(page_id, offset), ZRememberOutcome::Precise);
        assert!(
            table.take_coarse_pages().is_empty(),
            "a high page id must not coarsen",
        );
    }

    #[test]
    fn table_swap_all_flips_every_page() {
        let table = ZRememberedSetTable::new();
        for page_id in 0..4u64 {
            table.register_old_page(page_id, OLD_PAGE);
            table.remember(page_id, 64 * (page_id as usize + 1));
        }
        assert_eq!(table.total_bits_set(), 4);
        assert_eq!(table.total_bits_set_in_snapshot(), 0);

        table.swap_all();

        assert_eq!(table.total_bits_set(), 0, "mutators resume into clean maps");
        assert_eq!(table.total_bits_set_in_snapshot(), 4);
        for page_id in 0..4u64 {
            let set = table.get(page_id).expect("registered");
            assert_eq!(
                set.snapshot_offsets(),
                vec![64 * (page_id as usize + 1)],
                "page {page_id}",
            );
        }

        table.clear_all();
        assert_eq!(table.total_bits_set(), 0);
        assert_eq!(table.total_bits_set_in_snapshot(), 0);
    }

    #[test]
    fn table_retained_bytes_is_two_bitmaps_per_page() {
        let table = ZRememberedSetTable::new();
        assert_eq!(table.total_retained_bytes(), 0);
        table.register_old_page(0, OLD_PAGE);
        table.register_old_page(1, OLD_PAGE);
        let one = ZRememberedSet::new(0, OLD_PAGE).retained_bytes();
        assert_eq!(table.total_retained_bytes(), 2 * one);
        // The bitmaps dominate the struct: 64 KiB page => 2 KiB of bitmaps.
        assert!(one >= 2 * (OLD_PAGE / 64));
    }

    #[test]
    fn table_is_usable_concurrently() {
        const THREADS: usize = 8;
        let table = Arc::new(ZRememberedSetTable::new());
        for page_id in 0..4u64 {
            table.register_old_page(page_id, OLD_PAGE);
        }
        let start = Arc::new(std::sync::Barrier::new(THREADS));
        let mut handles = Vec::with_capacity(THREADS);
        for t in 0..THREADS {
            let table = Arc::clone(&table);
            let start = Arc::clone(&start);
            handles.push(std::thread::spawn(move || {
                start.wait();
                for i in 0..256usize {
                    let page_id = ((i + t) % 4) as u64;
                    let offset = (i * THREADS + t) * Z_REMSET_GRAIN_BYTES;
                    assert_eq!(table.remember(page_id, offset), ZRememberOutcome::Precise,);
                }
            }));
        }
        for h in handles {
            h.join().expect("thread panicked");
        }
        assert_eq!(table.total_bits_set(), THREADS * 256);
        assert!(table.take_coarse_pages().is_empty());
    }

    // -----------------------------------------------------------------
    // ZRangeGenerationContext
    // -----------------------------------------------------------------

    #[test]
    fn range_context_classifies_and_pages_addresses() {
        let c = ctx();
        assert!(c.is_young(YOUNG_BASE));
        assert!(c.is_young(YOUNG_BASE + YOUNG_SIZE - 1));
        assert!(!c.is_young(YOUNG_BASE + YOUNG_SIZE));
        assert!(!c.is_young(OLD_BASE));

        assert!(c.is_old(OLD_BASE));
        assert!(c.is_old(OLD_BASE + OLD_SIZE - 1));
        assert!(!c.is_old(OLD_BASE + OLD_SIZE));
        assert!(!c.is_old(YOUNG_BASE));

        assert_eq!(c.old_page_count(), 4);
        assert_eq!(c.page_of(OLD_BASE), Some((0, 0)));
        assert_eq!(c.page_of(OLD_BASE + 64), Some((0, 64)));
        assert_eq!(c.page_of(OLD_BASE + OLD_PAGE as u64 + 8), Some((1, 8)),);
        assert_eq!(c.page_base(2), OLD_BASE + 2 * OLD_PAGE as u64);
        assert_eq!(c.page_of(YOUNG_BASE), None, "young pages have no rset");
        assert_eq!(c.page_of(OLD_BASE + OLD_SIZE), None);
    }

    // -----------------------------------------------------------------
    // FINDING N7 (2026-08-07) — page-id namespaces
    // -----------------------------------------------------------------

    /// A `ZGenerationContext` that answers like every real-heap implementation
    /// in the tree: ids straight out of `ZPageAllocator`, so it takes the
    /// trait's default `page_id_namespace`. Stands in for
    /// `adapters::ZHeapGenerationContext`, which this module cannot import.
    struct AllocatorIdContext {
        /// `page_id -> base`, in allocation order, ids from 1 as the real
        /// allocator hands them out (`page.rs`: `next_page_id` starts at 1).
        pages: Vec<(u64, u64)>,
    }

    impl AllocatorIdContext {
        /// Four pages covering the same addresses as [`ctx`], but numbered the
        /// way the allocator numbers them: 1, 2, 3, 4.
        fn over_the_same_old_range() -> Self {
            AllocatorIdContext {
                pages: (0..4u64)
                    .map(|i| (i + 1, OLD_BASE + i * OLD_PAGE as u64))
                    .collect(),
            }
        }

        fn register_all(&self, table: &ZRememberedSetTable) {
            for &(id, _) in &self.pages {
                table.register_old_page(id, OLD_PAGE);
            }
        }
    }

    impl ZGenerationContext for AllocatorIdContext {
        fn is_old(&self, addr: u64) -> bool {
            addr >= OLD_BASE && addr < OLD_BASE + OLD_SIZE
        }
        fn is_young(&self, addr: u64) -> bool {
            addr >= YOUNG_BASE && addr < YOUNG_BASE + YOUNG_SIZE
        }
        fn page_of(&self, addr: u64) -> Option<(u64, usize)> {
            self.pages
                .iter()
                .find(|&&(_, base)| addr >= base && addr < base + OLD_PAGE as u64)
                .map(|&(id, base)| (id, (addr - base) as usize))
        }
        // `page_id_namespace` deliberately NOT overridden: the default must be
        // Allocator, so that every existing real-heap impl in the tree is
        // correct without being edited.
    }

    /// PINS the N7 verdict: the two numberings are different, they are declared
    /// to be different, and the declaration is machine-readable.
    ///
    /// `ZRangeGenerationContext` derives a dense index from 0 by address
    /// arithmetic; `ZPageReal::id` counts allocations from 1. Over the *same*
    /// four pages the two disagree on every one of them — and disagree by
    /// producing other *valid* ids, which is what makes a mismatch alias
    /// silently instead of coarsening loudly.
    #[test]
    fn the_range_contexts_ids_are_its_own_and_it_says_so() {
        let range = ctx();
        let allocator = AllocatorIdContext::over_the_same_old_range();

        assert_eq!(range.page_id_namespace(), ZPageIdNamespace::ContextLocal);
        assert_eq!(
            allocator.page_id_namespace(),
            ZPageIdNamespace::Allocator,
            "an impl that returns ZPageReal::id must get the DEFAULT — that is \
             why page_id_namespace is a provided method",
        );

        for i in 0..4u64 {
            let addr = OLD_BASE + i * OLD_PAGE as u64 + 64;
            let (range_id, range_off) = range.page_of(addr).expect("in the old range");
            let (alloc_id, alloc_off) = allocator.page_of(addr).expect("in the old range");
            assert_eq!(
                range_off, alloc_off,
                "the OFFSET half agrees; only the id differs"
            );
            assert_ne!(
                range_id, alloc_id,
                "page {i}: dense index {range_id} vs allocator id {alloc_id}",
            );
            // And the disagreement is an alias, not a miss: the id the range
            // context produces for page `i` is a page the allocator DID hand
            // out (for i >= 1), so a table keyed by allocator ids will happily
            // resolve it — to the wrong page.
            if i >= 1 {
                assert!(
                    allocator.pages.iter().any(|&(id, _)| id == range_id),
                    "index {range_id} collides with a real allocator id",
                );
            }
        }
    }

    /// PINS the check itself, in BOTH build profiles.
    ///
    /// `on_reference_store` only `debug_assert!`s the pairing, which pins
    /// nothing in a release test run. `context_namespace_matches` is the same
    /// predicate as a plain function, so this test holds either way.
    #[test]
    fn the_barrier_reports_a_table_and_context_that_disagree() {
        // Empty table: compatible with anything, because it has no key yet.
        let empty = Arc::new(ZRememberedSetTable::new());
        assert_eq!(empty.page_id_namespace(), None);
        let barrier = ZStoreBarrier::new(Arc::clone(&empty));
        assert!(barrier.context_namespace_matches(&ctx()));
        assert!(barrier.context_namespace_matches(&AllocatorIdContext::over_the_same_old_range()));

        // Table registered by the allocator, driven by the range context: the
        // N7 pairing.
        let allocator_keyed = Arc::new(ZRememberedSetTable::new());
        AllocatorIdContext::over_the_same_old_range().register_all(&allocator_keyed);
        assert_eq!(
            allocator_keyed.page_id_namespace(),
            Some(ZPageIdNamespace::Allocator),
        );
        let barrier = ZStoreBarrier::new(allocator_keyed);
        assert!(
            !barrier.context_namespace_matches(&ctx()),
            "an allocator-keyed table driven by a context-local context is the \
             lost-edge configuration and must be reported",
        );
        assert!(barrier.context_namespace_matches(&AllocatorIdContext::over_the_same_old_range()));

        // ... and the correctly paired table.
        let range_keyed = Arc::new(ZRememberedSetTable::new());
        let range = ctx();
        range.register_pages(&range_keyed);
        assert_eq!(
            range_keyed.page_id_namespace(),
            Some(ZPageIdNamespace::ContextLocal),
        );
        let barrier = ZStoreBarrier::new(range_keyed);
        assert!(barrier.context_namespace_matches(&range));
        assert!(
            !barrier.context_namespace_matches(&AllocatorIdContext::over_the_same_old_range()),
            "the mismatch is symmetric",
        );
    }

    /// PINS what the mismatch actually costs, so nobody has to take the prose on
    /// trust: the edge is not dropped and not coarsened — it lands on **another
    /// live page's bitmap**.
    ///
    /// Driven through `table.remember` rather than `on_reference_store`, because
    /// the barrier's `debug_assert!` would (correctly) refuse to let this
    /// configuration run in a debug build. That assertion is the fix; this test
    /// is what it is protecting against.
    #[test]
    fn a_mismatched_namespace_aliases_onto_a_live_page_rather_than_coarsening() {
        let allocator = AllocatorIdContext::over_the_same_old_range();
        let table = ZRememberedSetTable::new();
        allocator.register_all(&table);

        // The mutator stores into a field on the SECOND old page. The range
        // context names it page 1; the allocator names it page 2.
        let field = OLD_BASE + OLD_PAGE as u64 + 512;
        let (wrong_id, offset) = ctx().page_of(field).expect("in the old range");
        let (right_id, right_offset) = allocator.page_of(field).expect("in the old range");
        assert_eq!((wrong_id, offset), (1, 512));
        assert_eq!((right_id, right_offset), (2, 512));

        assert_eq!(
            table.remember(wrong_id, offset),
            ZRememberOutcome::Precise,
            "the wrong id is a REGISTERED page, so the edge is recorded \
             precisely — into the wrong bitmap. No coarsening, no warning.",
        );
        assert!(
            table.take_coarse_pages().is_empty(),
            "nothing was coarsened: this is why the defect is silent",
        );

        let victim = table.get(wrong_id).expect("allocator page 1 is registered");
        let real_page = table.get(right_id).expect("allocator page 2 is registered");
        assert!(
            victim.is_remembered(512),
            "page 1 got a bit it never earned"
        );
        assert!(
            !real_page.is_remembered(512),
            "and page 2 — the page that actually holds the old->young edge — \
             has NOTHING. Its young referent is freed while live.",
        );
    }

    /// PINS the pairing constructor: `register_pages` registers exactly the ids
    /// `page_of` will later produce, in the namespace it declares.
    #[test]
    fn register_pages_registers_exactly_the_ids_the_context_produces() {
        let table = ZRememberedSetTable::new();
        let c = ctx();
        assert_eq!(c.register_pages(&table), 4);
        assert_eq!(table.len(), 4);

        let mut ids = table.page_ids();
        ids.sort_unstable();
        assert_eq!(ids, vec![0, 1, 2, 3]);
        assert_eq!(
            table.page_id_namespace(),
            Some(ZPageIdNamespace::ContextLocal),
        );

        // Every address in the old range resolves to a registered set.
        for i in 0..4u64 {
            let (id, off) = c
                .page_of(OLD_BASE + i * OLD_PAGE as u64 + 8)
                .expect("in the old range");
            assert_eq!(table.remember(id, off), ZRememberOutcome::Precise);
        }
        assert!(table.take_coarse_pages().is_empty());

        // Idempotent, and it does not re-latch.
        assert_eq!(c.register_pages(&table), 4);
        assert_eq!(table.len(), 4);
        assert_eq!(table.total_bits_set(), 4, "re-registering kept the bitmaps");
    }

    /// The latch is set by the first registration and read back verbatim.
    ///
    /// Deliberately does NOT assert the conflict *panic*: that is a
    /// `debug_assert!`, so a `#[should_panic]` here would pass in debug and fail
    /// in release. The observable half — what the table says it is keyed by —
    /// holds in both.
    #[test]
    fn the_table_latches_the_namespace_of_its_first_registration() {
        let fresh = ZRememberedSetTable::new();
        assert_eq!(fresh.page_id_namespace(), None, "nothing registered yet");

        let allocator_keyed = ZRememberedSetTable::new();
        allocator_keyed.register_old_page(7, OLD_PAGE);
        assert_eq!(
            allocator_keyed.page_id_namespace(),
            Some(ZPageIdNamespace::Allocator),
            "the plain register_old_page DECLARES the allocator namespace — \
             which is what all of its production call sites mean",
        );
        // A second, same-namespace registration is not a conflict.
        allocator_keyed.register_old_page(8, OLD_PAGE);
        assert_eq!(
            allocator_keyed.page_id_namespace(),
            Some(ZPageIdNamespace::Allocator),
        );

        let context_keyed = ZRememberedSetTable::new();
        context_keyed.register_old_page_in(ZPageIdNamespace::ContextLocal, 0, OLD_PAGE);
        assert_eq!(
            context_keyed.page_id_namespace(),
            Some(ZPageIdNamespace::ContextLocal),
        );

        // Removing every page does not un-latch: the table has been used, and
        // the ids that will be registered next must still agree with the ones
        // that were.
        assert!(context_keyed.remove(0).is_some());
        assert!(context_keyed.is_empty());
        assert_eq!(
            context_keyed.page_id_namespace(),
            Some(ZPageIdNamespace::ContextLocal),
        );
    }

    // -----------------------------------------------------------------
    // ZStoreBarrier — generation gating
    // -----------------------------------------------------------------

    #[test]
    fn store_barrier_remembers_only_old_to_young() {
        let (barrier, c) = barrier_with_all_pages_registered();

        let old_field = OLD_BASE + 512; // page 0, offset 512
        let young_value = YOUNG_BASE + 64;
        let old_value = OLD_BASE + 4096;
        let young_field = YOUNG_BASE + 128;

        // old -> young: the only case that creates an edge a young collection
        // could miss.
        assert_eq!(
            barrier.on_reference_store(&c, old_field, young_value),
            ZStoreBarrierOutcome::Precise,
        );
        let page0 = barrier.table().get(0).expect("page 0 registered");
        assert!(page0.is_remembered(512));
        assert_eq!(page0.bits_set(), 1);

        // old -> old: found by the old-generation trace, NOT the remembered
        // set. Remembering it would be pure scan cost for no safety.
        assert_eq!(
            barrier.on_reference_store(&c, old_field + 8, old_value),
            ZStoreBarrierOutcome::Filtered,
        );
        assert!(!page0.is_remembered(520));

        // young -> young: the young collection traces the whole young
        // generation, so there is nothing to remember.
        assert_eq!(
            barrier.on_reference_store(&c, young_field, young_value),
            ZStoreBarrierOutcome::Filtered,
        );

        // young -> old: an edge in the wrong direction entirely.
        assert_eq!(
            barrier.on_reference_store(&c, young_field, old_value),
            ZStoreBarrierOutcome::Filtered,
        );

        // null store into an old field: no edge.
        assert_eq!(
            barrier.on_reference_store(&c, old_field + 16, 0),
            ZStoreBarrierOutcome::Filtered,
        );

        assert_eq!(page0.bits_set(), 1, "exactly one edge was remembered");
        assert_eq!(barrier.table().total_bits_set(), 1);

        let stats = barrier.stats();
        assert_eq!(stats.stores_seen, 5);
        assert_eq!(stats.remembered_precise, 1);
        assert_eq!(stats.remembered_coarse, 0);
        assert_eq!(stats.filtered_source_not_old, 2, "the two young sources");
        assert_eq!(
            stats.filtered_value_not_young, 2,
            "the old->old store and the null store",
        );
    }

    /// FINDING B (2026-08-07), pinned as behaviour rather than left as prose.
    ///
    /// This trait's `u64`s are machine addresses. Hand it the *heap offset* the
    /// load barrier returns — the other domain in this subsystem — and the edge
    /// is **silently filtered**: no panic, no warning, no coarsening, and a
    /// remembered set that stays empty while genuine old→young edges accumulate.
    ///
    /// The test states both halves so the failure has a signature: the offset
    /// form filters and bumps `filtered_value_not_young`, the address form is
    /// `Precise`. If a caller ever wires the barrier to the offset domain, the
    /// only trace in production is `filtered_value_not_young == stores_seen`.
    #[test]
    fn a_heap_offset_passed_where_a_machine_address_belongs_is_silently_filtered() {
        let (barrier, c) = barrier_with_all_pages_registered();

        // Pretend the ZGC heap base is YOUNG_BASE, so this young object's
        // vaddr-style heap offset is 0x40 — a number that is not in any
        // generation's address range and is therefore "not young".
        let young_address = YOUNG_BASE + 0x40;
        let young_as_offset = 0x40u64;
        let old_field = OLD_BASE + 256;

        assert_eq!(
            barrier.on_reference_store(&c, old_field, young_as_offset),
            ZStoreBarrierOutcome::Filtered,
            "an offset-domain referent must not be mistaken for a young address",
        );
        assert_eq!(
            barrier.table().total_bits_set(),
            0,
            "the edge was dropped — this is the silent failure the domain note \
             on ZGenerationContext describes",
        );

        // The same edge, expressed in this trait's domain, is recorded.
        assert_eq!(
            barrier.on_reference_store(&c, old_field, young_address),
            ZStoreBarrierOutcome::Precise,
        );
        assert_eq!(barrier.table().total_bits_set(), 1);

        let stats = barrier.stats();
        assert_eq!(stats.stores_seen, 2);
        assert_eq!(
            stats.filtered_value_not_young, 1,
            "the ONLY counter that moves when the domains are crossed",
        );
        assert_eq!(stats.remembered_precise, 1);
        assert_eq!(stats.remembered_coarse, 0);
    }

    #[test]
    fn store_barrier_records_the_field_slot_not_the_object_base() {
        let (barrier, c) = barrier_with_all_pages_registered();
        let object = OLD_BASE + 2048;
        // Field 3 of a compact object: HEADER_SIZE + 3 * REF_FIELD_SIZE.
        let field_offset = cratonvm_types::HEADER_SIZE + 3 * Z_REMSET_GRAIN_BYTES;

        assert_eq!(
            barrier.on_reference_store_in_object(&c, object, field_offset, YOUNG_BASE),
            ZStoreBarrierOutcome::Precise,
        );

        let page0 = barrier.table().get(0).expect("page 0 registered");
        let expected = 2048 + field_offset;
        assert!(page0.is_remembered(expected));
        assert!(
            !page0.is_remembered(2048),
            "the object BASE must not be marked — that is card-table \
             behaviour and it is what the precision buys us over",
        );
        assert_eq!(page0.snapshot_offsets().len(), 0);
        let mut seen: Vec<usize> = Vec::new();
        page0.iterate(|off| seen.push(off));
        assert_eq!(seen, vec![expected]);
    }

    #[test]
    fn store_barrier_targets_the_right_page() {
        let (barrier, c) = barrier_with_all_pages_registered();
        // One edge per page, at a distinct in-page offset.
        for page_id in 0..4u64 {
            let addr = OLD_BASE + page_id * OLD_PAGE as u64 + 128;
            assert_eq!(
                barrier.on_reference_store(&c, addr, YOUNG_BASE + 8),
                ZStoreBarrierOutcome::Precise,
            );
        }
        for page_id in 0..4u64 {
            let set = barrier.table().get(page_id).expect("registered");
            assert_eq!(
                set.bits_set(),
                1,
                "page {page_id} holds exactly its own edge"
            );
            assert!(set.is_remembered(128));
        }
    }

    #[test]
    fn store_barrier_coarsens_an_unregistered_old_page() {
        let table = Arc::new(ZRememberedSetTable::new());
        // Deliberately register only page 0 — in the context's namespace, since
        // the context below is what will key the table.
        table.register_old_page_in(ZPageIdNamespace::ContextLocal, 0, OLD_PAGE);
        let barrier = ZStoreBarrier::new(table);
        let c = ctx();

        let addr_on_page_2 = OLD_BASE + 2 * OLD_PAGE as u64 + 64;
        assert_eq!(
            barrier.on_reference_store(&c, addr_on_page_2, YOUNG_BASE),
            ZStoreBarrierOutcome::Coarsened,
            "an unregistered page must coarsen, never silently drop the edge",
        );
        assert_eq!(barrier.table().take_coarse_pages(), vec![2]);
        assert_eq!(barrier.stats().remembered_coarse, 1);
    }

    // -----------------------------------------------------------------
    // Promotion
    // -----------------------------------------------------------------

    #[test]
    fn promotion_remembers_every_still_young_outgoing_reference() {
        let (barrier, c) = barrier_with_all_pages_registered();

        // A young object with four reference fields is promoted to page 1 at
        // in-page offset 1024. Two fields still point at young objects, one at
        // an old object, one is null.
        let promoted = OLD_BASE + OLD_PAGE as u64 + 1024;
        let h = cratonvm_types::HEADER_SIZE;
        let outgoing: Vec<(usize, u64)> = vec![
            (h, YOUNG_BASE + 0x40),       // young  -> remember
            (h + 8, OLD_BASE + 0x20),     // old    -> skip
            (h + 16, 0),                  // null   -> skip
            (h + 24, YOUNG_BASE + 0x900), // young  -> remember
        ];

        assert_eq!(barrier.on_promote(&c, promoted, &outgoing), 2);

        let page1 = barrier.table().get(1).expect("page 1 registered");
        assert!(page1.is_remembered(1024 + h));
        assert!(page1.is_remembered(1024 + h + 24));
        assert!(!page1.is_remembered(1024 + h + 8), "old referent, no edge");
        assert!(
            !page1.is_remembered(1024 + h + 16),
            "null referent, no edge"
        );
        assert_eq!(page1.bits_set(), 2);

        let stats = barrier.stats();
        assert_eq!(stats.promotions, 1);
        assert_eq!(stats.promotion_fields_remembered, 2);
        assert_eq!(stats.remembered_precise, 2);

        // The whole point: those edges were invisible to the store barrier
        // while the object was young (young->young stores are filtered), so
        // `on_promote` is the ONLY thing that records them.
        let young_field = YOUNG_BASE + 0x100;
        assert_eq!(
            barrier.on_reference_store(&c, young_field, YOUNG_BASE + 0x40),
            ZStoreBarrierOutcome::Filtered,
        );
    }

    #[test]
    fn promotion_into_an_unmappable_address_reports_nothing_remembered() {
        let (barrier, c) = barrier_with_all_pages_registered();
        // An address outside the old range: constraint 1 on `on_promote` was
        // violated (the caller passed the pre-copy source address).
        let bogus = YOUNG_BASE + 512;
        assert_eq!(barrier.on_promote(&c, bogus, &[(0, YOUNG_BASE)]), 0,);
        assert_eq!(barrier.stats().promotion_fields_remembered, 0);
        assert_eq!(barrier.table().total_bits_set(), 0);
    }

    #[test]
    fn promoted_edges_survive_into_the_collectors_snapshot() {
        let (barrier, c) = barrier_with_all_pages_registered();
        let promoted = OLD_BASE + 256;
        barrier.on_promote(&c, promoted, &[(0, YOUNG_BASE + 8)]);

        // Young-mark start.
        barrier.table().swap_all();

        let page0 = barrier.table().get(0).expect("registered");
        assert_eq!(
            page0.snapshot_offsets(),
            vec![256],
            "the promotion's edge is a root for THIS young collection",
        );
    }
}
