// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
//! (`zgc-real-fullsuite-regression-RETIRED-20260807.md`).
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
//! versus [`cratonvm_gc_core::card_table::CardTable`], which is one `AtomicU8` per
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
//! have**. It is one contiguous [`Arena`](cratonvm_gc_core::arena::Arena), non-moving, and
//! stop-the-world. For a *first* generational landing — the one that has to fix
//! the 35 hanging classes — reusing [`cratonvm_gc_core::card_table::CardTable`] over an
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
//!
//! # Persistence contract — the swap erases, so the scan must renew
//!
//! A bit protects its edge for **one** young cycle after the store that set it.
//! [`ZRememberedSet::swap`] wipes the buffer the previous cycle scanned, and a
//! long-lived old→young reference is stored once and never again, so nothing
//! renews the bit unless the collector does. The renewal is
//! [`ZRememberedSet::iterate_snapshot_retaining`] (or
//! [`ZRememberedSetTable::iterate_slot_addresses`] with `snapshot = true`),
//! which re-sets the bit for every slot the caller reports as still young. A
//! young cycle that scans with the plain [`ZRememberedSet::iterate_snapshot`]
//! is sound on the cycle after each store and frees live objects on every
//! later one. The two-cycle trace is on `iterate_snapshot_retaining`.
//!
//! # Page ids are reused, so a page leaving old must be de-registered
//!
//! `ZPageAllocator` caches freed Small and Medium pages and hands them back
//! carrying their original id, so `page_id` is not a unique name for a
//! generation of objects. When a page leaves the old generation the owner calls
//! [`ZRememberedSetTable::remove`], or
//! [`ZRememberedSetTable::recycle_old_page`] to do both at once. See the ⚠
//! block on [`ZRememberedSet::page_id`].

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
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
/// [`cratonvm_gc_core::card_table::CardTable`]'s byte-map — at the cost of the exact
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
/// Deliberately equal to [`cratonvm_gc_core::card_table::CARD_SIZE`]. That equality is
/// what makes a zero-word skip cost the collector exactly what a clean-card
/// byte test costs it — see the module header's scan-cost note.
pub const Z_REMSET_BYTES_PER_WORD: usize = Z_REMSET_GRAIN_BYTES * Z_REMSET_BITS_PER_WORD;

/// `CRATONVM_ZGC_REMSET_SUMMARY` -- do the remembered set's per-cycle passes
/// (swap wipe, snapshot scan, snapshot clear, popcount) visit only the words
/// [`ZRememberedSet`]'s per-buffer summary names? **Default ON** (zr z1/gn);
/// `0`/`off`/`false`/`no` restores the full per-word walks. The summary itself
/// is maintained either way (one relaxed load per card, plus a `fetch_or` on
/// the first card in a 512-byte word), so the switch is an A/B of the scan
/// cost on one binary. Only a generational ZGC run writes a remembered set.
///
/// zr z3/lt: read UNCACHED, once per [`ZRememberedSetTable`] (per heap), and
/// copied into each [`ZRememberedSet`] the table creates; it used to be a
/// process `OnceLock`, so two heaps could not run different arms.
pub fn zgc_remset_summary_enabled() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

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
    ///
    /// **This is not the store barrier's own count.**
    /// [`ZRememberedSet::iterate_snapshot_retaining`] renews a surviving edge
    /// by calling `remember` into the (just-wiped) current buffer, so every
    /// carried-forward bit lands here too. Read it beside [`Self::bits_retained`]:
    /// `bits_newly_set - bits_retained` is what the mutators dirtied, and
    /// `bits_newly_set` alone over-reports barrier traffic by one per surviving
    /// edge per young cycle — a lifetime-weighted figure, exactly the artefact
    /// `census::ZSlotCensus::last_walk` exists to sit beside.
    pub bits_newly_set: u64,
    /// Lifetime count of snapshot bits carried forward into the mutators'
    /// buffer by [`ZRememberedSet::iterate_snapshot_retaining`] — i.e. edges
    /// that were still old→young when the collector looked, and therefore had
    /// to survive the next [`ZRememberedSet::swap`].
    ///
    /// **`bits_retained == 0` on a run with more than one young cycle and a
    /// stable old→young edge is the inert state**, and it is the counter that
    /// distinguishes "the scan re-remembered nothing because nothing was still
    /// young" from "nobody is calling the retaining scan at all". See the
    /// persistence invariant on [`ZRememberedSet::swap`].
    pub bits_retained: u64,
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
    /// `ZPageAllocator`'s `next_page_id` counter, which starts at 1 and is
    /// incremented on every `alloc_page`. `forwarding::PageCandidate::page_id`,
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
    ///
    /// # ⚠ An id IS reused — the earlier text here was wrong (2026-09-20)
    ///
    /// This note used to claim the id is "**never recycled downward** when a
    /// page is freed (`page.rs`'s `free_page` returns the memory to the granule
    /// pool and drops the id)". That is true of **Large** pages only.
    /// `ZPageAllocator::free_page_locked` (`gc/src/zgc/page.rs:1551`) takes the
    /// Large branch to `release_granules_locked`, which does drop the id — and
    /// for **Small and Medium** it instead pushes the *same*
    /// `Arc<ZPageReal>` onto a per-class cache (`page.rs:1573-1580`), from
    /// which `alloc_page` pops it again **carrying its original id**. The id
    /// counter never goes backwards, but the *live* id set is not monotonic:
    /// the same integer names a different generation of objects after a free /
    /// re-allocate round trip.
    ///
    /// The consequence for this module is a stale bitmap, not an aliased one.
    /// A remembered set registered for an old page that is later freed and
    /// recycled still holds that page's bits, and
    /// [`ZRememberedSetTable::register_old_page`] is idempotent — so if the
    /// recycled page becomes old again it gets the *old page's* bitmap back.
    /// Those bits name slot offsets of objects that no longer exist. They
    /// cannot lose an edge (the set only ever over-approximates), but every one
    /// of them becomes a wild read for whatever consumes the scan, so the
    /// discipline is:
    ///
    /// > **When a page leaves the old generation — freed, demoted, or
    /// > relocated into a fresh id — the owner must call
    /// > [`ZRememberedSetTable::remove`]**, or
    /// > [`ZRememberedSetTable::recycle_old_page`] if it is being re-registered
    /// > in the same breath. Nothing in this module can detect the transition
    /// > on its own: it holds a `u64` and a `usize` and, by design, no handle on
    /// > the page.
    page_id: u64,
    /// Base machine address of the old page, or `0` for "not supplied".
    ///
    /// # Why the set needs a base at all, when the bitmap is page-relative
    ///
    /// Because a page-relative offset is not a root. Something has to add the
    /// page base before the edge can be read, and while this module held only
    /// `(page_id, page_size)` that something was an **out-of-band map** owned by
    /// the consumer — `adapters::ZTableRememberedSetView::page_bases`, whose own
    /// doc records that a missing entry is a *silently dropped root*, i.e. the
    /// use-after-free this whole module exists to prevent, reintroduced at the
    /// seam.
    ///
    /// Carrying the base here removes the second map and the way it can
    /// disagree with the first. It costs one `u64` per old page (against the
    /// `page_size / 32` bytes of bitmap beside it, so rounding error) and it
    /// imports nothing: a base is a number, exactly as the id and the size are,
    /// and this module still never dereferences one.
    ///
    /// `0` is reserved for "the registrar did not know it", which is what
    /// [`ZRememberedSetTable::register_old_page`] passes. That is not a defect
    /// — a table used only through `remember`/`iterate` never needs a base —
    /// but [`Self::slot_address`] answers `None` for such a set rather than
    /// returning `0 + offset`, which would be a wild address dressed as a root.
    base: AtomicU64,
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
    /// One bit per WORD of the matching buffer: a superset of that buffer's
    /// non-zero words (zr z1/gn, `CRATONVM_ZGC_REMSET_SUMMARY`).
    ///
    /// # Why
    ///
    /// A 2 MiB page has 4096 bitmap words, and every young cycle used to make
    /// four full passes over each carded page's words -- the swap's wipe, the
    /// snapshot scan, `clear_snapshot` and the `bits_set` popcount the driver
    /// asks to decide whether the page can be dropped -- whatever the number
    /// of cards on it. That is ~128 KiB of bitmap traffic per page with ONE
    /// card, i.e. O(carded old heap / 16) per young cycle, not O(cards). With
    /// the summary each of the four touches only the words the summary names
    /// (64 summary words per page, then one load per dirty word).
    ///
    /// # Invariant
    ///
    /// `summaries[b]` bit `w` is set whenever `buffers[b][w]` is non-zero.
    /// [`Self::remember`] sets the summary bit BEFORE the word bit, so a
    /// mutator stopped between the two leaves at worst a summary bit over a
    /// zero word (a wasted load), never a non-zero word the summary hides.
    /// Every clear of a buffer clears its summary in the same pass. The
    /// summary is maintained whatever the switch says; the switch only picks
    /// which words the four passes visit.
    summaries: [Box<[AtomicU64]>; 2],
    /// Index (0 or 1) of the buffer mutators are currently dirtying.
    current: AtomicUsize,
    /// See [`ZRememberedSetStats::words_scanned`].
    words_scanned: AtomicU64,
    /// See [`ZRememberedSetStats::words_skipped`].
    words_skipped: AtomicU64,
    /// See [`ZRememberedSetStats::bits_newly_set`].
    bits_newly_set: AtomicU64,
    /// See [`ZRememberedSetStats::bits_retained`].
    bits_retained: AtomicU64,
    /// [`zgc_remset_summary_enabled`] as the owning table read it (zr z3/lt).
    /// Fixed for the set's life, so its four passes always agree.
    summary_on: bool,
}

impl ZRememberedSet {
    /// Create an empty remembered set covering `page_size` bytes of the old
    /// page identified by `page_id`, with no known base address.
    ///
    /// Both buffers are allocated up front. Lazy allocation would put a
    /// branch-and-maybe-allocate on the store barrier's fast path, which is
    /// the last place in the VM that can afford one.
    pub fn new(page_id: u64, page_size: usize) -> Self {
        Self::new_seeded(page_id, page_size, zgc_remset_summary_enabled())
    }

    /// [`Self::new`] with the summary switch supplied by the owning table
    /// (zr z3/lt), instead of read from the environment per set.
    pub fn new_seeded(page_id: u64, page_size: usize, summary_on: bool) -> Self {
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

        let summary_words = word_count.div_ceil(Z_REMSET_BITS_PER_WORD);
        let make_summary =
            || -> Box<[AtomicU64]> { (0..summary_words).map(|_| AtomicU64::new(0)).collect() };

        Self {
            page_id,
            base: AtomicU64::new(0),
            page_size,
            word_count,
            buffers: [make(), make()],
            summaries: [make_summary(), make_summary()],
            current: AtomicUsize::new(0),
            words_scanned: AtomicU64::new(0),
            words_skipped: AtomicU64::new(0),
            bits_newly_set: AtomicU64::new(0),
            bits_retained: AtomicU64::new(0),
            summary_on,
        }
    }

    /// [`Self::new`], with the page's base machine address supplied.
    ///
    /// Prefer this wherever the caller has the page in hand — see the field
    /// docs on [`Self::base`] for why a set that knows its own base removes the
    /// consumer-side base map that can silently drop roots.
    pub fn new_with_base(page_id: u64, base: u64, page_size: usize) -> Self {
        let set = Self::new(page_id, page_size);
        set.set_base(base);
        set
    }

    /// The old page this set belongs to.
    #[inline]
    pub fn page_id(&self) -> u64 {
        self.page_id
    }

    /// The page's base machine address, or `None` if the registrar did not
    /// supply one. See [`Self::base`].
    #[inline]
    pub fn base_address(&self) -> Option<u64> {
        match self.base.load(Ordering::Relaxed) {
            0 => None,
            base => Some(base),
        }
    }

    /// Supply (or correct) the page's base machine address.
    ///
    /// `Relaxed`: the base is immutable for the life of a page and is written
    /// by the registrar before the page is handed to anything that scans it, so
    /// there is no publication to order. It is an atomic at all only so that
    /// [`Self::new`]'s "not supplied" case can be filled in later through a
    /// shared `Arc` without a lock.
    #[inline]
    pub fn set_base(&self, base: u64) {
        self.base.store(base, Ordering::Relaxed);
    }

    /// The **machine address** of the reference slot at
    /// `page_relative_offset`, or `None` when the offset is outside the page or
    /// this set has no base.
    ///
    /// This is the one place a page-relative bitmap offset becomes something a
    /// consumer can read. It refuses rather than guessing for the same reason
    /// [`Self::locate`] does: a wild address handed to a root scan is worse
    /// than a dropped diagnostic.
    #[inline]
    pub fn slot_address(&self, page_relative_offset: usize) -> Option<u64> {
        if page_relative_offset >= self.page_size {
            return None;
        }
        let base = self.base_address()?;
        base.checked_add(page_relative_offset as u64)
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
    /// [`CardTable::mark_dirty`](cratonvm_gc_core::card_table::CardTable::mark_dirty):
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
    /// Contrast [`CardTable::mark_dirty`](cratonvm_gc_core::card_table::CardTable::mark_dirty),
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
        let index = self.current_index();
        // SUMMARY FIRST -- see `Self::summaries` for why the order is the
        // invariant. A load before the RMW: after the first card in a word
        // every later one finds the summary bit already set.
        let summary = &self.summaries[index][word / Z_REMSET_BITS_PER_WORD];
        let summary_mask = 1u64 << (word % Z_REMSET_BITS_PER_WORD);
        if summary.load(Ordering::Relaxed) & summary_mask == 0 {
            summary.fetch_or(summary_mask, Ordering::Release);
        }
        let buffer = &self.buffers[index];
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
    /// `page_base + offset` is a root to trace from
    /// ([`Self::slot_address`] does that addition).
    ///
    /// # ⚠ Read-only, so it forgets
    ///
    /// This variant consumes the snapshot and writes nothing back, so every
    /// edge it yields is **gone after the next [`Self::swap`]** unless a
    /// mutator stores through the same slot again in the meantime. For a
    /// long-lived old→young edge — which is the population a remembered set
    /// exists for — no such store happens, and the second young cycle after the
    /// store frees a live object. Use it for diagnostics, for a
    /// one-cycle-and-done test, or when the caller renews the edges itself; a
    /// production young cycle wants [`Self::iterate_snapshot_retaining`], which
    /// carries still-young edges forward. The full argument, with the two-cycle
    /// trace, is on that method.
    pub fn iterate_snapshot<F: FnMut(usize)>(&self, f: F) {
        self.iterate_buffer(self.snapshot_index(), f);
    }

    /// [`Self::iterate_snapshot`], **carrying still-live edges forward**.
    ///
    /// `f` is handed each remembered offset and answers *"does this slot still
    /// hold a young reference?"*. Every `true` is re-set in the buffer mutators
    /// are dirtying, so it survives the next [`Self::swap`]. Returns how many
    /// were carried forward.
    ///
    /// # This is not an optimisation. It is the other half of the swap
    ///
    /// [`Self::swap`] makes the mutators' buffer the collector's snapshot and
    /// **wipes** what the collector scanned last cycle. Work the lifecycle of
    /// one bit through two cycles:
    ///
    /// ```text
    ///   interval 0   mutator stores old.f = young_obj    -> bit set in buf A
    ///   cycle 1      swap: A becomes the snapshot, B is wiped and goes current
    ///                scan A, trace young_obj, it survives
    ///   interval 1   nobody stores into old.f again      -> B stays clean
    ///   cycle 2      swap: B becomes the snapshot, A IS WIPED
    ///                scan B: empty. old.f -> young_obj is NOT a root.
    ///                young_obj is freed while old.f still points at it.
    /// ```
    ///
    /// The bit is not lost to a race; it is *erased on schedule*, by a swap
    /// doing exactly what it is documented to do. The store barrier cannot
    /// repair it either: the barrier fires on a **store**, and no store
    /// happened in interval 1 — that is the whole point of a long-lived edge.
    /// So the only place the edge can be renewed is the scan that just read it,
    /// which is why OpenJDK's `ZRemembered::scan_field` re-marks a field it
    /// finds still pointing into young, and why this method exists.
    ///
    /// **A consumer that calls [`Self::iterate_snapshot`] instead is correct
    /// for exactly one cycle after each store and unsound afterwards.** The
    /// signature is the enforcement available here: this variant cannot be
    /// called without answering the question, and the plain variant's doc now
    /// says what it costs.
    ///
    /// # Why re-setting into `current` and not into the snapshot
    ///
    /// The snapshot is about to be wiped by the next swap, so a bit written
    /// there is a bit written into the bin. `current` is the buffer that
    /// *becomes* the next snapshot, which is where a bit has to be for the next
    /// cycle to see it — and it is the same buffer a concurrent mutator store
    /// would have targeted, so the two compose by `fetch_or` with no extra
    /// rule.
    ///
    /// # Cost
    ///
    /// One `fetch_or` per **surviving** edge, on top of the scan that was
    /// happening anyway. Nothing per clean word and nothing per dead edge: an
    /// edge whose target died, was promoted, or was overwritten with `null`
    /// answers `false` and is dropped, which is how the set stays sparse
    /// instead of accumulating every edge the program has ever made.
    ///
    /// # Why the retain path cannot grow the set without bound
    ///
    /// Worth stating, because "re-set every bit you just read" is the shape of
    /// a ratchet, and a remembered set that only ever grows would turn every
    /// young cycle into a whole-heap scan one bit at a time. Three independent
    /// reasons it does not:
    ///
    /// 1. **The destination is a fixed-size bitmap, not a list.** `current`
    ///    is `buffers[i]`, a `Vec<AtomicU64>` of exactly `word_count` words,
    ///    sized once from `page_size` at construction and never resized.
    ///    `remember` refuses an out-of-range offset outright
    ///    (`Self::locate` returns `None`). The ceiling is therefore
    ///    `page_size / Z_REMSET_GRAIN` bits — the number of reference-sized
    ///    slots the page can physically hold — whatever any `f` answers.
    /// 2. **Re-setting is idempotent.** `fetch_or` of a bit already set is a
    ///    no-op, and `retained` only counts offsets that were in range. A slot
    ///    scanned on a hundred consecutive cycles occupies one bit on all of
    ///    them.
    /// 3. **It adds no pages.** This method writes into the set it was called
    ///    on and touches no other; [`ZRememberedSetTable`]'s map is unchanged
    ///    by a scan. The only growth vector for the *table* is registration,
    ///    which is a promotion event, not a scan.
    ///
    /// So the steady state is "one bit per live old→young slot", which is the
    /// population the structure is defined to hold, and an `f` that answered
    /// `true` unconditionally would converge on the page's slot count rather
    /// than diverge. Pinned by `gc/tests/zgc_j_remembered_retention.rs`.
    pub fn iterate_snapshot_retaining<F: FnMut(usize) -> bool>(&self, mut f: F) -> u64 {
        let mut retained: u64 = 0;
        self.iterate_buffer(self.snapshot_index(), |offset| {
            if f(offset) {
                // Into `current`, deliberately — see the doc above.
                if self.remember(offset) {
                    retained += 1;
                }
            }
        });
        // Relaxed: a diagnostic, like every other counter on this type.
        self.bits_retained.fetch_add(retained, Ordering::Relaxed);
        retained
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
        if self.summary_on {
            self.iterate_buffer_summarised(buffer_index, f);
            return;
        }
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

    /// [`Self::iterate_buffer`] through [`Self::summaries`]: visit only the
    /// words the summary names. Same offsets, same (ascending) order, and the
    /// same `words_scanned` / `words_skipped` accounting as the full walk --
    /// the counters stay "words of page examined" / "of which zero", so a
    /// sparsity ratio read off either arm means the same thing.
    fn iterate_buffer_summarised<F: FnMut(usize)>(&self, buffer_index: usize, mut f: F) {
        let buffer = &self.buffers[buffer_index];
        let mut nonzero: u64 = 0;
        for (si, scell) in self.summaries[buffer_index].iter().enumerate() {
            let mut sword = scell.load(Ordering::Acquire);
            while sword != 0 {
                let sbit = sword.trailing_zeros() as usize;
                sword &= sword - 1;
                let word_index = si * Z_REMSET_BITS_PER_WORD + sbit;
                let Some(cell) = buffer.get(word_index) else {
                    continue;
                };
                // `Acquire`: pairs with `remember`'s `Release`, as in the full walk.
                let mut word = cell.load(Ordering::Acquire);
                if word == 0 {
                    continue;
                }
                nonzero += 1;
                let word_base = word_index * Z_REMSET_BYTES_PER_WORD;
                while word != 0 {
                    let bit = word.trailing_zeros() as usize;
                    let offset = word_base + (bit << Z_REMSET_GRAIN_SHIFT);
                    if offset < self.page_size {
                        f(offset);
                    }
                    word &= word - 1;
                }
            }
        }
        let scanned = self.word_count as u64;
        self.words_scanned.fetch_add(scanned, Ordering::Relaxed);
        self.words_skipped
            .fetch_add(scanned - nonzero, Ordering::Relaxed);
    }

    /// Zero buffer `buffer_index` and its summary. With the summary switch on
    /// only the words the summary names are stored to; off, every word is
    /// (the pre-zr shape). Either way the summary ends all-zero, which is what
    /// [`Self::summaries`]'s invariant needs from a cleared buffer.
    fn zero_buffer(&self, buffer_index: usize) {
        let buffer = &self.buffers[buffer_index];
        if self.summary_on {
            for (si, scell) in self.summaries[buffer_index].iter().enumerate() {
                let mut sword = scell.load(Ordering::Relaxed);
                while sword != 0 {
                    let sbit = sword.trailing_zeros() as usize;
                    sword &= sword - 1;
                    if let Some(cell) = buffer.get(si * Z_REMSET_BITS_PER_WORD + sbit) {
                        cell.store(0, Ordering::Relaxed);
                    }
                }
                scell.store(0, Ordering::Relaxed);
            }
        } else {
            for cell in buffer.iter() {
                cell.store(0, Ordering::Relaxed);
            }
            for scell in self.summaries[buffer_index].iter() {
                scell.store(0, Ordering::Relaxed);
            }
        }
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
    /// reason [`CardTable::flush_all`](cratonvm_gc_core::card_table::CardTable::flush_all)
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
    /// # INVARIANT: the scan must renew what it wants to keep
    ///
    /// The swap **erases** the buffer the previous cycle scanned. That is
    /// correct and intended — it is what keeps the set sparse — but it means a
    /// bit only ever protects its edge for *one* young cycle after the store
    /// that set it. A long-lived old→young reference is stored once and then
    /// never again, so nothing renews its bit unless the collector does.
    ///
    /// [`Self::iterate_snapshot_retaining`] is that renewal, and a young cycle
    /// that scans with the plain [`Self::iterate_snapshot`] instead is sound on
    /// the cycle after each store and unsound on every later one. The failure
    /// is the module's worst — a live young object freed, no error, no
    /// coarsening, no repeatable symptom — and it is *scheduled* rather than
    /// racy, which makes it reproducible once you know to look for it and
    /// invisible until then.
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
        self.zero_buffer(incoming);

        self.current.store(incoming, Ordering::Release);
    }

    /// Zero both bitmaps and reset nothing else (the lifetime scan counters
    /// are deliberately kept — they are cumulative diagnostics).
    ///
    /// STW only, for the same reason as [`Self::swap`]: a concurrent
    /// `remember` racing a clear loses its edge.
    pub fn clear(&self) {
        self.zero_buffer(0);
        self.zero_buffer(1);
    }

    /// Zero only the collector's snapshot buffer, after it has been scanned.
    ///
    /// Optional: [`Self::swap`] wipes the incoming buffer anyway. Calling this
    /// as soon as the scan finishes just returns the pages to a clean state
    /// earlier, which makes [`Self::stats`] readable mid-cycle.
    pub fn clear_snapshot(&self) {
        self.zero_buffer(self.snapshot_index());
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
        if self.summary_on {
            let buffer = &self.buffers[buffer_index];
            for (si, scell) in self.summaries[buffer_index].iter().enumerate() {
                let mut sword = scell.load(Ordering::Acquire);
                while sword != 0 {
                    let sbit = sword.trailing_zeros() as usize;
                    sword &= sword - 1;
                    if let Some(cell) = buffer.get(si * Z_REMSET_BITS_PER_WORD + sbit) {
                        total += cell.load(Ordering::Acquire).count_ones() as usize;
                    }
                }
            }
            return total;
        }
        for cell in self.buffers[buffer_index].iter() {
            total += cell.load(Ordering::Acquire).count_ones() as usize;
        }
        total
    }

    /// Bytes of remembered-set metadata this set retains: both bitmaps plus
    /// the struct itself. Compare
    /// [`CardTable::retained_bytes`](cratonvm_gc_core::card_table::CardTable::retained_bytes),
    /// which measures the same thing for the card-table representation.
    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + 2 * (self.word_count + self.word_count.div_ceil(Z_REMSET_BITS_PER_WORD))
                * std::mem::size_of::<AtomicU64>()
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
            bits_retained: self.bits_retained.load(Ordering::Relaxed),
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
    /// "Coarsen **everything**": an old→young edge was seen whose page could
    /// not be named at all, so there is no id to put in `coarse`.
    ///
    /// # Why a flag rather than nothing (2026-09-20)
    ///
    /// [`ZStoreBarrier::on_reference_store`] has a branch where
    /// [`ZGenerationContext::is_old`] says yes and
    /// [`ZGenerationContext::page_of`] says `None`. It logged, counted a coarse
    /// edge, and returned [`ZStoreBarrierOutcome::Coarsened`] — but coarsening
    /// needs a page id, and that branch has none, so **nothing was recorded
    /// anywhere and the edge was dropped**. The outcome said "the next young
    /// cycle will rescan the page" and the truth was "the next young cycle will
    /// never hear about this edge", which is the exact shape of silent
    /// use-after-free this type's `ZRememberOutcome::Coarsened` doc promises
    /// cannot happen ("never *silently drop* an edge").
    ///
    /// This latch is the honest over-approximation: once set,
    /// [`Self::take_coarse_pages`] returns **every registered old page**, so the
    /// edge is found by a full scan wherever it lives. It is ruinously
    /// expensive and it is meant to be — the branch that sets it is a wiring
    /// error that should never execute, and a young cycle that silently costs
    /// what a major costs is how an operator finds out.
    ///
    /// `Relaxed`: it is a latch read once per cycle by the collector, at a
    /// safepoint, after the mutators that set it have been parked; the parking
    /// handshake supplies any ordering, and nothing is published through it.
    coarse_all: AtomicBool,
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
    /// `CRATONVM_ZGC_REMSET_SUMMARY`, read once per table (per heap) and
    /// handed to every set it creates (zr z3/lt; was a process `OnceLock`).
    summary_on: bool,
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
            coarse_all: AtomicBool::new(false),
            namespace: AtomicU8::new(0),
            summary_on: zgc_remset_summary_enabled(),
        }
    }

    /// This table's `CRATONVM_ZGC_REMSET_SUMMARY` (zr z3/lt).
    pub fn summary_enabled(&self) -> bool {
        self.summary_on
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

    /// [`Self::register_old_page`], supplying the page's **base machine
    /// address** as well.
    ///
    /// Prefer this at every call site that has the page in hand — which is all
    /// four of them, since they all already call `page.id()` and `page.size()`
    /// on something that also answers `page.base()`. The base is what lets
    /// [`Self::iterate_slot_addresses`] turn a bitmap offset into a root
    /// without a second, separately-maintained `page_id -> base` map on the
    /// consumer's side; see [`ZRememberedSet::base`] for why that second map is
    /// a root-dropping hazard rather than a convenience.
    ///
    /// Idempotent like its base-less sibling, with one addition: a page already
    /// registered **without** a base has the base filled in. That makes it safe
    /// to upgrade a call site at a time.
    ///
    /// # ⚠ It declares the ALLOCATOR namespace, which is not every heap's
    ///
    /// The only heap that ships (`ZgcRealHeap`) has no [`ZPageAllocator`] at
    /// all: its page ids are `(addr - arena_base) / Z_LOGICAL_PAGE_BYTES`, a
    /// dense index over its own model of one arena, and all three of its
    /// registration sites declare [`ZPageIdNamespace::ContextLocal`]. Calling
    /// *this* entry point on that table offers `Allocator` to a latch already
    /// holding `ContextLocal`, which is the namespace latch's conflict arm: an
    /// `error!` on every call and a `debug_assert!(false)` — i.e. a panic in
    /// every debug and test build.
    ///
    /// So a driver whose ids are context-local wants
    /// [`Self::register_old_page_with_base_in`], which takes the namespace.
    /// This shorthand stays for the allocator-keyed callers (the adapters and
    /// the integration suite) and for its existing tests.
    ///
    /// [`ZPageAllocator`]: crate::zgc::page::ZPageAllocator
    pub fn register_old_page_with_base(
        &self,
        page_id: u64,
        base: u64,
        page_size: usize,
    ) -> Arc<ZRememberedSet> {
        self.register_old_page_with_base_in(ZPageIdNamespace::Allocator, page_id, base, page_size)
    }

    /// [`Self::register_old_page_with_base`], with the id's namespace stated
    /// explicitly.
    ///
    /// # Why this had to be added (2026-09-20, wave 3)
    ///
    /// `register_old_page_with_base` and [`Self::recycle_old_page`] landed in
    /// one wave hard-coding [`ZPageIdNamespace::Allocator`]; the namespace latch
    /// and `ZgcRealHeap`'s three `ContextLocal` registrations landed in another.
    /// Neither wave could see the other, and the result was that the base — the
    /// whole point of which is to let [`Self::iterate_slot_addresses`] turn a
    /// bitmap offset into a root without the consumer-side `page_id -> base` map
    /// that silently drops them — had **no reachable call site on the shipping
    /// heap**. The one entry point that carried a base would have tripped the
    /// namespace conflict assert on first use.
    ///
    /// The base and the numbering are independent facts about a page and this
    /// is the form that says both.
    pub fn register_old_page_with_base_in(
        &self,
        namespace: ZPageIdNamespace,
        page_id: u64,
        base: u64,
        page_size: usize,
    ) -> Arc<ZRememberedSet> {
        let set = self.register_old_page_in(namespace, page_id, page_size);
        match set.base_address() {
            None => set.set_base(base),
            Some(existing) if existing != base => {
                // The same id naming two different addresses is the page-recycle
                // case (see `ZRememberedSet::page_id`) reaching this table
                // without a `remove` in between. The bits belong to the page
                // that is gone; keeping them would hand the collector offsets of
                // objects that no longer exist.
                tracing::warn!(
                    target: "zgc::remembered",
                    page_id,
                    old_base = existing,
                    new_base = base,
                    "old page id re-registered at a different base: the page was \
                     freed and recycled without ZRememberedSetTable::remove. \
                     Clearing the stale bitmap; call recycle_old_page to do this \
                     deliberately.",
                );
                set.clear();
                set.set_base(base);
            }
            Some(_) => {}
        }
        set
    }

    /// Re-register a page id whose page has been **freed and recycled**,
    /// discarding the previous page's bits.
    ///
    /// # When this is the right call
    ///
    /// `ZPageAllocator` caches freed Small and Medium pages and hands them back
    /// **with their original id** (`page.rs:1573-1580`), so a page id is not a
    /// unique name for a generation of objects — see the ⚠ block on
    /// [`ZRememberedSet::page_id`]. [`Self::register_old_page`] is idempotent by
    /// design, which is right for the promotion path (re-registering a page
    /// that is already old must not wipe its edges) and wrong for the recycle
    /// path (the bits name objects that no longer exist).
    ///
    /// The two cases are indistinguishable from inside this module — it holds a
    /// `u64` and a `usize` and nothing that can tell one page's lifetime from
    /// another's — so they are two entry points, and the owner of the page
    /// picks. Getting it wrong in this direction costs a scan
    /// (`recycle_old_page` on a live page drops edges the store barrier will
    /// not re-record — **that direction is unsound**, so only call this when
    /// the page really has been through `free_page`).
    ///
    /// Declares [`ZPageIdNamespace::Allocator`]; a context-local driver wants
    /// [`Self::recycle_old_page_in`]. See the ⚠ block on
    /// [`Self::register_old_page_with_base`] for why offering the wrong
    /// namespace is a debug panic rather than a warning.
    pub fn recycle_old_page(
        &self,
        page_id: u64,
        base: u64,
        page_size: usize,
    ) -> Arc<ZRememberedSet> {
        self.recycle_old_page_in(ZPageIdNamespace::Allocator, page_id, base, page_size)
    }

    /// [`Self::recycle_old_page`], with the id's namespace stated explicitly.
    pub fn recycle_old_page_in(
        &self,
        namespace: ZPageIdNamespace,
        page_id: u64,
        base: u64,
        page_size: usize,
    ) -> Arc<ZRememberedSet> {
        self.remove(page_id);
        self.register_old_page_with_base_in(namespace, page_id, base, page_size)
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
            // A size disagreement is the page-recycle case (see the ⚠ block on
            // `ZRememberedSet::page_id`) crossing a size class, or a caller
            // passing a size it does not actually know. Neither loses an edge —
            // an offset past the smaller bitmap is refused and coarsens — but
            // both mean this set describes a page that is not the one the caller
            // has in hand, so say so rather than returning it silently.
            if existing.page_size() != page_size {
                tracing::warn!(
                    target: "zgc::remembered",
                    page_id,
                    registered_size = existing.page_size(),
                    offered_size = page_size,
                    "old page id re-registered at a different size; keeping the \
                     existing bitmap. If the page was freed and recycled, use \
                     ZRememberedSetTable::recycle_old_page instead.",
                );
            }
            return existing;
        }
        let created = Arc::new(ZRememberedSet::new_seeded(
            page_id,
            page_size,
            self.summary_on,
        ));
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
        self.coarse_all.store(false, Ordering::Relaxed);
    }

    /// Mark that **every** registered old page must be scanned in full this
    /// cycle, because an old→young edge was seen that could not be attributed
    /// to any page.
    ///
    /// See [`Self::coarse_all`] for why a flag rather than a dropped edge. The
    /// only caller is [`ZStoreBarrier::on_reference_store`]'s unmappable-address
    /// branch; it is public so a test can pin the behaviour without reaching
    /// through the barrier.
    pub fn coarsen_all_pages(&self) {
        self.coarse_all.store(true, Ordering::Relaxed);
    }

    /// Is the "scan every old page" latch set? Diagnostic; [`Self::take_coarse_pages`]
    /// is what consumes it.
    pub fn coarsen_all_pending(&self) -> bool {
        self.coarse_all.load(Ordering::Relaxed)
    }

    /// Visit every remembered old→young slot as a **machine address**, across
    /// every registered page.
    ///
    /// This is the call `adapters.rs`'s seam-4 note asks this module for
    /// ("*what `remembered.rs` must add for this trait to be deletable*"),
    /// minus the part this module must not do. Of the three decisions that note
    /// lists, two are answered here and the third deliberately is not:
    ///
    /// 1. **The page id → base map** is answered: the base lives on the set
    ///    ([`ZRememberedSet::base`]), supplied by
    ///    [`Self::register_old_page_with_base`]. A page registered without one
    ///    yields nothing and is counted in the returned `skipped` figure, so a
    ///    half-upgraded call site shows up as a number rather than as missing
    ///    roots.
    /// 2. **The load's atomicity and ordering** is *not* answered, because this
    ///    module still never dereferences anything — it hands out the slot
    ///    address and the caller reads it. That keeps this file free of
    ///    `unsafe` and keeps the read where the storage is owned.
    /// 3. **Whether the yielded word is colored** is likewise the reader's, for
    ///    the same reason.
    ///
    /// `retain` decides persistence exactly as
    /// [`ZRememberedSet::iterate_snapshot_retaining`] does when `buffer` is the
    /// snapshot: answer `true` for a slot that still holds a young reference
    /// and the edge survives the next swap. It is ignored for the dirty buffer,
    /// which is already the buffer a `true` would write into.
    ///
    /// Returns `(visited, retained, pages_without_a_base)`.
    pub fn iterate_slot_addresses(
        &self,
        snapshot: bool,
        retain: &mut dyn FnMut(u64) -> bool,
    ) -> (u64, u64, usize) {
        let mut visited: u64 = 0;
        let mut retained: u64 = 0;
        let mut baseless: usize = 0;
        for set in self.snapshot() {
            if set.base_address().is_none() {
                baseless += 1;
                continue;
            }
            if snapshot {
                retained += set.iterate_snapshot_retaining(|offset| {
                    visited += 1;
                    match set.slot_address(offset) {
                        Some(addr) => retain(addr),
                        None => false,
                    }
                });
            } else {
                set.iterate(|offset| {
                    visited += 1;
                    if let Some(addr) = set.slot_address(offset) {
                        retain(addr);
                    }
                });
            }
        }
        if baseless > 0 {
            tracing::warn!(
                target: "zgc::remembered",
                pages = baseless,
                "remembered-set scan skipped pages with no base address: their \
                 old->young edges are NOT roots this cycle. Register them with \
                 register_old_page_with_base.",
            );
        }
        (visited, retained, baseless)
    }

    /// Total snapshot bits carried forward across every registered page. See
    /// [`ZRememberedSetStats::bits_retained`].
    pub fn total_bits_retained(&self) -> u64 {
        self.snapshot()
            .iter()
            .map(|s| s.stats().bits_retained)
            .sum()
    }

    /// Take the set of pages that must be scanned in full this cycle, clearing
    /// it.
    ///
    /// A non-empty result is a **wiring bug, not a heap bug**: it means the
    /// store barrier saw a store into an old page that was never registered
    /// with [`Self::register_old_page`]. The collection stays correct (the
    /// page is scanned conservatively), but the page pays a full scan.
    pub fn take_coarse_pages(&self) -> Vec<u64> {
        // The "an old address named no page at all" latch first: there is no id
        // to coarsen, so the only sound answer is every page. See `coarse_all`.
        if self.coarse_all.swap(false, Ordering::Relaxed) {
            let mut guard = self.coarse.lock();
            guard.clear();
            drop(guard);
            let pages = self.page_ids();
            if pages.is_empty() {
                // "Scan every old page" over an empty table scans NOTHING, so
                // the edge that set the latch is lost after all — and the latch
                // has just been consumed, so no later cycle recovers it either.
                //
                // This is not a hypothetical: the branch that sets the latch
                // fires exactly when the generation context cannot name a page,
                // and a context that cannot name pages is usually attached to a
                // table that has none registered. The over-approximation is
                // only honest while there is something to over-approximate
                // over; when there is not, the only correct thing this module
                // can do is say so at `error!` rather than log "the whole old
                // generation was coarsened" over a no-op.
                tracing::error!(
                    target: "cratonvm::gc::guard",
                    "EVERY old page coarsened, but NO old page is registered: the \
                     old->young edge that forced this has been lost, not \
                     over-approximated. The generation context reports addresses as \
                     old for pages nothing ever registered on this table -- see \
                     ZStoreBarrier::on_reference_store and \
                     ZRememberedSetTable::register_old_page_in.",
                );
                return pages;
            }
            tracing::warn!(
                target: "zgc::remembered",
                pages = pages.len(),
                "EVERY old page coarsened: the store barrier saw an old->young \
                 store whose address mapped to no page, so the edge could not be \
                 attributed. This young cycle scans the whole old generation. \
                 The generation context and the page allocator disagree about \
                 what is old — see ZStoreBarrier::on_reference_store.",
            );
            return pages;
        }
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
    /// [`CardTable::retained_bytes`](cratonvm_gc_core::card_table::CardTable::retained_bytes)
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
/// [`Arena`](cratonvm_gc_core::arena::Arena) whose addresses are the only names it has.
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
    /// An old→young edge that could not be recorded precisely, so something
    /// was coarsened. Two sub-cases, both safe, one much more expensive:
    ///
    /// * the source page had no registered remembered set, or the offset fell
    ///   outside its bitmap — **that page** is coarsened
    ///   ([`ZRememberOutcome::Coarsened`]);
    /// * [`ZGenerationContext::is_old`] said yes and
    ///   [`ZGenerationContext::page_of`] could not name a page at all — there
    ///   is no id to coarsen, so **the whole old generation** is
    ///   ([`ZRememberedSetTable::coarsen_all_pages`]).
    ///
    /// The second is a wiring error and is meant to be visible as one; see the
    /// branch in [`ZStoreBarrier::on_reference_store`].
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
    /// barrier's job (see [`cratonvm_gc_core::satb`]) and a different invariant.
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
                // `is_old` said yes but the context cannot name the page.
                //
                // This used to log, bump the coarse counter and return
                // `Coarsened` — while recording **nothing anywhere**, because
                // coarsening needs a page id and this branch has none. The
                // outcome said "the next young cycle rescans that page"; the
                // truth was "the next young cycle never hears about this edge",
                // and a live young object is freed. Of all the ways this module
                // can go wrong that was the only one where the type's own
                // promise ("never *silently drop* an edge") was false.
                //
                // The honest over-approximation is to coarsen the whole old
                // generation: there is no smaller unit that provably contains
                // the edge. It costs a full old scan on the next young cycle,
                // which is the point — this branch is a wiring error between the
                // generation context and the page allocator, and a young cycle
                // that silently costs what a major costs is how it gets found.
                tracing::warn!(
                    target: "zgc::remembered",
                    field_addr,
                    "store barrier: address is in the old generation but maps \
                     to no page; coarsening the WHOLE old generation so the \
                     old->young edge is not lost",
                );
                self.table.coarsen_all_pages();
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
                // Same reasoning as `on_reference_store`'s unmappable branch,
                // and worse here: this is not one edge, it is **every** outgoing
                // reference of a just-promoted object, and this call is the only
                // record any of them will ever have (the store barrier filtered
                // them all while the object was young). Returning 0 quietly was
                // a guaranteed use-after-free for every young object reachable
                // solely through this parent.
                tracing::warn!(
                    target: "zgc::remembered",
                    promoted_addr,
                    fields = outgoing.len(),
                    "on_promote: promoted object maps to no old page \
                     (constraints 1 and 2 on ZStoreBarrier::on_promote); \
                     coarsening the WHOLE old generation so its old->young \
                     edges are not lost",
                );
                self.table.coarsen_all_pages();
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
mod tests;
