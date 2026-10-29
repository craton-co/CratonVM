// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! [`HeapBitmap`] -- one bit per 8 bytes of a contiguous heap span, atomic,
//! alignment-exact, with removal and claiming.
//!
//! # Why this is a crate-level module and not a fourth copy
//!
//! It was `zgc::starts::ZObjectStartBits`, and its own header enumerated the
//! three near-identical bitmaps it could not reuse:
//!
//! * [`crate::young_mark::ObjectStartBits`] -- right semantics (alignment-exact)
//!   but `&mut self` over a plain `Vec<u64>`, "single-threaded by construction",
//!   and no `remove`.
//! * [`crate::young_mark::YoungMarkBits`] -- atomic, but its `locate`
//!   deliberately does NOT check alignment (`addr` and `addr + 4` map to the
//!   same bit) because its callers pre-screen, and no `remove`.
//! * [`crate::mark_bitmap::MarkBitmap`] -- atomic, but not alignment-exact and
//!   no `remove`.
//!
//! "Neither file is edited", it concluded, and wrote a fourth. This type is a
//! strict superset of all three, and the reason the other collectors could not
//! have it was that it lived inside a `#[cfg(feature = "zgc")]` module -- not
//! anything about the type, which references no `zgc` item at all.
//!
//! `zgc::starts` re-exports it under its old name, so that collector is
//! untouched. What changes is that the generational and G1 collectors can now
//! reach the one implementation that is atomic, alignment-exact and removable,
//! instead of the weaker ones they were left with.
//!
//! The measurement, the in-tree precedent and the exactness argument are the
//! section header immediately below, kept verbatim from where they were
//! written.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;
use rustc_hash::FxHashSet;

// ---------------------------------------------------------------------------
// Object-start membership — the allocation-path bitmap
//
// Written for `ZgcRealHeap` and kept verbatim below, because the measurement
// and the exactness argument are what justify the structure and they are
// specific to where they were taken. The STRUCTURE is not specific to that
// collector, which is the point of this module.
// ---------------------------------------------------------------------------
//
// # Why (a measurement with an in-tree precedent, not a hypothesis)
//
// `ZgcRealHeap::registry` holds the base address of every live allocation and
// is written on EVERY allocation, from every mutator thread. It used to be a
// `Mutex<FxHashSet<usize>>`. `bench/BinTreesClassic.java`, release build,
// ABBA-interleaved, checksums identical on both sides (so this backend is
// CORRECT, just slow), 2026-08-07:
//
// ```text
//   depth 12   generational   35 ms    zgc    231 ms     6.6x
//   depth 14   generational  138 ms    zgc   1566 ms    11.3x
//   depth 16   generational  621 ms    zgc  11417 ms    18.4x
//   depth 18   generational  ~7.9 s    zgc   ~116 s     14.6x
// ```
//
// The ratio is SUPERLINEAR: each depth step is ~4x the objects, and the
// generational collector scales ~4x while this one scales ~7x. And
// `--verbose:gc` shows the depth-16 run performed exactly ONE collection
// (1.1 ms, `bytes_freed=0`), finishing at 719 MB against a 4.2 GB heap. So the
// cost is not the collector, and it is not allocation locking — TLABs
// (`ZgcRealHeap::tlabs`) landed first and did not move the number. It is
// per-allocation work on the mutator path that grows with the LIVE-OBJECT
// COUNT. Tens of millions of entries in one global hash set, behind one global
// mutex, probed with cache-missing scatter reads over a multi-gigabyte table,
// is exactly that shape and nothing else on the path is.
//
// # The precedent
//
// `gen_heap` had this defect and it was diagnosed and fixed on 2026-07-26
// (landed `7307935bc`). There a `young_object_starts: FxHashSet<usize>` made a
// moving collection cost O(objects *allocated*) instead of O(objects
// *surviving*): `perf` over bt18 measured `HashMap::insert` at 40.7% of the
// whole process plus `reserve_rehash` at 8.3% — 49% together. The fix was
// [`crate::young_mark::ObjectStartBits`], one bit per 8 bytes of
// `[base, base + used)`, and bt18 went 5.0x -> 2.1x. That finding's own
// generalisation is why this section exists: *any membership-over-a-contiguous-
// arena set in this GC is a bitmap candidate.* This registry is precisely that,
// and worse than the one that was fixed — `gen_heap`'s set was built once per
// collection; this one is written on every allocation from every thread under a
// single lock.
//
// # Why the two existing bitmaps could not be reused as-is
//
// * [`crate::young_mark::ObjectStartBits`] has exactly the right *semantics*
//   (it is alignment-exact: an unaligned address is rejected rather than
//   aliased onto a neighbour's bit) but its accessors take `&mut self` over a
//   plain `Vec<u64>`, because its own doc says it is "single-threaded by
//   construction — the walk and the forwarding that reads it both run inside
//   the collection's stop-the-world region". This registry is written from
//   concurrent mutators, so it cannot serve. It also has no `remove`, which the
//   sweep's in-place prune needs.
// * [`crate::young_mark::YoungMarkBits`] IS atomic and IS the allocation model
//   copied below (`alloc_zeroed` + `*mut AtomicU64` + `fetch_or`), but its
//   `locate` deliberately does NOT check alignment — `addr` and `addr + 4` map
//   to the same bit — because its callers pre-screen. Here that would be
//   unsound: `ZgcRealHeap::is_object_address` would answer `Some` for an
//   INTERIOR conservative-root candidate and hand back an `ObjectRef` pointing
//   four bytes into an object, which is the `is_addr_live` unsoundness
//   `ZgcRealHeap::conservative_addr_span` documents. It also has no `remove`.
//
// Neither file was edited, and a fourth bitmap was written. That was the wrong
// answer -- the two weaker ones are still in use by the other collectors, which
// is exactly the cost this module now removes -- but the TYPE it produced is
// the right one. [`HeapBitmap`] below is it:
// `YoungMarkBits`'s storage and RMW discipline, `ObjectStartBits`'s exactness
// rule, plus the `remove` the sweep needs.
//
// # Why the bitmap is EXACT here (three legs, each verified against the code)
//
// 1. **One contiguous region with a stable base.** `ZgcRealHeap::with_capacity`
//    creates a single [`Arena`] and captures `arena_base`/`arena_end` from it;
//    there is no `Arena::grow` call anywhere in this file and `alloc_raw` is the
//    single arena chokepoint, so the envelope never moves. Those two fields
//    already back `ZgcRealHeap::conservative_addr_span` for exactly this
//    reason.
// 2. **Every footprint is a multiple of 8.** `Arena::alloc` opens with
//    `let size = size.checked_add(7)? & !7` (`arena.rs:579`) and warns on an
//    unrounded request, so consecutive starts sit on the 8-byte grid.
// 3. **Every start is 8-aligned relative to `arena_base`.** All three of this
//    heap's paths into the arena ask for `align == 8`: `alloc_raw` calls
//    `arena.alloc(size, 8)`, `tlab_refill` calls `arena.alloc(want,
//    ZGC_TLAB_ALIGN)` with [`ZGC_TLAB_ALIGN`]` == 8`, and the bump path aligns
//    the *offset* (`arena.rs:634`) so `addr - arena_base` is a multiple of 8 by
//    construction.
//
// Therefore `(addr - arena_base) / 8` is a total, collision-free encoding of
// "is this an object start", and a non-multiple-of-8 offset is *provably* not
// one — which is what lets [`HeapBitmap::locate`] reject it instead of
// aliasing, preserving the exact-base answer the `FxHashSet` gave.
//
// **TLAB-served allocations preserve all three.** A TLAB is not a second
// allocator: `tlab_refill` carves its chunk out of this same arena with
// `arena.alloc(want, ZGC_TLAB_ALIGN)`, so the chunk base is on the grid; every
// object inside is bumped at [`ZGC_TLAB_ALIGN`] (the constant's own doc pins it
// at 8 precisely so "the cursor can never leave the object grid"); and
// [`zgc_tlab_footprint`] rounds each request to that alignment. So a TLAB start
// is `chunk_base + 8k` and `chunk_base` is `arena_base + 8j`.
//
// # The one leg that is NOT a proof, and the fallback that covers it
//
// `Arena`'s backing store is a `Vec<u8>`, whose pointer Rust only guarantees to
// be 1-aligned; and `Arena::alloc`'s FREE-LIST tiers align the *absolute*
// address (`arena.rs:481`, `:548`) while the bump tier aligns the *offset*
// (`arena.rs:634`). The two agree iff `arena_base` is itself 8-aligned, which
// every real allocator delivers for a multi-megabyte block but no type in this
// tree asserts. Rather than bet on it, [`HeapBitmap`] keeps an
// [`overflow`](HeapBitmap::overflow) set for any address the grid cannot
// encode. It is empty on every real run (and a one-shot `tracing::warn!` says
// so if it is not), it costs one relaxed load of a never-written cache line on
// the query path, and it means a base can never be LOST — losing one would make
// `is_object_address` deny a reachable object and drop it from conservative
// rooting, which is a crash, not a slowdown.

/// One bit per 8 bytes of the arena: "is this address an object start?"
///
/// The atomic, removable twin of [`crate::young_mark::ObjectStartBits`] — see
/// the section header above for why neither existing bitmap could be reused and
/// for the three-leg exactness argument.
///
/// # Memory
///
/// One bit per 8 bytes is **1/64th of the arena**, committed up front: ~1 MB for
/// the 64 MB default heap, ~66 MB for a 4.2 GB one. That is deliberate rather
/// than lazily committed. `alloc_zeroed` for a block this size goes straight to
/// the OS (`mmap`/`VirtualAlloc`) and the pages are demand-faulted anyway, so
/// the resident cost already tracks the part of the arena that has been
/// allocated into; a hand-rolled two-level commit would add a dependent load to
/// `ZgcRealHeap::is_object_address`, which is on the mutator path via
/// `jit_checkcast`. It also compares favourably with what it replaces: the hash
/// set it removes cost ~8 bytes per LIVE OBJECT plus load factor — at bt16's
/// 719 MB of ~72-byte nodes that is well over 100 MB, and it grew without bound
/// because the collection that would prune it essentially never fires.
pub struct HeapBitmap {
    /// `alloc_zeroed` block of [`Self::nwords`] `AtomicU64`s, freed in `Drop`
    /// (or, zr z10/mt, a private mapping of them: see `mapped_len`).
    /// Raw rather than `Box<[AtomicU64]>` for the same reason
    /// [`crate::young_mark::YoungMarkBits`] is: `AtomicU64` is not `Clone`, so
    /// `vec![]` cannot build one, and collecting an iterator would MEMSET tens
    /// of megabytes instead of taking a zero-page mapping.
    pub words: *mut AtomicU64,
    pub nwords: usize,
    /// The arena base. Bit `i` denotes `base + i * 8`.
    pub base: usize,
    /// Arena capacity in bytes; `[base, base + span)` is the covered range.
    pub span: usize,
    /// Bases the 8-byte grid cannot encode — see the section header's "one leg
    /// that is NOT a proof". Expected to stay empty forever.
    pub overflow: Mutex<FxHashSet<usize>>,
    /// `overflow.len()`, readable without taking the lock. The query path tests
    /// this before it will even consider locking, so the fallback costs a
    /// relaxed load of a line that is never written on a healthy run.
    pub overflow_len: AtomicUsize,
    /// One-shot latch for the "the grid could not encode a base" warning: one
    /// line per allocation would itself be the hang.
    pub overflow_warned: AtomicBool,
    /// Which bitmap this is, for the spill warning. Two instances exist over
    /// the SAME geometry -- the object-start registry and the mark bits (see
    /// `ZgcRealHeap::mark_bits`) -- and a warning that does not say which one
    /// spilled names the wrong invariant.
    pub label: &'static str,
    /// One bit per BLOCK of [`BLOCK_WORDS`] words (32 KiB of arena): set before
    /// any bit in the block is, so a clear block bit proves the block's words
    /// are all zero. See [`Self::nearest_base_at_or_below`] for the scan it
    /// bounds. Cleared only by [`Self::forget_empty_blocks`] (nio21 wave 9
    /// L3), with no inserter running, for a block whose words it has just
    /// read as all zero; every other path only sets it.
    blocks: Box<[AtomicU64]>,
    /// zr z10/mt: the length of the private mapping `words` lives on
    /// ([`crate::reservation::map_side_table`]), or 0 when `words` is an
    /// `alloc_zeroed` block. Decided once at construction; `Drop` frees by it.
    mapped_len: usize,
}

/// zr z10/mt: the smallest bitmap [`HeapBitmap::labelled_side_table`] puts on
/// a mapping of its own (a 64 MiB arena). A smaller one wastes little inside
/// an allocator's huge page and would cost a VMA of its own.
pub const SIDE_TABLE_MAP_MIN_BYTES: usize = 1 << 20;

/// Words per [`HeapBitmap::blocks`] bit.
const BLOCK_WORDS: usize = 64;

/// Words per summary block, for a reader of [`HeapBitmap::block_may_hold_bits`]
/// (zr z5/sw: the ZGC complement sweep's region step).
pub const HEAP_BITMAP_BLOCK_WORDS: usize = BLOCK_WORDS;

// SAFETY: identical argument to `crate::young_mark::YoungMarkBits`. Every
// access to `words` is an atomic operation on `AtomicU64`; the allocation is
// owned exclusively by this value and freed exactly once in `Drop`. The
// `overflow` set is behind a `Mutex`.
unsafe impl Send for HeapBitmap {}
// SAFETY: as above — all shared access is atomic or mutex-guarded.
unsafe impl Sync for HeapBitmap {}

impl HeapBitmap {
    /// Cover `[base, base + span)` as the object-start registry.
    pub fn new(base: usize, span: usize) -> Self {
        Self::labelled(base, span, "object-start")
    }

    /// Cover `[base, base + span)` under a stated role.
    ///
    /// The mark bitmap is a second instance over the identical geometry, and
    /// that identity is the point: both are indexed off the same `arena_base`
    /// on the same 8-byte grid, so a base the registry can encode is a base the
    /// mark bits can encode, and neither can silently disagree with the other
    /// about which addresses are representable.
    pub fn labelled(base: usize, span: usize, label: &'static str) -> Self {
        Self::labelled_side_table(base, span, label, false)
    }

    /// [`Self::labelled`], with the words on a private `MADV_NOHUGEPAGE`
    /// mapping when `own_mapping` is set and the table is at least
    /// [`SIDE_TABLE_MAP_MIN_BYTES`] (zr z10/mt, Linux; ZGC's
    /// `CRATONVM_ZGC_SIDE_TABLE_NO_THP`). Anywhere else, or when the kernel
    /// refuses the mapping, the words are an `alloc_zeroed` block exactly as
    /// before. Both read as zero until written; only where the pages come from
    /// differs (see [`crate::reservation::map_side_table`]).
    pub fn labelled_side_table(
        base: usize,
        span: usize,
        label: &'static str,
        own_mapping: bool,
    ) -> Self {
        let nwords = span.div_ceil(8).div_ceil(64);
        let bytes = nwords.saturating_mul(std::mem::size_of::<AtomicU64>());
        let mapped = if own_mapping && nwords != 0 && bytes >= SIDE_TABLE_MAP_MIN_BYTES {
            crate::reservation::map_side_table(bytes)
        } else {
            None
        };
        let mapped_len = if mapped.is_some() { bytes } else { 0 };
        let words = if let Some(p) = mapped {
            // A fresh anonymous mapping is page-aligned and zero-filled: a
            // valid run of `nwords` `AtomicU64`s of value 0.
            p as *mut AtomicU64
        } else if nwords == 0 {
            std::ptr::NonNull::<AtomicU64>::dangling().as_ptr()
        } else {
            let layout = std::alloc::Layout::array::<AtomicU64>(nwords)
                .expect("zgc object-start bitmap layout overflow");
            // SAFETY: `nwords > 0` so the layout is non-zero-sized, and an
            // all-zero bit pattern is a valid `AtomicU64` (value 0).
            let p = unsafe { std::alloc::alloc_zeroed(layout) } as *mut AtomicU64;
            if p.is_null() {
                std::alloc::handle_alloc_error(layout);
            }
            p
        };
        let nblocks = nwords.div_ceil(BLOCK_WORDS);
        Self {
            words,
            nwords,
            base,
            span: if nwords == 0 { 0 } else { span },
            overflow: Mutex::new(FxHashSet::default()),
            overflow_len: AtomicUsize::new(0),
            overflow_warned: AtomicBool::new(false),
            label,
            blocks: (0..nblocks.div_ceil(64))
                .map(|_| AtomicU64::new(0))
                .collect(),
            mapped_len,
        }
    }

    /// zr z10/mt: are the words on a mapping of their own
    /// ([`Self::labelled_side_table`]) rather than an allocator block?
    pub fn on_own_mapping(&self) -> bool {
        self.mapped_len != 0
    }

    /// zt t3/rl (`zgc-zr3fp-proposal-bitmap-pages-follow-arena-granules`,
    /// step 1; `CRATONVM_ZGC_BITMAP_RELEASE_WITH_GRANULE`): give back the
    /// 4 KiB pages of this table that cover only arena bytes inside
    /// `[lo, hi)` (absolute addresses) and hold no bit. Returns the bytes
    /// discarded. Only a table on its own mapping (`on_own_mapping`, Linux)
    /// does anything; an allocator block is left alone.
    ///
    /// A page is discarded only after every word of it was read as zero, and
    /// the caller must guarantee that no bit can be set in `[lo, hi)` until
    /// it returns (ZGC: the span is free, so no marker can reach an object
    /// there, and the arena lock is held so nothing is handed out there).
    /// A run of 4 KiB pages that is not whole kernel pages is left alone
    /// (`reservation::discard_side_table_pages`, zt t4/x3). A discarded page reads
    /// zero again, which is what it held; the summary `blocks` bits are left
    /// as they are (a set bit only means "may hold bits").
    pub fn discard_zero_pages_over(&self, lo: usize, hi: usize) -> usize {
        const PAGE: usize = 4096;
        if self.mapped_len == 0 || self.nwords == 0 {
            return 0;
        }
        let lo = lo.max(self.base);
        let hi = hi.min(self.base.saturating_add(self.span));
        if hi <= lo {
            return 0;
        }
        // Word `w` covers arena `[base + w * 512, base + (w + 1) * 512)`;
        // only words wholly inside `[lo, hi)`.
        let w0 = (lo - self.base).div_ceil(512);
        let w1 = ((hi - self.base) / 512).min(self.nwords);
        if w1 <= w0 {
            return 0;
        }
        let table = self.words as usize;
        let first_page = (table + w0 * 8 + PAGE - 1) & !(PAGE - 1);
        let end_page = (table + w1 * 8) & !(PAGE - 1);
        let page_is_zero = |p: usize| -> bool {
            let w = (p - table) / 8;
            (w..w + PAGE / 8).all(|i| {
                // SAFETY: `p + PAGE <= table + w1 * 8`, so `i < w1 <= nwords`.
                unsafe { (*self.words.add(i)).load(Ordering::Acquire) == 0 }
            })
        };
        let discard = |start: usize, len: usize| -> usize {
            // SAFETY: `[start, start + len)` is inside this table's mapping,
            // every word of it was read as zero, and the caller guarantees no
            // writer there.
            let ok = unsafe { crate::reservation::discard_side_table_pages(start as *mut u8, len) };
            if ok { len } else { 0 }
        };
        let mut discarded = 0usize;
        let mut run: Option<usize> = None;
        let mut p = first_page;
        while p < end_page {
            match (page_is_zero(p), run) {
                (true, None) => run = Some(p),
                (false, Some(start)) => {
                    discarded += discard(start, p - start);
                    run = None;
                }
                _ => {}
            }
            p += PAGE;
        }
        if let Some(start) = run {
            discarded += discard(start, end_page - start);
        }
        discarded
    }

    /// Record that word `w` is about to get a bit: set its block's bit in
    /// [`Self::blocks`], BEFORE the word's own bit is set, so a reader that
    /// finds the block bit clear cannot have missed a bit it could otherwise
    /// see. A load first, because after the first object in a block every
    /// later one finds the bit already set.
    ///
    /// `w < self.nwords` is the caller's to ensure.
    #[inline]
    pub fn note_word(&self, w: usize) {
        let blk = w / BLOCK_WORDS;
        let slot = &self.blocks[blk >> 6];
        let mask = 1u64 << (blk & 63);
        if slot.load(Ordering::Acquire) & mask == 0 {
            slot.fetch_or(mask, Ordering::AcqRel);
        }
    }

    /// Note every block covering the arena addresses `[lo, hi)` (clamped to
    /// this bitmap), as [`Self::note_word`] would for a bit in each.
    ///
    /// nio21 wave 9 L3: for a word writer that does NOT go through
    /// [`Self::insert`] -- the JIT's inline start-bit store
    /// (`crate::tlab::JitZgcAnnounceTable`) writes `words` directly and has no
    /// pointer to [`Self::blocks`]. Called on the VM-TLAB chunk at carve, before
    /// any buffer over it is armed, so "the block bit is set before any of its
    /// words" holds for that writer too. One relaxed load (and at most one
    /// `fetch_or`) per 32 KiB of chunk.
    pub fn note_blocks_covering(&self, lo: usize, hi: usize) {
        let (first, last) = self.word_range(lo, hi);
        if first >= last {
            return;
        }
        for blk in (first / BLOCK_WORDS)..=((last - 1) / BLOCK_WORDS) {
            self.note_word(blk * BLOCK_WORDS);
        }
    }

    /// Is block `blk`'s summary bit set? Tests only.
    #[cfg(test)]
    pub(crate) fn block_noted(&self, blk: usize) -> bool {
        self.blocks
            .get(blk >> 6)
            .is_some_and(|s| s.load(Ordering::Acquire) & (1u64 << (blk & 63)) != 0)
    }

    /// Clear the summary bit of every noted block whose words are all zero.
    /// Returns how many it cleared.
    ///
    /// # Why (nio21 wave 9 L3)
    ///
    /// The summary bits used to be set-only, so a block a sweep emptied (a
    /// swept nursery stretch) kept its bit forever and
    /// [`Self::nearest_base_in`] walked its words four at a time on every pin
    /// probe that landed above it: up to 64 Ki steps at the 128 MiB bound,
    /// per stale stack word, per probed peer, per pause
    /// (`docs/feature-designs/generative-gc-architecture.md`).
    /// After this pass the scan skips such a block in one summary probe.
    ///
    /// # Cost
    ///
    /// One summary load per 64 blocks, and per noted block a word scan that
    /// stops at the first non-zero word -- in a populated block, the first.
    /// The full 64-word read is paid only by a block that is empty, and then
    /// once: its bit is clear afterwards and the next pass skips it.
    ///
    /// # STOP-THE-WORLD ONLY, with no word writer suspended mid-store
    ///
    /// The invariant is "a clear block bit proves the block's words are
    /// zero". An inserter that read the block bit as set (and so skipped its
    /// `fetch_or`) and then set a word bit after this pass cleared the block
    /// bit would break it -- and a broken summary makes the pin probe skip a
    /// live object. So the caller must exclude every concurrent writer of
    /// `words`, including a JIT thread OS-suspended between its inline
    /// epoch check and its store (`ZgcRealHeap::forget_empty_registry_blocks`
    /// declines the pass in a cycle that froze one). The world being stopped
    /// at polls is not enough on its own for that writer.
    pub fn forget_empty_blocks(&self) -> usize {
        let mut forgotten = 0usize;
        for (s, slot) in self.blocks.iter().enumerate() {
            let mut bits = slot.load(Ordering::Acquire);
            if bits == 0 {
                continue;
            }
            let mut clear = 0u64;
            while bits != 0 {
                let b = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let w0 = ((s << 6) | b) * BLOCK_WORDS;
                if w0 >= self.nwords {
                    continue; // never set by a bounded writer; leave it alone
                }
                let w1 = (w0 + BLOCK_WORDS).min(self.nwords);
                // SAFETY: `w0 <= w < w1 <= self.nwords`.
                let empty = (w0..w1)
                    .all(|w| (unsafe { (*self.words.add(w)).load(Ordering::Acquire) }) == 0);
                if empty {
                    clear |= 1u64 << b;
                }
            }
            if clear != 0 {
                slot.fetch_and(!clear, Ordering::AcqRel);
                forgotten += clear.count_ones() as usize;
            }
        }
        forgotten
    }

    /// May block `blk` (words `[blk * BLOCK_WORDS, (blk + 1) * BLOCK_WORDS)`,
    /// 32 KiB of arena) hold a set bit? `false` PROVES every word of the block
    /// is zero; `true` proves nothing. Out-of-range blocks answer `false`
    /// (their words read as zero through [`Self::word_at`]).
    ///
    /// zr z5/sw: on the ZGC MARK bitmap this is the per-region liveness
    /// summary the sweep reads. The summary bit is set at the claim point
    /// (`claim` -> `note_word`, before the word's `fetch_or`) and by every
    /// other word writer (`insert`, `set_range` for allocate-black), so a
    /// block whose bit is clear held no marked object this cycle, and the
    /// complement sweep frees it without reading its mark words or visiting a
    /// dead base. The bit is reset per cycle by
    /// [`Self::forget_blocks_within`] right after the mark clear.
    #[inline]
    pub fn block_may_hold_bits(&self, blk: usize) -> bool {
        self.blocks
            .get(blk >> 6)
            .is_some_and(|s| s.load(Ordering::Acquire) & (1u64 << (blk & 63)) != 0)
    }

    /// Clear the summary bit of every block lying WHOLLY inside the word
    /// ranges covering `ranges` (arena address ranges, as for
    /// [`Self::clear_within`]); `None` means every block. Returns how many
    /// set bits it cleared.
    ///
    /// zr z5/sw. Unlike [`Self::forget_empty_blocks`] this reads no word: it
    /// is the summary half of a clear that has JUST zeroed those words, and
    /// it is sound only there. **Call it only immediately after
    /// [`Self::clear_within`] / [`Self::clear_within_sparse`] over the same
    /// `ranges` (or [`Self::clear_all`] for `None`), stop-the-world, with no
    /// word writer running.** A block straddling a range edge keeps its bit:
    /// a set bit over zero words only costs a reader a word scan, while a
    /// clear bit over a set word is a live object the sweep frees.
    ///
    /// Not called on the object-start registry, whose summary also bounds the
    /// pin probe and is forgotten only by the word-verifying pass.
    pub fn forget_blocks_within(&self, ranges: Option<&[(usize, usize)]>) -> usize {
        let mut forgotten = 0usize;
        let mut clear_blocks = |blk_lo: usize, blk_hi: usize| {
            let mut blk = blk_lo;
            while blk < blk_hi {
                let s = blk >> 6;
                let lo_bit = blk & 63;
                let n = (64 - lo_bit).min(blk_hi - blk);
                let mask = if n == 64 {
                    !0u64
                } else {
                    ((1u64 << n) - 1) << lo_bit
                };
                if let Some(slot) = self.blocks.get(s) {
                    if slot.load(Ordering::Relaxed) & mask != 0 {
                        let prev = slot.fetch_and(!mask, Ordering::AcqRel);
                        forgotten += (prev & mask).count_ones() as usize;
                    }
                }
                blk += n;
            }
        };
        match ranges {
            None => clear_blocks(0, self.nwords.div_ceil(BLOCK_WORDS)),
            Some(ranges) => {
                for &(lo, hi) in ranges {
                    let (first, last) = self.word_range(lo, hi);
                    // Wholly-covered blocks only. The last block of the bitmap
                    // may be short: it is wholly covered when the range runs
                    // to `nwords`.
                    let blk_lo = first.div_ceil(BLOCK_WORDS);
                    let blk_hi = if last == self.nwords {
                        last.div_ceil(BLOCK_WORDS)
                    } else {
                        last / BLOCK_WORDS
                    };
                    if blk_lo < blk_hi {
                        clear_blocks(blk_lo, blk_hi);
                    }
                }
            }
        }
        forgotten
    }

    /// The highest block at or below `blk` whose [`Self::blocks`] bit is set,
    /// or `None`.
    #[inline]
    fn highest_noted_block_at_or_below(&self, blk: usize) -> Option<usize> {
        let mut s = blk >> 6;
        let mut word =
            self.blocks.get(s)?.load(Ordering::Acquire) & (u64::MAX >> (63 - (blk & 63)));
        loop {
            if word != 0 {
                return Some((s << 6) | (63 - word.leading_zeros() as usize));
            }
            if s == 0 {
                return None;
            }
            s -= 1;
            word = self.blocks[s].load(Ordering::Acquire);
        }
    }

    /// Word index and bit mask for `addr`, or `None` when the grid cannot
    /// encode it.
    ///
    /// The `off & 7 != 0` rejection is the whole exactness argument in one
    /// line, and it is what [`crate::young_mark::YoungMarkBits`] omits: without
    /// it an interior address would alias its object's bit and
    /// `ZgcRealHeap::is_object_address` would promote a conservative-root
    /// candidate that is not a base.
    #[inline]
    pub fn locate(&self, addr: usize) -> Option<(usize, u64)> {
        let off = addr.checked_sub(self.base)?;
        if off >= self.span || off & 7 != 0 {
            return None;
        }
        let bit = off >> 3;
        Some((bit >> 6, 1u64 << (bit & 63)))
    }

    /// Record an object start.
    ///
    /// # Why `fetch_or` and not a CAS loop
    ///
    /// The word is shared with the 63 neighbouring 8-byte grid slots, so two
    /// threads allocating adjacent objects write the same word — a
    /// `load`/`or`/`store` would drop one of them. `fetch_or` is a single
    /// atomic read-modify-write, and because this bitmap is *monotone between
    /// collections* (inserts only ever set bits; the only clears happen in the
    /// stop-the-world sweep, with no mutator inserting) there is no value to
    /// re-check and therefore nothing for a retry loop to do. That is exactly
    /// [`crate::young_mark::YoungMarkBits::try_mark`]'s argument.
    ///
    /// `Release`: this is at least as strong as the `Mutex::unlock` it
    /// replaces. A thread that hands a fresh pointer to another thread does so
    /// through a plain field store, which supplies no edge of its own; keeping
    /// the release here means the reader's `Acquire` in [`Self::contains`]
    /// still cannot observe "not an object" for a pointer whose publication it
    /// has already seen. Relaxed would be sufficient under the memory model
    /// only if the publication path itself carried the edge, and it does not.
    #[inline]
    pub fn insert(&self, addr: usize) {
        match self.locate(addr) {
            // SAFETY: `locate` bounds-checked `addr`, so `w < self.nwords`.
            Some((w, mask)) => unsafe {
                self.note_word(w);
                (*self.words.add(w)).fetch_or(mask, Ordering::Release);
            },
            None => self.spill(addr),
        }
    }

    /// The grid could not encode `addr`; keep it exactly, in the side set.
    ///
    /// zt t2/x1: only for an address [`Self::locate`] refuses. Since zt t1/tp
    /// [`Self::contains`] answers a locatable address by its bit alone, so a
    /// locatable address spilled here would read as absent.
    #[cold]
    pub fn spill(&self, addr: usize) {
        debug_assert!(
            self.locate(addr).is_none(),
            "HeapBitmap::spill of a grid-encodable address {addr:#x} (contains would miss it)"
        );
        {
            let mut overflow = self.overflow.lock();
            if overflow.insert(addr) {
                let n = overflow.len();
                self.overflow_len.store(n, Ordering::Release);
            }
        }
        if !self.overflow_warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                target: "zgc",
                addr = addr,
                base = self.base,
                span = self.span,
                label = self.label,
                "zgc bitmap: an address is off the 8-byte grid (or outside the \
                 arena) — falling back to the exact side set for it; membership \
                 stays correct, but every such address costs a lock and the \
                 bitmap is not carrying it"
            );
        }
    }

    /// Exact membership: is `addr` the base of a registered allocation?
    ///
    /// `Acquire` pairs with [`Self::insert`]'s `Release`; see there. The
    /// overflow probe is guarded by a relaxed-cost load that is zero on every
    /// healthy run, so the common answer is one aligned load and one mask.
    ///
    /// zt t1/tp (`zgc-zr4rv4-mark-pool-default-residuals-DECIDED-20261019.md` item 4):
    /// an address the grid encodes is answered by its bit alone. The overflow
    /// set only ever receives addresses [`Self::locate`] refuses (`insert` and
    /// the object-start writer spill on `None`, the claim's `None` arm inserts
    /// there), and `base` / `span` never change, so a locatable address is
    /// never in it -- the old fall-through took the overflow lock for every
    /// unmarked in-span address (the mark's `TEST_FIRST` pre-test) whenever
    /// one off-grid address had been spilled.
    #[inline]
    pub fn contains(&self, addr: usize) -> bool {
        match self.locate(addr) {
            Some((w, mask)) => {
                // SAFETY: `locate` bounds-checked `addr`, so `w < self.nwords`.
                let word = unsafe { (*self.words.add(w)).load(Ordering::Acquire) };
                word & mask != 0
            }
            None => {
                self.overflow_len.load(Ordering::Acquire) != 0
                    && self.overflow.lock().contains(&addr)
            }
        }
    }

    /// One bitmap word: the start (or mark) bits for 512 bytes of arena.
    ///
    /// The unit a bitmap-driven sweep walks in. Words outside the bitmap read
    /// as zero rather than panicking, so a caller iterating a snapshot taken
    /// at a different moment cannot walk off the end.
    #[inline]
    pub fn word_at(&self, w: usize) -> u64 {
        if w >= self.nwords {
            return 0;
        }
        // SAFETY: bounds-checked immediately above.
        unsafe { (*self.words.add(w)).load(Ordering::Acquire) }
    }

    /// Number of words this bitmap covers.
    #[inline]
    pub fn word_count(&self) -> usize {
        self.nwords
    }

    /// The arena address the low bit of word `w` describes.
    #[inline]
    pub fn word_base(&self, w: usize) -> usize {
        self.base + (w << 9)
    }

    /// Keep only the bits `marks` also has set, word by word: the bulk twin of
    /// [`Self::remove`], and what a sweep does to the registry once it
    /// knows the live set as a bitmap.
    ///
    /// One `fetch_and` per 512 arena bytes instead of one per dead object.
    /// `Release` for the same reason `remove` uses it: whatever the sweep
    /// wrote into the dead object must not be observable after its base stops
    /// being a registered allocation.
    ///
    /// Bits set in `marks` but not here are ignored -- an object allocated
    /// black after this snapshot was taken is already registered and must stay
    /// so. Addresses in the overflow set are NOT touched; they are off-grid by
    /// definition and their owner sweeps them one at a time.
    pub fn retain_marked(&self, marks: &HeapBitmap) {
        for w in 0..self.nwords {
            let keep = marks.word_at(w);
            // SAFETY: `w < self.nwords`.
            let word = unsafe { &*self.words.add(w) };
            if word.load(Ordering::Relaxed) & !keep == 0 {
                continue; // nothing to clear in this word
            }
            word.fetch_and(keep, Ordering::Release);
        }
    }

    /// [`Self::retain_marked`] over the words covering `ranges` only, with
    /// [`Self::snapshot_word_runs`]'s argument and verification: a word outside
    /// the ranges holds no start, so and-ing it with anything changes nothing.
    ///
    /// The whole-bitmap form visited `nwords` words per sweep however little
    /// was allocated: 33 million at a 16 GiB `-Xmx`, about half of a 1 MB-live
    /// collection's sweep.
    pub fn retain_marked_within(&self, marks: &HeapBitmap, ranges: &[(usize, usize)]) {
        self.debug_assert_clear_outside(ranges);
        for &(lo, hi) in ranges {
            let (first, last) = self.word_range(lo, hi);
            for w in first..last {
                let keep = marks.word_at(w);
                // SAFETY: `word_range` clamps to `nwords`.
                let word = unsafe { &*self.words.add(w) };
                if word.load(Ordering::Relaxed) & !keep == 0 {
                    continue;
                }
                word.fetch_and(keep, Ordering::Release);
            }
        }
    }

    /// [`Self::retain_marked_within`] with a plain load and store per word
    /// that changes, instead of a locked `fetch_and`.
    ///
    /// zgc pc3 (`CRATONVM_ZGC_RETAIN_PLAIN_STORES`, default ON, `=0` kill):
    /// on an all-garbage ZGC cycle nearly every word of the bumped range holds
    /// a dead start, so the prune paid one locked read-modify-write per word
    /// -- ~4 ms of a ~20 ms pause on the zgc-r21 lean row (`perf`, host,
    /// 2026-10-23). Same words, same result: every caller runs inside the
    /// pause, with no writer of these words running (the function's own
    /// contract), which is all a plain store needs
    /// ([`Self::store_word_single_writer`]'s argument).
    pub fn retain_marked_within_plain(&self, marks: &HeapBitmap, ranges: &[(usize, usize)]) {
        self.debug_assert_clear_outside(ranges);
        for &(lo, hi) in ranges {
            let (first, last) = self.word_range(lo, hi);
            for w in first..last {
                // SAFETY: `word_range` clamps to `nwords`.
                let word = unsafe { &*self.words.add(w) };
                let old = word.load(Ordering::Relaxed);
                if old == 0 {
                    continue;
                }
                let keep = marks.word_at(w);
                if old & !keep != 0 {
                    word.store(old & keep, Ordering::Release);
                }
            }
        }
    }

    /// Clear one start. Called only from the sweep's in-place prune, inside the
    /// stop-the-world region.
    ///
    /// `fetch_and` rather than `load`/`store` for [`Self::insert`]'s reason —
    /// the word is shared with 63 neighbours, and although the sweep is the
    /// only writer *of this word's bits* while the world is stopped, using the
    /// RMW costs nothing and removes the assumption. `Release` keeps the clear
    /// no weaker than the `Mutex::unlock` it replaces, so the preceding
    /// zero-fill of the dead object cannot be observed after it.
    ///
    /// (gengc-round2-alloc2, 2026-09-20: this doc block was attached to
    /// [`Self::word_at`], which then carried two and `remove` carried none —
    /// the same merge damage gengc-round1-alloc found six instances of in
    /// `arena.rs` and `tlab.rs`.)
    /// Clear the bits of `mask` in word `w` with one `fetch_and` -- the
    /// per-word twin of [`Self::remove`] for a sweep that already holds a
    /// word's dead bits (zr z2/g2, the young complement sweep, where
    /// [`Self::retain_marked_within`] cannot be used because the word range
    /// starts at the nursery floor and that function's debug check demands an
    /// empty bitmap outside its ranges). Out-of-range `w` is a no-op.
    /// Stop-the-world only, like `remove`.
    pub fn clear_word_bits(&self, w: usize, mask: u64) {
        if w >= self.nwords || mask == 0 {
            return;
        }
        // SAFETY: bounds-checked immediately above.
        unsafe { (*self.words.add(w)).fetch_and(!mask, Ordering::Release) };
    }

    /// JIT round 23 wave 8, lane W8-L3 (`CRATONVM_GEN_LAZY_MIRROR_PLAIN_STORES`):
    /// store `value` into word `w` with one plain Release store -- no locked
    /// read-modify-write. ONLY for a bitmap with a single writer at a time
    /// (the arena's lazy-dirty mirror, written under the arena lock or stop
    /// the world): with no concurrent writer, `load` then `store` leaves the
    /// value a `fetch_and` / `fetch_or` would, and the lock-free readers
    /// (`contains`, Acquire) see the old word or the new one, as before.
    /// Out-of-range `w` is a no-op.
    #[inline]
    pub fn store_word_single_writer(&self, w: usize, value: u64) {
        if w >= self.nwords {
            return;
        }
        // SAFETY: bounds-checked immediately above.
        unsafe { (*self.words.add(w)).store(value, Ordering::Release) };
    }

    /// [`Self::set_range`] for a single-writer bitmap (see
    /// [`Self::store_word_single_writer`]): the same bits and block notes,
    /// with plain stores -- `!0` into every interior word, load-or-store at
    /// the two edge words -- instead of one locked `fetch_or` per word. A
    /// non-moving young sweep publishes every reclaimed byte here, about
    /// 480 000 words per 245 MB cycle (JIT r23 w8 sampler: `set_range` was
    /// 7% of the mutator thread's samples on `R21AllocLean lean-static-long16`).
    pub fn set_range_single_writer(&self, lo: usize, hi: usize) {
        if self.nwords == 0 || hi <= lo {
            return;
        }
        let lo_off = match lo.checked_sub(self.base) {
            Some(o) if o < self.span => o,
            _ => return,
        };
        let hi_off = hi.saturating_sub(self.base).min(self.span);
        if hi_off <= lo_off {
            return;
        }
        // The slot arithmetic is `set_range`'s, verbatim.
        let first = lo_off.div_ceil(8);
        let last = hi_off.div_ceil(8); // exclusive
        if last <= first {
            return;
        }
        let (w0, b0) = (first >> 6, first & 63);
        let (w1, b1) = (last >> 6, last & 63);
        let w_last = if b1 != 0 { w1 } else { w1 - 1 }.min(self.nwords - 1);
        for blk in (w0 / BLOCK_WORDS)..=(w_last / BLOCK_WORDS) {
            self.note_word(blk * BLOCK_WORDS);
        }
        let or_word = |w: usize, mask: u64| {
            let v = self.word_at(w);
            if v | mask != v {
                self.store_word_single_writer(w, v | mask);
            }
        };
        if w0 == w1 {
            or_word(w0, (!0u64 << b0) & ((1u64 << b1) - 1));
            return;
        }
        or_word(w0, !0u64 << b0);
        for w in (w0 + 1)..w1.min(self.nwords) {
            self.store_word_single_writer(w, !0u64);
        }
        if b1 != 0 && w1 < self.nwords {
            or_word(w1, (1u64 << b1) - 1);
        }
    }

    pub fn remove(&self, addr: usize) {
        if let Some((w, mask)) = self.locate(addr) {
            // SAFETY: `locate` bounds-checked `addr`, so `w < self.nwords`.
            let prev = unsafe { (*self.words.add(w)).fetch_and(!mask, Ordering::Release) };
            if prev & mask != 0 {
                return;
            }
        }
        if self.overflow_len.load(Ordering::Acquire) != 0 {
            let mut overflow = self.overflow.lock();
            if overflow.remove(&addr) {
                let n = overflow.len();
                self.overflow_len.store(n, Ordering::Release);
            }
        }
    }

    /// Set the bit for `addr` and report whether **this call** set it.
    ///
    /// The exactly-once primitive the concurrent marker needs, and the reason
    /// the mark bits can leave the object header at all.
    /// [`ObjectHeader::try_add_gc_flags`] is a CAS loop on a byte that shares
    /// its word with `gc_age` and the sticky layout flags; this is one
    /// `fetch_or` on a word that carries nothing but mark bits, so there is no
    /// value to re-check and nothing for a retry loop to do.
    ///
    /// The caller's contract is `try_mark`'s verbatim: **`true` means you now
    /// own the obligation to scan it.** Two `true`s for one object would be a
    /// double scan (wasteful); a lost update would leave an object marked and
    /// never traced, so its children are swept while it is alive and pointing
    /// at them. `fetch_or` returns the previous word, which is exactly the
    /// evidence needed to tell the two apart.
    ///
    /// `AcqRel`: the `Release` half publishes whatever the marker wrote before
    /// claiming (nothing today, but the ordering must not depend on that), and
    /// the `Acquire` half pairs with a peer's claim so a worker that loses the
    /// race AT THE `fetch_or` sees everything the winner did before it.
    /// gen r5w5/old9: not a loser on the relaxed pre-test below, which
    /// returns `false` without any acquire; nothing depends on that edge
    /// today (a loser only skips the object).
    ///
    /// Off-grid addresses go to the exact side set, where "did I insert it"
    /// is `FxHashSet::insert`'s own answer.
    #[inline]
    pub fn claim(&self, addr: usize) -> bool {
        match self.locate(addr) {
            // SAFETY: `locate` bounds-checked `addr`, so `w < self.nwords`.
            Some((w, mask)) => {
                let word = unsafe { &*self.words.add(w) };
                // TEST BEFORE THE READ-MODIFY-WRITE. This is asked of every
                // EDGE, not every object, and most edges point at something
                // already marked -- a shared graph is why marking is a
                // traversal and not a walk. An unconditional `fetch_or` makes
                // each of those an exclusive-state acquisition of a cache line
                // every other worker is also writing; a plain load answers the
                // same question from a shared one.
                //
                // The RMW is still the arbiter, and this only skips the races
                // it would have lost: nothing clears a mark bit during a mark,
                // so "already set" is a stable answer, while "not set yet"
                // falls through and contends for the claim exactly as before.
                if word.load(Ordering::Relaxed) & mask != 0 {
                    return false;
                }
                self.note_word(w);
                let prev = word.fetch_or(mask, Ordering::AcqRel);
                prev & mask == 0
            }
            None => {
                let mut overflow = self.overflow.lock();
                let inserted = overflow.insert(addr);
                if inserted {
                    let n = overflow.len();
                    self.overflow_len.store(n, Ordering::Release);
                }
                inserted
            }
        }
    }

    /// Set every bit covering `[lo, hi)`, in one pass over the words.
    ///
    /// The bulk twin of [`Self::insert`], and the reason allocate-black can be
    /// a per-CHUNK operation instead of a per-OBJECT one. A 512 KiB TLAB chunk
    /// of 72-byte objects is ~7 000 objects and therefore ~7 000 `fetch_or`s on
    /// the allocation fast path; the same span here is 1 024 bitmap words, i.e.
    /// one `fetch_or` per 512 arena bytes whatever the object size.
    ///
    /// `fetch_or` and not a plain store even for the interior words: a
    /// concurrent marker may be claiming bits in the same words for objects
    /// that are not in this range, and a read-modify-write is the only thing
    /// that does not drop its claim. Setting a bit that is already set is a
    /// no-op, so there is no value to re-check and nothing for a retry loop to
    /// do -- the same argument as [`Self::insert`]'s.
    ///
    /// Addresses the grid cannot encode are NOT spilled: an interior address of
    /// the range is not an object base, so it can never be queried, and the
    /// only bases inside the range are 8-aligned by construction. The range
    /// itself is clamped to the covered span rather than spilling its ends.
    pub fn set_range(&self, lo: usize, hi: usize) {
        if self.nwords == 0 || hi <= lo {
            return;
        }
        let lo_off = match lo.checked_sub(self.base) {
            Some(o) if o < self.span => o,
            _ => return,
        };
        let hi_off = hi.saturating_sub(self.base).min(self.span);
        if hi_off <= lo_off {
            return;
        }
        // Bit indices: every 8-byte grid slot whose BASE lies in `[lo, hi)` --
        // the first slot at or above `lo`, up to (exclusive) the first slot at
        // or above `hi`. `lo` and `hi` are 8-aligned at every caller (a chunk
        // base or a TLAB cursor), so both roundings are no-ops there.
        //
        // gen r5w5/old9: `last` used to be `hi_off / 8`, which rounds an
        // unaligned `hi` DOWN and so dropped the slot at `floor8(hi)` although
        // its base lies inside the range. For allocate-black that is the
        // unsafe direction (an object allocated there would read unmarked);
        // the old comment's "can only decline to set a bit" called it
        // conservative, and for this bitmap declining is not. Rounding up
        // stays inside the bitmap: `hi_off <= span`, so `last <=
        // span.div_ceil(8)`, the number of slots.
        let first = lo_off.div_ceil(8);
        let last = hi_off.div_ceil(8); // exclusive
        if last <= first {
            return;
        }
        let (w0, b0) = (first >> 6, first & 63);
        let (w1, b1) = (last >> 6, last & 63);
        // Every word that may get a bit, `w0` through `w1` when `b1 != 0`.
        let w_last = if b1 != 0 { w1 } else { w1 - 1 }.min(self.nwords - 1);
        for blk in (w0 / BLOCK_WORDS)..=(w_last / BLOCK_WORDS) {
            self.note_word(blk * BLOCK_WORDS);
        }
        if w0 == w1 {
            // `last > first` and both land in one word, so `0 <= b0 < b1 <= 63`
            // and the shift below cannot overflow.
            let mask = (!0u64 << b0) & ((1u64 << b1) - 1);
            // SAFETY: `first < last <= span.div_ceil(8)`, so `w0 < nwords`.
            unsafe { (*self.words.add(w0)).fetch_or(mask, Ordering::Release) };
            return;
        }
        // SAFETY for all three: every index is derived from an offset bounded
        // by `span`, and `nwords` covers `span.div_ceil(8).div_ceil(64)`.
        unsafe {
            (*self.words.add(w0)).fetch_or(!0u64 << b0, Ordering::Release);
            for w in (w0 + 1)..w1.min(self.nwords) {
                (*self.words.add(w)).fetch_or(!0u64, Ordering::Release);
            }
            if b1 != 0 && w1 < self.nwords {
                (*self.words.add(w1)).fetch_or((1u64 << b1) - 1, Ordering::Release);
            }
        }
    }

    /// The word range covering the arena addresses `[lo, hi)`, clamped to this
    /// bitmap. Half-open, and the end rounds UP so a partially covered word is
    /// included.
    #[inline]
    pub fn word_range(&self, lo: usize, hi: usize) -> (usize, usize) {
        let end = self.base.saturating_add(self.span);
        let lo = lo.clamp(self.base, end);
        let hi = hi.clamp(lo, end);
        let first = (lo - self.base) >> 9;
        let last = ((hi - self.base) + 511) >> 9;
        (first.min(self.nwords), last.min(self.nwords))
    }

    /// Copy only the words covering `ranges`, leaving every other word zero.
    ///
    /// # Why a bitmap pass may skip most of the heap
    ///
    /// This bitmap is sized by CAPACITY -- one bit per 8 arena bytes, so
    /// `-Xmx / 512` bytes of words -- and the snapshot the mark phase takes was
    /// copying all of it, once per collection. At `-Xmx4g` that is 8.4 million
    /// atomic loads into a fresh 64 MiB `Vec`, and it is paid whether the heap
    /// holds ten objects or ten million.
    ///
    /// Measured on `G1ChurnPauseProbe 50 1800`, comparing cycles at the SAME
    /// registered-object count so the only variable is the heap flag:
    /// `snapshot_us` 11-13 ms at `-Xmx2048m` against 21-23 ms at `-Xmx4096m`.
    /// Exactly the ratio of the two capacities, and the reason a pause target
    /// that holds at 2 GiB is unreachable at 4 GiB: this is a floor no
    /// allocation budget can lower.
    ///
    /// The arena is TWO-ENDED. Small objects and TLAB chunks bump up from
    /// `base`, allocations at or above `ZGC_LARGE_OBJECT_MIN` bump down from
    /// capacity, and the middle between the two cursors has never been handed
    /// out -- so no object starts there and no bit in it can be set. Copying it
    /// transfers zeroes. `ranges` is the caller's statement of where the two
    /// ends currently are; everything else is skipped.
    ///
    /// The result is NOT full length any more (2026-10-07): it is one
    /// `(first word, words)` run per merged range, and a consumer indexes it by
    /// the same absolute word index as before (`ZSnapWords` in
    /// `zgc/starts.rs`). The full-length copy this replaced assumed
    /// `vec![0u64; nwords]` "takes a zero-page mapping rather than memsetting",
    /// which is a Linux `calloc` property, not an allocator guarantee: under the
    /// launcher's mimalloc on Windows the default `-Xmx` (a quarter of a 64 GiB
    /// machine) made it a 267 MB zeroed allocation, about 30 ms, taken twice per
    /// collection on a heap with 1 MB live -- the floor every `System.gc()` paid
    /// (`docs/feature-designs/generative-gc-architecture.md`).
    ///
    /// # The invariant this rests on
    ///
    /// "Nothing is allocated in the middle" is the whole argument, and a
    /// caller that passes wrong bounds silently loses objects -- the snapshot
    /// is the mark phase's oracle, so a base missing from it is a live object
    /// the marker will not visit. Debug builds therefore VERIFY it rather than
    /// trusting it: [`Self::debug_assert_clear_outside`] walks the skipped
    /// words and asserts every one is zero.
    ///
    /// `ranges` are arena ADDRESS ranges, as for [`Self::clear_within`]; the
    /// runs come back ascending and disjoint (overlapping or touching word
    /// ranges are merged), and empty ranges produce no run.
    pub fn snapshot_word_runs(&self, ranges: &[(usize, usize)]) -> Vec<(usize, Vec<u64>)> {
        let runs = self
            .merged_word_spans(ranges)
            .into_iter()
            .map(|(first, last)| {
                let words = (first..last)
                    // SAFETY: `word_range` clamps to `nwords`.
                    .map(|w| unsafe { (*self.words.add(w)).load(Ordering::Acquire) })
                    .collect();
                (first, words)
            })
            .collect();
        self.debug_assert_clear_outside(ranges);
        runs
    }

    /// The word spans covering `ranges`: ascending, disjoint, overlapping or
    /// touching spans merged, empty ones dropped. The grid
    /// [`Self::snapshot_word_runs`] copies and [`Self::bases_within`] walks.
    fn merged_word_spans(&self, ranges: &[(usize, usize)]) -> Vec<(usize, usize)> {
        let mut spans: Vec<(usize, usize)> = ranges
            .iter()
            .map(|&(lo, hi)| self.word_range(lo, hi))
            .filter(|&(a, b)| a < b)
            .collect();
        spans.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
        for (a, b) in spans {
            match merged.last_mut() {
                Some(last) if a <= last.1 => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        merged
    }

    /// Every base [`Self::snapshot_word_runs`] over `ranges` plus the spill set
    /// would name, as one list -- the words covering `ranges` ascending, then
    /// the spill, sorted when the spill is non-empty -- WITHOUT the
    /// intermediate copy, and allocated once at its exact length.
    ///
    /// zt t26/pc1 (`CRATONVM_ZGC_RELOCATE_LIVE_DIRECT`): the slide's live list
    /// was `snapshot_within(bounds).bases()`, i.e. a copy of every covered
    /// bitmap word into fresh runs, then a `Vec` of bases grown by doubling
    /// from empty -- about twenty reallocations and twice the final bytes
    /// faulted in for a million survivors, all inside the pause. This reads
    /// each covered word twice (a popcount to size the list, then the
    /// extraction) and allocates once. Same words, same order, same list.
    ///
    /// **STOP-THE-WORLD ONLY**: the two passes must see the same words. (A
    /// word that changed between them would only grow the list past its
    /// reservation, never lose a base, but no caller relies on that.)
    pub fn bases_within(&self, ranges: &[(usize, usize)]) -> Vec<usize> {
        let merged = self.merged_word_spans(ranges);
        let mut count = 0usize;
        for &(first, last) in &merged {
            for w in first..last {
                // SAFETY: `word_range` clamps to `nwords`.
                count +=
                    unsafe { (*self.words.add(w)).load(Ordering::Acquire) }.count_ones() as usize;
            }
        }
        let spill: Vec<usize> = if self.has_spill() {
            self.overflow.lock().iter().copied().collect()
        } else {
            Vec::new()
        };
        let mut out: Vec<usize> = Vec::with_capacity(count + spill.len());
        for &(first, last) in &merged {
            for w in first..last {
                // SAFETY: `word_range` clamps to `nwords`.
                let mut word = unsafe { (*self.words.add(w)).load(Ordering::Acquire) };
                while word != 0 {
                    let b = word.trailing_zeros() as usize;
                    word &= word - 1;
                    out.push(self.base + ((w * 64 + b) << 3));
                }
            }
        }
        let spilled = !spill.is_empty();
        out.extend(spill);
        if spilled {
            out.sort_unstable();
        }
        self.debug_assert_clear_outside(ranges);
        out
    }

    /// The bases in `[lo, hi)`, ascending: a QUERY of one span, where
    /// [`Self::bases_within`] is a bounded PASS over the whole set (its ranges
    /// must cover every set bit, which its debug check asserts, and it appends
    /// the whole spill set). zt t34: R6's region queries asked `bases_within`
    /// for one region and tripped that check in every debug build
    /// (`zt32_the_plan_copies_lazily_and_the_ring_survives`); in release they
    /// also read every spilled base as inside the region. **STOP-THE-WORLD
    /// ONLY**, like [`Self::bases_within`].
    pub fn bases_in_span(&self, lo: usize, hi: usize) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::new();
        if hi <= lo {
            return out;
        }
        let (first, last) = self.word_range(lo, hi);
        for w in first..last {
            // SAFETY: `word_range` clamps to `nwords`.
            let mut word = unsafe { (*self.words.add(w)).load(Ordering::Acquire) };
            while word != 0 {
                let b = word.trailing_zeros() as usize;
                word &= word - 1;
                let addr = self.base + ((w * 64 + b) << 3);
                if (lo..hi).contains(&addr) {
                    out.push(addr);
                }
            }
        }
        if self.has_spill() {
            let before = out.len();
            out.extend(
                self.overflow
                    .lock()
                    .iter()
                    .copied()
                    .filter(|a| (lo..hi).contains(a)),
            );
            if out.len() != before {
                out.sort_unstable();
            }
        }
        out
    }

    /// [`Self::bases_within`] that skips every summary block whose bit is
    /// clear ([`Self::block_may_hold_bits`]: a clear bit proves the block's
    /// 64 words are zero, the invariant the pin probe already relies on).
    /// Same words read where any may hold a base, same order, same list.
    ///
    /// zgc pc3 (`CRATONVM_ZGC_RELOCATE_LIVE_SUMMARY`, default ON, `=0` kill):
    /// after the sweep has pruned the registry and forgotten its empty blocks,
    /// the slide's live list streamed every covered word twice to find the
    /// survivors: ~820 k words for 23 k survivors, 3-4 ms of a ~20 ms pause on
    /// the zgc-r21 lean row. With the summary it reads one bit per empty
    /// block. **STOP-THE-WORLD ONLY**, like [`Self::bases_within`].
    pub fn bases_within_summarised(&self, ranges: &[(usize, usize)]) -> Vec<usize> {
        let merged = self.merged_word_spans(ranges);
        let walk = |f: &mut dyn FnMut(usize, u64)| {
            for &(first, last) in &merged {
                let mut w = first;
                while w < last {
                    let blk = w / BLOCK_WORDS;
                    let end = ((blk + 1) * BLOCK_WORDS).min(last);
                    if !self.block_may_hold_bits(blk) {
                        w = end;
                        continue;
                    }
                    while w < end {
                        // SAFETY: `word_range` clamps to `nwords`.
                        let word = unsafe { (*self.words.add(w)).load(Ordering::Acquire) };
                        if word != 0 {
                            f(w, word);
                        }
                        w += 1;
                    }
                }
            }
        };
        let mut count = 0usize;
        walk(&mut |_, word| count += word.count_ones() as usize);
        let spill: Vec<usize> = if self.has_spill() {
            self.overflow.lock().iter().copied().collect()
        } else {
            Vec::new()
        };
        let mut out: Vec<usize> = Vec::with_capacity(count + spill.len());
        walk(&mut |w, mut word| {
            while word != 0 {
                let b = word.trailing_zeros() as usize;
                word &= word - 1;
                out.push(self.base + ((w * 64 + b) << 3));
            }
        });
        let spilled = !spill.is_empty();
        out.extend(spill);
        if spilled {
            out.sort_unstable();
        }
        self.debug_assert_clear_outside(ranges);
        out
    }

    /// Clear only the words covering `ranges`. The twin of
    /// [`Self::snapshot_word_runs`], with the same argument and the same
    /// verification: a word outside the ranges is already zero, so storing a
    /// zero into it is work that buys nothing.
    ///
    /// **STOP-THE-WORLD ONLY**, for the reason [`Self::clear_all`] gives.
    pub fn clear_within(&self, ranges: &[(usize, usize)]) {
        self.debug_assert_clear_outside(ranges);
        for &(lo, hi) in ranges {
            let (first, last) = self.word_range(lo, hi);
            for w in first..last {
                // SAFETY: `word_range` clamps to `nwords`.
                unsafe { (*self.words.add(w)).store(0, Ordering::Relaxed) };
            }
        }
        if self.overflow_len.load(Ordering::Acquire) != 0 {
            self.overflow.lock().clear();
            self.overflow_len.store(0, Ordering::Release);
        }
        std::sync::atomic::fence(Ordering::Release);
    }

    /// [`Self::clear_within`] that stores only into words that are NOT already
    /// zero, and returns how many words it skipped (zr z3/fp).
    ///
    /// # Why (resident memory, not time)
    ///
    /// The words are an `alloc_zeroed` block, so a word nobody ever wrote sits
    /// on an untouched page that the OS has not backed. `clear_within` stores a
    /// zero into every word of the bounded ranges once per cycle, and a STORE
    /// faults a private page in even when it writes the value already there:
    /// after the first collection the whole bounded span of this bitmap is
    /// resident -- 1/64 of every arena byte the cursors ever passed -- although
    /// only the words over live objects were ever set. Testing first leaves the
    /// pages over garbage and holes untouched, and a load of an untouched page
    /// maps the shared zero page, which is not counted as resident.
    ///
    /// Same final state as `clear_within`, under the same contract
    /// (**STOP-THE-WORLD ONLY**): with no concurrent writer, "store zero where
    /// non-zero" and "store zero everywhere" leave identical words.
    pub fn clear_within_sparse(&self, ranges: &[(usize, usize)]) -> u64 {
        self.debug_assert_clear_outside(ranges);
        let mut skipped = 0u64;
        for &(lo, hi) in ranges {
            let (first, last) = self.word_range(lo, hi);
            for w in first..last {
                // SAFETY: `word_range` clamps to `nwords`.
                let word = unsafe { &*self.words.add(w) };
                if word.load(Ordering::Relaxed) == 0 {
                    skipped += 1;
                } else {
                    word.store(0, Ordering::Relaxed);
                }
            }
        }
        if self.overflow_len.load(Ordering::Acquire) != 0 {
            self.overflow.lock().clear();
            self.overflow_len.store(0, Ordering::Release);
        }
        std::sync::atomic::fence(Ordering::Release);
        skipped
    }

    /// Bytes of address space the words occupy: `nwords * 8`, i.e. 1/64 of the
    /// covered span. What the bitmap CAN cost, not what it does (zr z3/fp).
    pub fn reserved_bytes(&self) -> usize {
        self.nwords * std::mem::size_of::<AtomicU64>()
    }

    /// Bytes of word pages that have had a bit SET through a noting writer,
    /// estimated from [`Self::blocks`] (zr z3/fp): one block is 64 words =
    /// 512 bytes, so a byte of a summary word covers 8 blocks = 4 KiB of words,
    /// one OS page. Counts each such page once.
    ///
    /// An estimate of the resident part, in both directions: a page whose words
    /// were only ever STORED zero (a non-sparse clear) is resident and not
    /// counted, and a block [`Self::forget_empty_blocks`] has forgotten is
    /// still resident and not counted either. Diagnostics only (the summary's
    /// `zgc-footprint` line); takes no lock.
    pub fn noted_page_bytes(&self) -> usize {
        let mut pages = 0usize;
        for slot in self.blocks.iter() {
            let bits = slot.load(Ordering::Relaxed);
            if bits == 0 {
                continue;
            }
            pages += bits.to_le_bytes().iter().filter(|b| **b != 0).count();
        }
        (pages * 4096).min(self.reserved_bytes().next_multiple_of(4096))
    }

    /// Every word OUTSIDE `ranges` must be zero. Debug builds only: it is the
    /// O(capacity) walk the bounded passes exist to avoid, so running it in
    /// release would give back exactly what they saved.
    ///
    /// This is the one check that can catch a caller whose bounds are wrong,
    /// and the failure it prevents is silent: the snapshot is the mark phase's
    /// oracle, so a base outside the bounds is a live object the marker never
    /// visits and the sweep then frees.
    #[inline]
    pub fn debug_assert_clear_outside(&self, ranges: &[(usize, usize)]) {
        #[cfg(debug_assertions)]
        {
            let mut covered = vec![false; self.nwords];
            for &(lo, hi) in ranges {
                let (first, last) = self.word_range(lo, hi);
                for c in covered.iter_mut().take(last).skip(first) {
                    *c = true;
                }
            }
            for (w, c) in covered.iter().enumerate() {
                if *c {
                    continue;
                }
                // SAFETY: `w < nwords`.
                let bits = unsafe { (*self.words.add(w)).load(Ordering::Acquire) };
                assert_eq!(
                    bits,
                    0,
                    "{}: word {w} (arena {:#x}..{:#x}) is outside the live bounds \
                     {ranges:?} but holds start bits -- a bounded bitmap pass \
                     would drop them, and for the object-start registry that is \
                     a live object the marker never visits",
                    self.label,
                    self.word_base(w),
                    self.word_base(w) + 512,
                );
            }
        }
        #[cfg(not(debug_assertions))]
        let _ = ranges;
    }

    /// Clear **every** bit, in one pass over the words.
    ///
    /// # Why this is the whole argument for a side bitmap
    ///
    /// The state it replaces is "walk the registry and clear
    /// [`GC_FLAG_MARKED`] in every object's header". That walk is one
    /// read-modify-write per object, scattered over the whole arena, each one
    /// dirtying a 64-byte line that has to be written back — and the 2026-08-17
    /// pause anatomy measured it at **94-96% of the mark-start pause** (34 ms
    /// of 35 on a 4.6M-entry registry, 66 of 69 on a 10.8M one). Here the same
    /// operation is a sequential store over `span / 512` bytes: 8 MB for a
    /// 4.2 GB heap, touched linearly, with no dependency on how many objects
    /// the heap holds.
    ///
    /// **STOP-THE-WORLD ONLY.** `Relaxed` stores plus one trailing `Release`
    /// fence rather than a released store per word: with every mutator parked
    /// there is no concurrent reader to order against word by word, and the
    /// single fence is what the resumed mutators synchronise with. A caller
    /// that runs this with the world alive would erase claims out from under
    /// the marker, which is a use-after-free, not a torn read -- so the
    /// ordering is not what protects it and the caller's safepoint is.
    pub fn clear_all(&self) {
        for w in 0..self.nwords {
            // SAFETY: `w < self.nwords`, and `words` holds exactly that many.
            unsafe { (*self.words.add(w)).store(0, Ordering::Relaxed) };
        }
        if self.overflow_len.load(Ordering::Acquire) != 0 {
            self.overflow.lock().clear();
            self.overflow_len.store(0, Ordering::Release);
        }
        std::sync::atomic::fence(Ordering::Release);
    }

    /// [`Self::clear_all`] for an owner that holds the bitmap EXCLUSIVELY: one
    /// `memset` over the words instead of a relaxed store per word.
    ///
    /// # Why `&mut self` is the witness (gc-common w3-e, 2026-09-23)
    ///
    /// `common-g-proposal-bitmap-bulk-clear`: the atomic stores cannot be
    /// merged or vectorised, so a populated clear pays one scalar store per
    /// 512 bytes of heap. A non-atomic bulk write is sound exactly when nothing
    /// else can access the words at the same time, and the proposal suggested
    /// proving that with a `StopTheWorldToken`. That witness does not fit every
    /// caller: `ConcurrentMarker::finish_sweep` clears outside any pause (see
    /// `MarkBitmap::clear`), and a token says nothing about a bitmap another
    /// thread holds a reference to anyway. `&mut self` says precisely the right
    /// thing -- no other reference to this bitmap exists for the duration of
    /// the call -- and every `words` access in the tree goes through `&self`
    /// methods of this type. Callers that only have `&self` (an `Arc`-shared
    /// marker, ZGC's per-heap bitmaps) keep [`Self::clear_all`].
    pub fn clear_all_exclusive(&mut self) {
        if self.nwords != 0 {
            // SAFETY: `words` is this value's own `nwords`-long allocation
            // (non-dangling because `nwords != 0`), `&mut self` excludes every
            // concurrent access, and all-zero is a valid `AtomicU64`.
            unsafe { std::ptr::write_bytes(self.words, 0, self.nwords) };
        }
        self.overflow.get_mut().clear();
        *self.overflow_len.get_mut() = 0;
    }

    /// [`Self::clear_within`] under the same exclusive-access witness as
    /// [`Self::clear_all_exclusive`]: one `memset` per range.
    pub fn clear_within_exclusive(&mut self, ranges: &[(usize, usize)]) {
        self.debug_assert_clear_outside(ranges);
        for &(lo, hi) in ranges {
            let (first, last) = self.word_range(lo, hi);
            if first < last {
                // SAFETY: `word_range` clamps to `nwords`, so
                // `[first, last)` lies inside this value's allocation;
                // `&mut self` excludes every concurrent access.
                unsafe { std::ptr::write_bytes(self.words.add(first), 0, last - first) };
            }
        }
        self.overflow.get_mut().clear();
        *self.overflow_len.get_mut() = 0;
    }

    /// Is anything held outside the grid? See [`Self::overflow`].
    #[inline]
    pub fn has_spill(&self) -> bool {
        self.overflow_len.load(Ordering::Acquire) != 0
    }

    /// The greatest recorded start at or below `addr`, or `None`.
    ///
    /// This is the whole of interior-pointer resolution. Allocations do not
    /// overlap, so the ONLY base whose extent can contain `addr` is the
    /// greatest one `<= addr`: find it, deref one header, compare one extent.
    /// A forward walk reaches the same candidate — after visiting every base
    /// below it and dereferencing every one of their headers.
    ///
    /// WHY THAT MATTERED. `ZgcRealHeap::is_heap_addr` is not a GC-only path:
    /// `VmHeap::is_heap_addr`'s ZGC arm feeds it per-slot conservative root
    /// scanning over ambiguous JVM-long-vs-jobject operand words, so every
    /// interior pointer and every long bit pattern that happens to land inside
    /// the arena envelope bought a full walk of the object-start bitmap up to
    /// the arena's high-water mark. On a heap whose cursor has reached capacity
    /// that is the entire bitmap — ~3M word loads plus a header dereference per
    /// set bit, per probe. Measured with `perf record` on
    /// `type.temporal.InstantTests` (2026-08-11, Azure Linux, default
    /// collector): **80.7% of all CPU samples in `VmHeap::is_heap_addr`**, with
    /// another 11.5% in `object_body_size` — the header deref inside that walk.
    /// The class takes 6.7 s on real HotSpot and had not finished in 1800 s.
    ///
    /// Backwards, the scan stops at the first set bit below `addr`, which in a
    /// populated heap is a handful of words away. It is never worse in shape
    /// than the forward walk it replaces (both are bounded by the bitmap), and
    /// it dereferences exactly one header instead of one per live object.
    ///
    /// The `overflow` set is deliberately NOT consulted here — it carries no
    /// ordering, so "greatest at or below" is not a question it can answer. The
    /// caller keeps the full walk for that arm; see [`Self::has_spill`].
    pub fn nearest_base_at_or_below(&self, addr: usize) -> Option<usize> {
        self.nearest_base_in(addr, 0)
    }

    /// [`Self::nearest_base_at_or_below`], but only a base at or above
    /// `floor`: `None` when the greatest recorded start `<= addr` is below
    /// `floor` (or there is none). Same answer as
    /// `nearest_base_at_or_below(addr).filter(|&b| b >= floor)`, with the scan
    /// stopped at `floor`'s word instead of running on to the bottom of the
    /// bitmap. `floor == 0` is exactly [`Self::nearest_base_at_or_below`].
    ///
    /// nio20 wave 6, lane L2 (`heapcb`): the conservative pin probe
    /// (`ZgcRealHeap::resolve_interior_for_pin`) was 22% of the row's profile.
    /// Its callers pass `addr - <largest extent any object can have>`: a base
    /// further down cannot contain `addr`, so stopping there changes no
    /// answer, while a stale word in a long empty stretch (the never-bumped
    /// middle of a large `-Xmx` arena, a swept nursery) no longer costs one
    /// load per 512 empty bytes all the way down. Empty words are also
    /// skipped four at a time (one OR of four loads per 2 KiB).
    pub fn nearest_base_in(&self, addr: usize, floor: usize) -> Option<usize> {
        if self.span == 0 || self.nwords == 0 {
            return None;
        }
        let off = addr.checked_sub(self.base)?;
        // Past the covered span: the last gridded slot is still the greatest
        // candidate at or below `addr`.
        let off = off.min(self.span - 1);
        let bit = off >> 3; // floor onto the 8-byte grid
        let mut w = bit >> 6;
        debug_assert!(
            w < self.nwords,
            "bit index is derived from a bounded offset"
        );
        // The lowest word that can hold a start `>= floor`. `floor <= addr`
        // is not required: a floor above `addr` clamps to `w` and the result
        // test below refuses whatever that word holds.
        let floor_word = (floor.saturating_sub(self.base).min(self.span - 1) >> 3 >> 6).min(w);
        // Keep only bits at or below `bit` in the first word. `u64::MAX >> k`
        // has its low `64 - k` bits set, so `k = 63 - (bit & 63)` leaves
        // exactly bits `0..=(bit & 63)`.
        let k = 63 - (bit & 63);
        // SAFETY: `w < self.nwords`, checked above.
        let mut word = unsafe { (*self.words.add(w)).load(Ordering::Acquire) } & (u64::MAX >> k);
        loop {
            if word != 0 {
                let b = 63 - word.leading_zeros() as usize;
                let found = self.base + (((w << 6) | b) << 3);
                // Every other candidate is lower still.
                return (found >= floor).then_some(found);
            }
            // Invariant: no start at or below `addr` in words `w..`, and
            // `w >= floor_word`.
            if w <= floor_word {
                return None;
            }
            // Skip empty words four at a time while four remain at or above
            // `floor_word` AND inside `w`'s own block (`w % BLOCK_WORDS >= 4`
            // puts `w - 4 .. w` in it). Stopping at the block edge hands the
            // crossing to the single step below, whose block-summary check
            // skips every never-noted block in one probe; before nio21 the
            // four-word loop ran straight through block edges, so that check
            // never fired and a stale word in the never-bumped middle walked
            // every word down to the floor (64 Ki steps at the 128 MiB bound).
            while w >= floor_word + 4 && w % BLOCK_WORDS >= 4 {
                // SAFETY: `w - 4 >= floor_word >= 0` and `w < self.nwords`.
                let any = unsafe {
                    (*self.words.add(w - 1)).load(Ordering::Acquire)
                        | (*self.words.add(w - 2)).load(Ordering::Acquire)
                        | (*self.words.add(w - 3)).load(Ordering::Acquire)
                        | (*self.words.add(w - 4)).load(Ordering::Acquire)
                };
                if any != 0 {
                    break;
                }
                w -= 4;
            }
            // `w == floor_word` here means the block loop just proved
            // `floor_word` itself empty.
            if w <= floor_word {
                return None;
            }
            w -= 1;
            // ENTERING A LOWER BLOCK: skip every block nothing was ever set in
            // (2026-10-07). The bitmap is capacity-sized and the arena bumps
            // from its two ends, so an address in the never-bumped middle --
            // any stack word or `long` that happens to fall inside the arena
            // envelope -- walked down every word of the middle to the low
            // end's last object: up to 33 million loads (~16 ms) per probe at a
            // 16 GiB `-Xmx`, and the helper-window pass probes every word of
            // every blocked thread's stack in each pause.
            if w % BLOCK_WORDS == BLOCK_WORDS - 1 {
                let blk = w / BLOCK_WORDS;
                match self.highest_noted_block_at_or_below(blk) {
                    None => return None,
                    Some(lower) if lower < blk => {
                        w = lower * BLOCK_WORDS + (BLOCK_WORDS - 1);
                        // Every word of a block wholly below `floor_word`
                        // holds only starts below `floor`.
                        if w < floor_word {
                            return None;
                        }
                    }
                    Some(_) => {}
                }
            }
            // SAFETY: `w` only decreases from a value `< self.nwords`.
            word = unsafe { (*self.words.add(w)).load(Ordering::Acquire) };
        }
    }

    /// Visit every set start, ASCENDING. `f` returns `false` to stop early.
    ///
    /// `end_hint` is a PERFORMANCE BOUND, not a filter: every base strictly
    /// below it is guaranteed to be visited, bases at or above it MAY be. Pass
    /// `usize::MAX` for a full walk. It exists because the walk cost of a
    /// bitmap and of a hash set have opposite shapes — the set was O(live), the
    /// bitmap is O(arena span), so on a mostly-empty multi-gigabyte heap an
    /// unbounded walk would be a *regression* (~8.2M word loads for a 4.2 GB
    /// arena against a few thousand hash steps). The one caller that walks from
    /// the mutator path, `ZgcRealHeap::is_heap_addr`, has the arena's
    /// high-water mark available and passes it, which restores the O(bytes
    /// actually allocated) shape.
    ///
    /// `Acquire` on each word load, matching [`Self::contains`]. These walks
    /// are off the allocation path, so the per-word ordering is not worth
    /// trading for a fence.
    pub fn for_each_base(&self, end_hint: usize, f: &mut dyn FnMut(usize) -> bool) {
        // Bytes of the covered span that can hold a base below `end_hint`,
        // rounded UP to a whole word: over-approximating is what makes the
        // parameter a hint rather than a filter.
        let reach = end_hint.saturating_sub(self.base).min(self.span);
        let limit = reach.div_ceil(8).div_ceil(64).min(self.nwords);
        for w in 0..limit {
            // SAFETY: `w < limit <= self.nwords`.
            let mut word = unsafe { (*self.words.add(w)).load(Ordering::Acquire) };
            while word != 0 {
                let b = word.trailing_zeros() as usize;
                word &= word - 1;
                if !f(self.base + ((w * 64 + b) << 3)) {
                    return;
                }
            }
        }
        // The spill is always walked in full: its members are precisely the
        // ones whose address the grid could not reason about, so no bound
        // derived from the grid may exclude them.
        if self.has_spill() {
            for &addr in self.overflow.lock().iter() {
                if !f(addr) {
                    return;
                }
            }
        }
    }
}

impl Drop for HeapBitmap {
    fn drop(&mut self) {
        if self.nwords == 0 {
            return;
        }
        if self.mapped_len != 0 {
            // SAFETY: `words` is the mapping `labelled_side_table` made with
            // exactly this length, released exactly once.
            unsafe { crate::reservation::unmap_side_table(self.words as *mut u8, self.mapped_len) };
            return;
        }
        let layout = std::alloc::Layout::array::<AtomicU64>(self.nwords)
            .expect("zgc object-start bitmap layout overflow");
        // SAFETY: `words` came from `alloc_zeroed` with this exact layout and
        // is freed exactly once.
        unsafe { std::alloc::dealloc(self.words as *mut u8, layout) };
    }
}

// ---------------------------------------------------------------------------
// Tests (gengc-round2-alloc2, 2026-09-20)
//
// This module had none of its own: every property it has was exercised only
// through `MarkBitmap`, `young_mark` and `zgc::starts`. The two below pin the
// bounded-clear contract that `MarkBitmap::clear` started leaning on in round
// 2: the `word_base` <-> `word_range` round trip that turns a pair of WORD
// indices into the ADDRESS range `clear_within` takes, and the wipe itself. A
// widening in that conversion would silently over-clear (harmless); a
// narrowing would leave a stale black bit, which is a live object reaped. That
// is worth a test rather than an argument.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The block summary skips the never-bumped middle without changing an
    /// answer: every probe agrees with a plain word-by-word backward scan, on
    /// a bitmap with starts at both ends, one in the middle, every writer
    /// (`insert`, `claim`, `set_range`) and probes on, just above and far
    /// above each set bit and across block boundaries (2026-10-07,
    /// `first-segment-access-starves-under-back-to-back-system-gc`).
    #[test]
    fn nearest_base_skips_empty_blocks_and_agrees_with_a_plain_scan() {
        const BASE: usize = 0x4000_0000;
        let span = 64 * 1024 * 1024; // 131 072 words, 2 048 blocks
        let bm = HeapBitmap::labelled(BASE, span, "test");
        bm.insert(BASE);
        bm.insert(BASE + 8 * 77);
        bm.claim(BASE + 40 * 1024 * 1024 + 16);
        bm.set_range(BASE + 32 * 1024 - 64, BASE + 32 * 1024 + 64); // straddles a block edge
        bm.insert(BASE + span - 8);
        let plain = |addr: usize| -> Option<usize> {
            let mut a = addr.min(BASE + span - 1) & !7;
            loop {
                if bm.contains(a) {
                    return Some(a);
                }
                if a == BASE {
                    return None;
                }
                a -= 8;
            }
        };
        let probes = [
            BASE,
            BASE + 8,
            BASE + 8 * 77,
            BASE + 8 * 78,
            BASE + 32 * 1024 - 72,
            BASE + 32 * 1024 - 64,
            BASE + 32 * 1024 + 56,
            BASE + 32 * 1024 + 64,
            BASE + 1024 * 1024,
            BASE + 40 * 1024 * 1024 + 8,
            BASE + 40 * 1024 * 1024 + 16,
            BASE + 40 * 1024 * 1024 + 4096 * 8,
            BASE + span - 16,
            BASE + span - 8,
            BASE + span + 4096,
        ];
        for addr in probes {
            assert_eq!(
                bm.nearest_base_at_or_below(addr),
                plain(addr),
                "addr {:#x}",
                addr
            );
        }
        // Nothing at or below: the empty low side of a fresh bitmap.
        let empty = HeapBitmap::labelled(BASE, span, "test");
        empty.insert(BASE + span / 2);
        assert_eq!(empty.nearest_base_at_or_below(BASE + span / 2 - 8), None);
        assert_eq!(
            empty.nearest_base_at_or_below(BASE + span - 8),
            Some(BASE + span / 2)
        );
    }

    /// `snapshot_word_runs` copies exactly the merged word ranges, and
    /// `retain_marked_within` prunes exactly what `retain_marked` would when the
    /// ranges hold every start.
    #[test]
    fn word_runs_and_bounded_retain_match_the_whole_bitmap_forms() {
        const BASE: usize = 0x4000_0000;
        let span = 1024 * 1024;
        let starts = HeapBitmap::labelled(BASE, span, "test");
        let marks = HeapBitmap::labelled(BASE, span, "test");
        for off in [0usize, 8, 512, 4096, 8192] {
            starts.insert(BASE + off);
        }
        starts.insert(BASE + span - 8);
        marks.insert(BASE + 8);
        marks.insert(BASE + span - 8);
        let ranges = [
            (BASE, BASE + 16 * 1024),
            (BASE + span - 4096, BASE + span),
            (BASE + 100, BASE + 200),
        ];
        let runs = starts.snapshot_word_runs(&ranges);
        assert_eq!(
            runs.len(),
            2,
            "the overlapping third range merges into the first"
        );
        for (w0, words) in &runs {
            for (i, word) in words.iter().enumerate() {
                assert_eq!(*word, starts.word_at(w0 + i));
            }
        }
        starts.retain_marked_within(&marks, &ranges[..2]);
        for w in 0..starts.word_count() {
            assert_eq!(starts.word_at(w), marks.word_at(w), "word {w}");
        }
    }

    /// `word_range(word_base(lo), word_base(hi))` must return `(lo, hi)`.
    ///
    /// Checked at a span that is NOT a multiple of 512, which is where the two
    /// functions could disagree: `word_base(nwords)` runs past `base + span`
    /// and `word_range` clamps it back, so the round trip survives only
    /// because the clamp lands inside the same final word.
    #[test]
    fn the_word_index_to_address_round_trip_is_exact() {
        for span in [512usize, 4096, 1000, 1 << 16, 8] {
            let bm = HeapBitmap::labelled(0x10_0000, span, "test");
            let n = bm.word_count();
            for lo in 0..n {
                for hi in (lo + 1)..=n {
                    let (a, b) = bm.word_range(bm.word_base(lo), bm.word_base(hi));
                    assert_eq!(
                        (a, b),
                        (lo, hi),
                        "span={span} nwords={n}: words ({lo},{hi}) did not round trip"
                    );
                }
            }
        }
    }

    /// A `clear_within` whose range covers every set bit wipes all of them,
    /// while storing into only the words it was given.
    ///
    /// This is exactly the contract `MarkBitmap::clear` now leans on: it
    /// converts its marked-word WINDOW into this address range, and the
    /// window is a superset of the set bits by construction. Note the shape of
    /// the test — it does NOT set a bit outside the range and check it
    /// survives, because `clear_within` opens with
    /// `debug_assert_clear_outside`, so a caller that leaves a bit outside its
    /// bounds is a debug-build panic rather than an observable behaviour.
    /// Every word outside is already zero; the saving is that it stays
    /// un-stored-to.
    #[test]
    fn clear_within_wipes_every_bit_inside_its_range() {
        let bm = HeapBitmap::labelled(0x10_0000, 1 << 16, "test");
        let base = bm.base;
        assert_eq!(bm.word_count(), 128, "1<<16 bytes is 128 bitmap words");

        // Three bases inside words 5..8, i.e. arena bytes 2560..4096.
        let addrs = [base + 5 * 512, base + 6 * 512 + 24, base + 7 * 512 + 504];
        for a in addrs {
            bm.insert(a);
        }
        assert!(!bm.has_spill(), "every address is on the 8-byte grid");
        for a in addrs {
            assert!(bm.contains(a), "{a:#x} should be registered");
        }

        // The window a caller would derive: words 5..8, 3 of 128.
        bm.clear_within(&[(bm.word_base(5), bm.word_base(8))]);
        for a in addrs {
            assert!(!bm.contains(a), "{a:#x} survived a clear that covered it");
        }

        // A second clear over the same range is a no-op, and a full-range one
        // still works on an already-empty bitmap (the degenerate case
        // `MarkBitmap::clear`'s `lo >= hi` arm falls back to).
        bm.clear_within(&[(bm.word_base(5), bm.word_base(8))]);
        bm.clear_all();
        for a in addrs {
            assert!(!bm.contains(a));
        }
    }

    /// gc-common w3-e: the `&mut` (memset) clears remove exactly what the
    /// atomic ones do -- the covered words, and the overflow spill set -- and
    /// leave words outside a bounded range alone.
    #[test]
    fn exclusive_clears_match_the_atomic_ones() {
        let mut bm = HeapBitmap::labelled(0x10_0000, 1 << 16, "test");
        let base = bm.base;
        let inside = [base + 5 * 512, base + 7 * 512 + 504];
        let outside = base + 20 * 512 + 8;
        for a in inside {
            bm.insert(a);
        }
        bm.insert(outside);
        // A bounded clear must not reach word 20. (Debug builds would trip
        // `debug_assert_clear_outside` on it, so the range covers it too and
        // the check is on what a SECOND, narrower clear leaves.)
        bm.clear_within_exclusive(&[(bm.word_base(5), bm.word_base(21))]);
        for a in inside.iter().copied().chain([outside]) {
            assert!(!bm.contains(a), "{a:#x} survived the bounded memset");
        }
        bm.insert(inside[0]);
        bm.spill(base + 3);
        assert!(bm.has_spill());
        bm.clear_all_exclusive();
        assert!(!bm.contains(inside[0]));
        assert!(!bm.has_spill(), "the overflow set must be cleared too");
        assert!((0..bm.word_count()).all(|w| bm.word_at(w) == 0));
    }

    /// zt t1/tp (zr4rv4 mark-pool item 4): with an off-grid address spilled,
    /// an in-span address is answered by its bit (no overflow probe), and the
    /// spilled and out-of-span addresses still by the set.
    #[test]
    fn zt1tp_a_spill_does_not_change_in_span_answers() {
        let bm = HeapBitmap::labelled(0x10_0000, 1 << 16, "test");
        let base = bm.base;
        bm.insert(base + 64);
        bm.insert(base + 3); // off the grid: spilled
        bm.insert(base + (1 << 16) + 8); // past the span: spilled
        assert!(bm.has_spill());
        assert!(bm.contains(base + 64));
        assert!(!bm.contains(base + 72), "unmarked in-span slot");
        assert!(bm.contains(base + 3));
        assert!(bm.contains(base + (1 << 16) + 8));
        assert!(!bm.contains(base + 5), "an off-grid address never spilled");
    }

    /// gen r5w5/old9: `set_range` sets every slot whose BASE lies in
    /// `[lo, hi)` — including the slot at `floor8(hi)` when `hi` is not on the
    /// grid, which the old `hi_off / 8` dropped — and nothing at or past the
    /// first slot at or above `hi`, in one word and across words.
    #[test]
    fn set_range_covers_the_last_slot_of_an_unaligned_end() {
        let cases: [(usize, usize); 4] = [
            (5, 17 * 8 + 3),
            (5, 200 * 8 + 1),
            (0, 64 * 8 + 7),
            (3, 4 * 8),
        ];
        for (lo_slot, hi_bytes) in cases {
            let bm = HeapBitmap::labelled(0x10_0000, 1 << 16, "test");
            let base = bm.base;
            bm.set_range(base + lo_slot * 8, base + hi_bytes);
            let end_slot = hi_bytes.div_ceil(8);
            for slot in 0..(end_slot + 70) {
                assert_eq!(
                    bm.contains(base + slot * 8),
                    slot >= lo_slot && slot < end_slot,
                    "range [{lo_slot} slots, {hi_bytes} bytes): slot {slot}",
                );
            }
        }
    }

    /// nio20 wave 6, lane L2: `nearest_base_in(addr, floor)` is exactly
    /// `nearest_base_at_or_below(addr)` restricted to `>= floor`, for every
    /// address and floor over sparse and dense layouts -- including the
    /// four-word skip's edges (runs of empty words of every length mod 4,
    /// a floor inside, at and past a populated word) -- and `floor == 0` is
    /// the unbounded scan. Checked against a brute-force walk over a plain
    /// `Vec`, so the reference cannot share a bug with the bit scan.
    #[test]
    fn nearest_base_in_is_the_floored_nearest_base() {
        const BASE: usize = 0x40_0000;
        const SPAN: usize = 64 * 512; // 64 words
                                      // Bases chosen so the gaps between populated words are 0..=9 words.
        let layouts: [&[usize]; 4] = [
            &[],
            &[
                8,
                504,
                512,
                3 * 512 + 16,
                4 * 512,
                9 * 512 + 8,
                15 * 512,
                20 * 512 + 256,
                27 * 512,
                36 * 512 + 8,
                46 * 512,
                63 * 512 + 504,
            ],
            &[0],
            &[63 * 512 + 504],
        ];
        for bases in layouts {
            let bm = HeapBitmap::labelled(BASE, SPAN, "test");
            for &b in bases {
                bm.insert(BASE + b);
            }
            let truth = |addr: usize, floor: usize| -> Option<usize> {
                bases
                    .iter()
                    .map(|&b| BASE + b)
                    .filter(|&b| b <= addr && b >= floor)
                    .max()
            };
            // Every third grid slot plus a few misaligned and out-of-span
            // probes (every slot of every floor is ~10M calls in a debug run).
            let mut addrs: Vec<usize> = (0..SPAN / 8).step_by(3).map(|s| BASE + s * 8).collect();
            addrs.extend([
                BASE + 3,
                BASE + 512 + 7,
                BASE + SPAN,
                BASE + SPAN + 4096,
                BASE - 8,
            ]);
            for &addr in &addrs {
                assert_eq!(
                    bm.nearest_base_in(addr, 0),
                    bm.nearest_base_at_or_below(addr),
                    "floor 0 must be the unbounded scan at {addr:#x}"
                );
                for floor_slot in (0..=SPAN / 8 + 2).step_by(13) {
                    let floor = BASE + floor_slot * 8;
                    assert_eq!(
                        bm.nearest_base_in(addr, floor),
                        truth(addr, floor),
                        "addr={addr:#x} floor={floor:#x} bases={bases:?}"
                    );
                }
                // A floor below the bitmap is no floor at all.
                assert_eq!(bm.nearest_base_in(addr, 0x1000), truth(addr, 0));
            }
        }
    }

    /// nio21 wave 1 L1: the floored scan over SEVERAL summary blocks, where
    /// the four-word skip now stops at each block edge so the block-summary
    /// skip can jump never-noted blocks. Bases sit in blocks 0, 3 and 7 of
    /// eight (one at each block's first and last slot too), every other
    /// block is never noted, and one block is noted but emptied again (a
    /// swept stretch: its summary bit stays set). Checked against a
    /// brute-force walk, for addresses in every block and floors in, at and
    /// between the populated blocks.
    #[test]
    fn nearest_base_in_crosses_never_noted_blocks() {
        const BASE: usize = 0x80_0000;
        const BLOCK_BYTES: usize = BLOCK_WORDS * 512;
        const SPAN: usize = 8 * BLOCK_BYTES;
        let bases: [usize; 6] = [
            8,
            3 * BLOCK_BYTES,
            3 * BLOCK_BYTES + 17 * 512 + 40,
            4 * BLOCK_BYTES - 8,
            7 * BLOCK_BYTES + 5 * 512,
            8 * BLOCK_BYTES - 8,
        ];
        let bm = HeapBitmap::labelled(BASE, SPAN, "test");
        for &b in &bases {
            bm.insert(BASE + b);
        }
        // Block 5: noted, then emptied (its summary bit stays set).
        let swept = BASE + 5 * BLOCK_BYTES + 512;
        bm.insert(swept);
        bm.remove(swept);
        let truth = |addr: usize, floor: usize| -> Option<usize> {
            bases
                .iter()
                .map(|&b| BASE + b)
                .filter(|&b| b <= addr && b >= floor)
                .max()
        };
        let mut addrs: Vec<usize> = (0..SPAN / 8).step_by(61).map(|s| BASE + s * 8).collect();
        addrs.extend(bases.iter().map(|&b| BASE + b));
        addrs.extend([
            BASE + SPAN - 8,
            BASE + SPAN,
            BASE + 6 * BLOCK_BYTES - 8,
            BASE + 4 * BLOCK_BYTES,
        ]);
        let floors: Vec<usize> = (0..=8)
            .flat_map(|blk| {
                [
                    blk * BLOCK_BYTES,
                    blk * BLOCK_BYTES + 8,
                    (blk * BLOCK_BYTES).saturating_sub(8),
                ]
            })
            .map(|o| BASE + o)
            .chain([0, BASE + 3 * BLOCK_BYTES + 17 * 512 + 48])
            .collect();
        for &addr in &addrs {
            for &floor in &floors {
                assert_eq!(
                    bm.nearest_base_in(addr, floor),
                    truth(addr, floor),
                    "addr={addr:#x} floor={floor:#x}"
                );
            }
        }
    }

    /// nio21 wave 9 L3: `forget_empty_blocks` clears exactly the noted blocks
    /// whose words are all zero (a swept stretch), keeps every populated one,
    /// changes no `nearest_base_in` answer, and a later insert into a
    /// forgotten block notes it again and is found.
    #[test]
    fn forget_empty_blocks_clears_only_swept_blocks_and_keeps_answers() {
        const BASE: usize = 0x80_0000;
        const BLOCK_BYTES: usize = BLOCK_WORDS * 512;
        const SPAN: usize = 8 * BLOCK_BYTES;
        let bm = HeapBitmap::labelled(BASE, SPAN, "test");
        let live: Vec<usize> = vec![
            BASE + 8,
            BASE + 3 * BLOCK_BYTES - 8,
            BASE + 6 * BLOCK_BYTES + 4096,
        ];
        for &b in &live {
            bm.insert(b);
        }
        // Blocks 1, 4 and 5 are noted and then swept empty (block 4 at its
        // first slot, block 5 at its last word, block 1 by a range).
        let swept = [BASE + 4 * BLOCK_BYTES, BASE + 6 * BLOCK_BYTES - 8];
        for &s in &swept {
            bm.insert(s);
            bm.remove(s);
        }
        let ranged = BASE + BLOCK_BYTES + 512;
        bm.set_range(ranged, ranged + 64);
        for a in (ranged..ranged + 64).step_by(8) {
            bm.remove(a);
        }
        for blk in [0, 1, 2, 4, 5, 6] {
            assert!(bm.block_noted(blk), "block {blk} noted before the pass");
        }
        assert_eq!(bm.forget_empty_blocks(), 3, "blocks 1, 4 and 5 are empty");
        for (blk, noted) in [
            (0, true),
            (1, false),
            (2, true),
            (3, false),
            (4, false),
            (5, false),
            (6, true),
            (7, false),
        ] {
            assert_eq!(bm.block_noted(blk), noted, "block {blk}");
        }
        assert_eq!(bm.forget_empty_blocks(), 0, "idempotent");
        let truth = |live: &[usize], addr: usize, floor: usize| -> Option<usize> {
            live.iter()
                .copied()
                .filter(|&b| b <= addr && b >= floor)
                .max()
        };
        let check = |live: &[usize]| {
            for slot in (0..SPAN / 8 + 4).step_by(29) {
                let addr = BASE + slot * 8;
                for floor in [0, BASE, BASE + 2 * BLOCK_BYTES, BASE + 5 * BLOCK_BYTES + 8] {
                    assert_eq!(
                        bm.nearest_base_in(addr, floor),
                        truth(live, addr, floor),
                        "addr={addr:#x} floor={floor:#x}"
                    );
                }
            }
        };
        check(&live[..]);
        // A new allocation in a forgotten block re-notes it before its bit.
        let reused = BASE + 4 * BLOCK_BYTES + 1024;
        bm.insert(reused);
        assert!(bm.block_noted(4));
        let mut live2 = live.clone();
        live2.push(reused);
        check(&live2[..]);
    }

    /// nio21 wave 9 L3: a writer that sets a word bit WITHOUT `insert` (the
    /// JIT's inline start-bit store) is invisible to the summary-bounded scan
    /// unless its range was noted first, and visible once it was. This is
    /// the invariant the VM-TLAB carve now restores with
    /// `note_blocks_covering`.
    #[test]
    fn a_raw_word_store_needs_its_block_noted_first() {
        const BASE: usize = 0x80_0000;
        const BLOCK_BYTES: usize = BLOCK_WORDS * 512;
        const SPAN: usize = 8 * BLOCK_BYTES;
        let raw_store = |bm: &HeapBitmap, addr: usize| {
            let (w, mask) = bm.locate(addr).expect("on grid");
            // SAFETY: `locate` bounds-checked `w`.
            unsafe { (*bm.words.add(w)).fetch_or(mask, Ordering::Release) };
        };
        let obj = BASE + 5 * BLOCK_BYTES + 3 * 512 + 64;
        let probe = BASE + 7 * BLOCK_BYTES; // two blocks above, past a block edge

        let unnoted = HeapBitmap::labelled(BASE, SPAN, "test");
        unnoted.insert(BASE + 8);
        raw_store(&unnoted, obj);
        assert_eq!(
            unnoted.nearest_base_in(probe, 0),
            Some(BASE + 8),
            "the documented hazard: an un-noted block is skipped"
        );

        let noted = HeapBitmap::labelled(BASE, SPAN, "test");
        noted.insert(BASE + 8);
        noted.note_blocks_covering(BASE + 4 * BLOCK_BYTES + 512, BASE + 6 * BLOCK_BYTES + 512);
        for blk in [4, 5, 6] {
            assert!(noted.block_noted(blk), "block {blk}");
        }
        assert!(!noted.block_noted(7));
        raw_store(&noted, obj);
        assert_eq!(noted.nearest_base_in(probe, 0), Some(obj));
        assert_eq!(noted.nearest_base_in(obj + 100, obj), Some(obj));
        // An empty or out-of-span range notes nothing and does not panic.
        noted.note_blocks_covering(BASE + 7 * BLOCK_BYTES, BASE + 7 * BLOCK_BYTES);
        noted.note_blocks_covering(BASE + SPAN, BASE + SPAN + 4096);
        assert!(!noted.block_noted(7));
    }

    /// zr z5/sw: the per-cycle reset of the summary forgets exactly the
    /// blocks wholly inside the cleared ranges, keeps a straddling block's bit
    /// (the conservative direction), and every writer re-notes its block on
    /// the next cycle, so `block_may_hold_bits == false` keeps proving "no
    /// set word" across a clear.
    #[test]
    fn forgetting_blocks_after_a_clear_keeps_the_summary_sound() {
        const BASE: usize = 0x80_0000;
        const BLOCK_BYTES: usize = BLOCK_WORDS * 512;
        const SPAN: usize = 8 * BLOCK_BYTES;
        let bm = HeapBitmap::labelled(BASE, SPAN, "test");
        bm.claim(BASE + 8); // block 0
        bm.claim(BASE + 2 * BLOCK_BYTES + 64); // block 2
        bm.set_range(BASE + 5 * BLOCK_BYTES - 64, BASE + 5 * BLOCK_BYTES + 64); // blocks 4, 5
        for blk in [0, 2, 4, 5] {
            assert!(
                bm.block_may_hold_bits(blk),
                "block {blk} noted by its writer"
            );
        }
        assert!(!bm.block_may_hold_bits(1));
        assert!(!bm.block_may_hold_bits(99), "out of range reads as empty");
        // Clear [block 0 .. middle of block 5): blocks 0..=4 are whole, 5 straddles.
        let ranges = [(BASE, BASE + 5 * BLOCK_BYTES + 512)];
        bm.clear_within_sparse(&ranges);
        let forgotten = bm.forget_blocks_within(Some(&ranges));
        assert_eq!(
            forgotten, 3,
            "blocks 0, 2 and 4 were set and wholly cleared"
        );
        for blk in [0, 2, 4] {
            assert!(!bm.block_may_hold_bits(blk), "block {blk} forgotten");
        }
        assert!(
            bm.block_may_hold_bits(5),
            "a straddling block keeps its bit"
        );
        // The next cycle's claim re-notes before it sets the word.
        assert!(bm.claim(BASE + 2 * BLOCK_BYTES + 64));
        assert!(bm.block_may_hold_bits(2));
        // `None` forgets everything (the clear_all arm).
        bm.clear_all();
        assert!(bm.forget_blocks_within(None) >= 2);
        for blk in 0..8 {
            assert!(!bm.block_may_hold_bits(blk), "block {blk} after clear_all");
        }
    }

    /// zr z10/mt: a bitmap on its own mapping is the same bitmap. It is on a
    /// mapping exactly when asked for, big enough and on Linux; it reads zero
    /// before any write; every writer and reader answers as on the
    /// `alloc_zeroed` twin over the same geometry; and it is released by
    /// `Drop` (a second one built after the first is dropped works too).
    #[test]
    fn zr10mt_a_side_table_bitmap_is_the_same_bitmap_on_its_own_mapping() {
        const BASE: usize = 0x4000_0000;
        let span = 64 * 1024 * 1024; // 1 MiB of words: the threshold
        for _ in 0..2 {
            let mapped = HeapBitmap::labelled_side_table(BASE, span, "test", true);
            let plain = HeapBitmap::labelled(BASE, span, "test");
            assert_eq!(mapped.on_own_mapping(), cfg!(target_os = "linux"));
            assert!(!plain.on_own_mapping());
            assert_eq!(mapped.nwords, plain.nwords);
            assert_eq!(mapped.reserved_bytes(), SIDE_TABLE_MAP_MIN_BYTES);
            assert_eq!(mapped.words as usize % std::mem::align_of::<AtomicU64>(), 0);
            // SAFETY: `words` holds `nwords` `AtomicU64`s for the bitmap's life.
            let words = unsafe { std::slice::from_raw_parts(mapped.words, mapped.nwords) };
            assert!(words.iter().all(|w| w.load(Ordering::Relaxed) == 0));
            for bm in [&mapped, &plain] {
                bm.insert(BASE);
                bm.insert(BASE + 8 * 77);
                assert!(bm.claim(BASE + 40 * 1024 * 1024 + 16));
                bm.insert(BASE + span - 8);
                bm.remove(BASE + 8 * 77);
            }
            for probe in [0, 8, 8 * 77, 40 * 1024 * 1024 + 16, span - 8, span - 16] {
                assert_eq!(
                    mapped.contains(BASE + probe),
                    plain.contains(BASE + probe),
                    "probe +{probe:#x}"
                );
            }
            assert!(mapped.contains(BASE + span - 8));
            mapped.clear_all();
            assert!(!mapped.contains(BASE));
        }
    }

    /// zt t3/rl (`CRATONVM_ZGC_BITMAP_RELEASE_WITH_GRANULE`): the discard
    /// gives back only all-zero pages wholly inside the range, keeps a page
    /// with a bit, and leaves an allocator block alone. One 4 KiB page of
    /// words covers 256 KiB of arena.
    #[test]
    fn zt3_rl_discard_gives_back_only_zero_pages_inside_the_range() {
        const BASE: usize = 0x4000_0000;
        const PAGE_ARENA: usize = 256 * 1024;
        let span = 64 * 1024 * 1024;
        let bm = HeapBitmap::labelled_side_table(BASE, span, "test", true);
        bm.insert(BASE + 3 * PAGE_ARENA + 64);
        let released = bm.discard_zero_pages_over(BASE, BASE + 8 * PAGE_ARENA);
        if !bm.on_own_mapping() {
            // Off Linux (or a refused mapping): nothing to give back.
            assert_eq!(released, 0);
            return;
        }
        // Pages 0..8 but page 3; 0 if the kernel's page is not 4 KiB.
        assert!(released == 7 * 4096 || released == 0, "{released}");
        assert!(bm.contains(BASE + 3 * PAGE_ARENA + 64), "the set page kept its bit");
        // No whole page inside: nothing.
        assert_eq!(bm.discard_zero_pages_over(BASE + 8, BASE + PAGE_ARENA - 8), 0);
        // A discarded page reads zero and takes bits again.
        assert!(!bm.contains(BASE + 64));
        bm.insert(BASE + 64);
        assert!(bm.contains(BASE + 64));
        let plain = HeapBitmap::labelled_side_table(BASE, span, "test", false);
        assert_eq!(plain.discard_zero_pages_over(BASE, BASE + span), 0);
    }

    /// zr z10/mt: the mapping is never taken when not asked for, for a
    /// table under the threshold, or for an empty one.
    #[test]
    fn zr10mt_small_or_unrequested_side_tables_stay_allocator_blocks() {
        const BASE: usize = 0x4000_0000;
        let big = 64 * 1024 * 1024;
        assert!(!HeapBitmap::labelled_side_table(BASE, big, "test", false).on_own_mapping());
        let small = HeapBitmap::labelled_side_table(BASE, big / 2, "test", true);
        assert!(
            !small.on_own_mapping(),
            "512 KiB of words is under the threshold"
        );
        small.insert(BASE + 64);
        assert!(small.contains(BASE + 64));
        let empty = HeapBitmap::labelled_side_table(BASE, 0, "test", true);
        assert!(!empty.on_own_mapping());
        assert_eq!(empty.nwords, 0);
    }
}

/// JIT round 23 wave 8, lane W8-L3 (`CRATONVM_GEN_LAZY_MIRROR_PLAIN_STORES`):
/// the single-writer forms write exactly the words the locked forms write.
#[cfg(test)]
mod r23_w8_single_writer_tests {
    use super::*;

    fn words(bm: &HeapBitmap) -> Vec<u64> {
        (0..bm.word_count()).map(|w| bm.word_at(w)).collect()
    }

    /// Every edge shape (inside one word, word-aligned ends, ragged ends,
    /// clamped past the span, empty) over a bitmap already holding bits.
    #[test]
    fn r23_w8_set_range_single_writer_equals_set_range() {
        let base = 0x10_0000usize;
        let span = 1usize << 16;
        let ranges = [
            (base + 8, base + 64),
            (base + 512, base + 1024),
            (base + 520, base + 4096 + 24),
            (base + 1000, base + 1001),
            (base + span - 40, base + span + 4096),
            (base, base + span),
            (base + 64, base + 64),
            (base - 64, base + 16),
        ];
        for &(lo, hi) in &ranges {
            let a = HeapBitmap::labelled(base, span, "test");
            let b = HeapBitmap::labelled(base, span, "test");
            for bm in [&a, &b] {
                bm.insert(base + 0x800);
                bm.insert(base + 8);
            }
            a.set_range(lo, hi);
            b.set_range_single_writer(lo, hi);
            assert_eq!(words(&a), words(&b), "range {:#x}..{:#x}", lo - base, hi.wrapping_sub(base));
        }
    }

    /// The plain store replaces the word and ignores an out-of-range index.
    #[test]
    fn r23_w8_store_word_single_writer_replaces_the_word() {
        let bm = HeapBitmap::labelled(0x10_0000, 1 << 16, "test");
        bm.store_word_single_writer(3, 0xF0F0);
        assert_eq!(bm.word_at(3), 0xF0F0);
        bm.store_word_single_writer(3, 0xF000);
        assert_eq!(bm.word_at(3), 0xF000);
        bm.store_word_single_writer(bm.word_count(), 1);
        assert_eq!(words(&bm).iter().filter(|&&w| w != 0).count(), 1);
    }

    /// zgc pc3: the plain-store prune leaves the words the locked one does,
    /// and the summarised live walk names the bases the plain walk does,
    /// before and after the empty blocks are forgotten (a stale set bit only
    /// costs a word scan; a clear bit is skipped).
    #[test]
    fn zgc_pc3_plain_retain_and_summarised_bases_agree_with_the_locked_forms() {
        const BASE: usize = 0x4000_0000;
        let span = 8 * 1024 * 1024; // 16 384 words, 256 blocks
        let build = || {
            let starts = HeapBitmap::labelled(BASE, span, "test");
            let marks = HeapBitmap::labelled(BASE, span, "test");
            // A dense dead run, survivors scattered through it, one survivor
            // on each side of a block edge, and an empty stretch.
            for i in 0..20_000usize {
                let a = BASE + i * 144;
                starts.insert(a);
                if i % 997 == 0 || i == 227 || i == 228 {
                    marks.insert(a);
                }
            }
            starts.insert(BASE + span - 8);
            marks.insert(BASE + span - 8);
            (starts, marks)
        };
        let ranges = [(BASE, BASE + span)];
        let (locked, marks) = build();
        let (plain, _) = build();
        locked.retain_marked_within(&marks, &ranges);
        plain.retain_marked_within_plain(&marks, &ranges);
        assert_eq!(words(&locked), words(&plain));
        let expect = locked.bases_within(&ranges);
        assert!(expect.len() > 20, "{}", expect.len());
        assert_eq!(
            plain.bases_within_summarised(&ranges),
            expect,
            "stale summary bits"
        );
        assert!(plain.forget_empty_blocks() > 0);
        assert_eq!(
            plain.bases_within_summarised(&ranges),
            expect,
            "forgotten blocks"
        );
        assert_eq!(plain.bases_within(&ranges), expect);
    }
}
