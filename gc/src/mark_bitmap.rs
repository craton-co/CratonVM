// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Mark bitmap for concurrent garbage collection.
//!
//! A bit-level bitmap covering the old generation's address space. Each bit
//! represents one potential object location at `MARK_GRANULARITY`-byte
//! intervals. Used during concurrent marking to distinguish:
//!
//! - **White** (unmarked): Potentially garbage — bit is 0.
//! - **Black** (marked): Known live — bit is 1.
//!
//! Gray objects (live but unscanned) are tracked in the mark queue, not in
//! the bitmap itself.

use std::sync::atomic::{fence, AtomicBool, AtomicU64, Ordering};

/// Granularity of the mark bitmap: one bit per 8 bytes of heap.
/// This matches the minimum object alignment (8-byte aligned headers).
pub const MARK_GRANULARITY: usize = 8;

/// A concurrent mark bitmap backed by atomic u64 words.
///
/// Thread-safe: multiple marker threads can set bits concurrently using
/// atomic CAS operations. The bitmap is allocated once per GC cycle and
/// cleared between cycles.
pub struct MarkBitmap {
    /// Atomic bitmap words. Each word covers 64 * MARK_GRANULARITY = 512 bytes.
    ///
    /// Built by [`Self::new`] from a ZEROED allocation rather than element by
    /// element -- see that constructor.
    words: Box<[AtomicU64]>,
    /// Base address of the heap region this bitmap covers.
    base_addr: usize,
    /// Total size (bytes) of the covered region.
    region_size: usize,
    /// Exclusive end of the covered region, precomputed.
    ///
    /// `base_addr + region_size`, saturating. [`Self::try_mark`] and
    /// [`Self::is_marked`] are the innermost loop of a mark cycle and each ran
    /// a `checked_add` plus two range compares plus a redundant
    /// `word_index >= words.len()` test per bit. The end and the word count are
    /// immutable for the bitmap's whole life, so they are computed once here
    /// and the per-bit work is two unsigned compares.
    region_end: usize,
    /// `words.len()`, precomputed for the same reason.
    num_words: usize,
    /// Has ANY bit in this bitmap been set since the last [`Self::clear`]?
    ///
    /// # Why a whole flag for this
    ///
    /// `clear()` is O(bitmap), and G1 calls it from `G1Region::reset()` for
    /// EVERY region a cleanup frees -- including the overwhelming majority
    /// that a young pause never marked into at all. Without this flag those
    /// regions pay a full sweep of their bitmap to write zeroes over zeroes.
    ///
    /// The flag is only ever set, never cleared except by `clear()` itself, so
    /// a `true` can be stale (a clear that was not strictly needed) but a
    /// `false` can NOT: the store below is sequenced BEFORE the `fetch_or` that
    /// sets the bit, so no bit is observable without the flag already being
    /// set. A stale-`false` would skip a needed clear and leave a black bit
    /// from the previous cycle -- a live object reaped, i.e. a use-after-free
    /// -- which is why the order matters and is asserted by
    /// `clear_skips_only_an_untouched_bitmap`.
    any_marked: AtomicBool,
}

impl MarkBitmap {
    /// Create a new zeroed bitmap covering a heap region.
    ///
    /// # Why the allocation is not `(0..n).map(|_| AtomicU64::new(0)).collect()`
    ///
    /// That is what stood here, and it materialises the bitmap one atomic at a
    /// time: a 4 GiB old generation is 8.4M words, and G1 builds one of these
    /// PER REGION at start-up. `vec![0u64; n]` takes `Vec`'s zeroing
    /// specialisation, which is a single `alloc_zeroed` -- on both platforms a
    /// lazily-zeroed mapping, so the pages are not even touched until a bit is
    /// actually set in them.
    ///
    /// The `u64 -> AtomicU64` conversion is a reinterpretation of an
    /// allocation this function exclusively owns, between two types with
    /// identical size, alignment and (for the value zero) bit pattern. Same
    /// shape as `zgc::starts::ZObjectStartBits::new`, which does its own
    /// `alloc_zeroed` for the same reason.
    pub fn new(base_addr: usize, region_size: usize) -> Self {
        let num_bits = region_size.div_ceil(MARK_GRANULARITY);
        let num_words = num_bits.div_ceil(64);
        let zeroed: Box<[u64]> = vec![0u64; num_words].into_boxed_slice();
        // SAFETY: `AtomicU64` is `#[repr(C)]` over an `UnsafeCell<u64>`, so it
        // has the same size and alignment as `u64`, and `AtomicU64::new(0)` has
        // the all-zero bit pattern this allocation already holds. The box is
        // owned exclusively here and no reference to it as `[u64]` survives the
        // conversion, so no aliasing rule is broken.
        let words: Box<[AtomicU64]> = unsafe {
            let raw = Box::into_raw(zeroed) as *mut [AtomicU64];
            Box::from_raw(raw)
        };
        Self {
            words,
            base_addr,
            region_size,
            region_end: base_addr.saturating_add(region_size),
            num_words,
            any_marked: AtomicBool::new(false),
        }
    }

    /// Word index for `addr`, or `None` when `addr` is outside this bitmap.
    ///
    /// The single containment screen both bit accessors share, so they cannot
    /// drift into disagreeing about what this bitmap covers.
    #[inline]
    fn locate(&self, addr: usize) -> Option<(usize, u64)> {
        if addr < self.base_addr || addr >= self.region_end {
            return None;
        }
        let bit_index = (addr - self.base_addr) / MARK_GRANULARITY;
        let word_index = bit_index / 64;
        if word_index >= self.num_words {
            return None;
        }
        Some((word_index, 1u64 << (bit_index % 64)))
    }

    /// Attempt to mark the bit for the given address. Returns `true` if the
    /// bit was newly set (was 0, now 1). Returns `false` if already marked.
    ///
    /// This is lock-free and safe to call from multiple threads concurrently.
    #[inline]
    pub fn try_mark(&self, addr: usize) -> bool {
        let Some((word_index, mask)) = self.locate(addr) else {
            return false;
        };
        // Sequenced BEFORE the `fetch_or`, so a set bit can never be observed
        // without this flag already being true -- see the field's doc for why
        // the other order would be a use-after-free. The read-first keeps the
        // line read-shared once the first mark of a cycle has happened, so the
        // steady-state cost is one relaxed load off an already-hot line.
        if !self.any_marked.load(Ordering::Relaxed) {
            self.any_marked.store(true, Ordering::Release);
        }

        // Atomic fetch-or: set the bit and check if it was already set.
        // Round-5 fix (CRIT — ARM cross-cycle race): pair `clear()`'s
        // `Release` store with `AcqRel` on the mark-side `fetch_or`. The
        // Acquire half guarantees the marker observes the zero bits
        // published by the prior cycle's `clear()`; the Release half
        // publishes the newly-set bit for downstream `is_marked()`
        // readers (including the next cycle's `clear()` ordering anchor).
        // Without this, on ARM/AArch64 a marker can observe a stale
        // black bit from a previous cycle and skip a live object → UAF.
        // On x86 `fetch_or` is a single `LOCK BTS`/`LOCK CMPXCHG` so the
        // upgrade is free; on ARM64 the extra LDAXR/STLXR is required
        // for correctness.
        let old = self.words[word_index].fetch_or(mask, Ordering::AcqRel);
        old & mask == 0 // true if bit was newly set
    }

    /// Check if the bit for the given address is marked.
    #[inline]
    pub fn is_marked(&self, addr: usize) -> bool {
        let Some((word_index, mask)) = self.locate(addr) else {
            return false;
        };

        // Round-5 fix (CRIT — ARM cross-cycle race): pair `clear()`'s
        // `Release` store and `try_mark`'s AcqRel `fetch_or` with an
        // `Acquire` load here so the reader observes a coherent view of
        // the bitmap across cycles on weakly-ordered platforms (ARM64).
        let word = self.words[word_index].load(Ordering::Acquire);
        word & mask != 0
    }

    /// Clear all bits (prepare for next GC cycle).
    ///
    /// **STOP-THE-WORLD ONLY**, which is what makes the ordering below the
    /// right one. Every caller in the tree runs inside a pause:
    /// `ConcurrentMarker::initial_mark` / `abort_cycle` / the post-sweep reset
    /// (`concurrent_mark.rs`), and `G1Region::reset` from G1's cleanup. A call
    /// with the world alive would erase marks out from under a running marker,
    /// which is a use-after-free rather than a torn read -- so the ordering is
    /// not what protects it, the caller's safepoint is.
    ///
    /// # Why this is no longer an atomic RMW per word
    ///
    /// It was `word.swap(0, AcqRel)` for every word, on the argument that a
    /// hypothetical future background pre-clear would need the Acquire half.
    /// That cost is not hypothetical: a `LOCK XCHG` per word over a 4 GiB old
    /// generation is 8.4M locked read-modify-writes inside the mark-start
    /// pause, and the identical structure in this crate has the measurement.
    /// `zgc::starts::ZObjectStartBits::clear_all` was the same shape and its
    /// own doc records the sibling operation it replaced at **94-96% of the
    /// mark-start pause (34 ms of 35 on a 4.6M-entry registry, 66 of 69 on a
    /// 10.8M one)**; it settled on exactly the form below.
    ///
    /// Relaxed stores plus ONE trailing `Release` fence give the resumed
    /// mutators and the next cycle's markers precisely the edge the per-word
    /// Release halves gave them, because a marker synchronises with the pause
    /// exit, not with an individual word. If a background pre-clear is ever
    /// built, it needs a phase handshake of its own -- per-word `AcqRel` was
    /// never sufficient for it either, since it does not linearise different
    /// words against each other (the old doc said so itself, and kept a
    /// trailing `SeqCst` fence to cover the gap).
    ///
    /// # The early return
    ///
    /// See [`Self::any_marked`]. A bitmap nothing marked into is already all
    /// zeroes, and G1 resets far more regions per cleanup than any pause marks
    /// into. A `false` here cannot be stale in the unsafe direction; the
    /// `Acquire` load pairs with the `Release` store `try_mark` makes before
    /// its `fetch_or`.
    pub fn clear(&self) {
        if !self.any_marked.load(Ordering::Acquire) {
            return;
        }
        for word in self.words.iter() {
            word.store(0, Ordering::Relaxed);
        }
        self.any_marked.store(false, Ordering::Relaxed);
        fence(Ordering::Release);
    }

    /// Has anything been marked into this bitmap since the last [`Self::clear`]?
    ///
    /// Exposed so a caller that would otherwise call `clear()` speculatively
    /// (G1's per-region reset) can also skip the surrounding bookkeeping, and
    /// so the invariant is testable.
    #[inline]
    pub fn any_marked(&self) -> bool {
        self.any_marked.load(Ordering::Acquire)
    }

    /// Count the total number of marked bits (for statistics).
    pub fn marked_count(&self) -> usize {
        self.words
            .iter()
            .map(|w| w.load(Ordering::Relaxed).count_ones() as usize)
            .sum()
    }

    /// The base address of the covered region.
    pub fn base_addr(&self) -> usize {
        self.base_addr
    }

    /// The size of the covered region.
    pub fn region_size(&self) -> usize {
        self.region_size
    }
}

impl std::fmt::Debug for MarkBitmap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MarkBitmap")
            .field("base_addr", &format_args!("{:#x}", self.base_addr))
            .field("region_size", &self.region_size)
            .field("num_words", &self.words.len())
            .field("marked", &self.marked_count())
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
    fn new_bitmap_all_clear() {
        let bm = MarkBitmap::new(0x1000, 4096);
        assert_eq!(bm.marked_count(), 0);
        assert!(!bm.is_marked(0x1000));
    }

    #[test]
    fn try_mark_returns_true_then_false() {
        let bm = MarkBitmap::new(0x0, 1024);
        // First mark: should succeed
        assert!(bm.try_mark(0));
        assert!(bm.is_marked(0));
        // Second mark: already set
        assert!(!bm.try_mark(0));
        assert_eq!(bm.marked_count(), 1);
    }

    #[test]
    fn mark_multiple_addresses() {
        let bm = MarkBitmap::new(0x0, 4096);
        assert!(bm.try_mark(0));
        assert!(bm.try_mark(8));
        assert!(bm.try_mark(16));
        assert!(bm.try_mark(4088)); // near end
        assert_eq!(bm.marked_count(), 4);
    }

    #[test]
    fn out_of_range_ignored() {
        let bm = MarkBitmap::new(0x1000, 1024);
        assert!(!bm.try_mark(0x0)); // below base
        assert!(!bm.try_mark(0x2000)); // above end
        assert!(!bm.is_marked(0x0));
    }

    #[test]
    fn clear_resets_all() {
        let bm = MarkBitmap::new(0x0, 1024);
        bm.try_mark(0);
        bm.try_mark(64);
        bm.try_mark(512);
        assert_eq!(bm.marked_count(), 3);
        bm.clear();
        assert_eq!(bm.marked_count(), 0);
        assert!(!bm.is_marked(0));
    }

    /// A cleared bitmap must read as empty from another thread.
    ///
    /// `clear()` is relaxed stores plus one trailing `Release` fence (it used
    /// to be an `AcqRel` swap per word; see its doc for the measurement that
    /// changed it). The property that has to hold is unchanged and is what is
    /// asserted: after a clear, no reader observes a bit from before it. The
    /// join below is the happens-before edge in this test, standing in for the
    /// stop-the-world pause exit that provides it in production.
    #[test]
    fn clear_publishes_zeroes_to_marker() {
        use std::sync::Arc;
        let bm = Arc::new(MarkBitmap::new(0x0, 4096));
        // Mark many bits, then clear in a thread, and read in another —
        // every reader observation post-clear must be `false`.
        for i in 0..512 {
            bm.try_mark(i * 8);
        }
        assert!(bm.marked_count() > 0);
        let bm_clearer = bm.clone();
        let clearer = std::thread::spawn(move || {
            bm_clearer.clear();
        });
        clearer.join().unwrap();
        // After clear+join (which provides happens-before), every bit
        // must be observed as unmarked.
        for i in 0..512 {
            assert!(!bm.is_marked(i * 8), "stale mark at {i}");
        }
        assert_eq!(bm.marked_count(), 0);
    }

    /// `clear()` may skip its sweep ONLY for a bitmap nothing has marked into.
    ///
    /// The direction that matters is the unsafe one: a bitmap with a bit set
    /// must never take the early return, because the bit would survive into the
    /// next cycle as a stale black mark and the object it names would be
    /// treated as live-then-reaped. `try_mark` sets `any_marked` BEFORE its
    /// `fetch_or` precisely so that ordering cannot invert.
    #[test]
    fn clear_skips_only_an_untouched_bitmap() {
        let bm = MarkBitmap::new(0x1000, 4096);
        assert!(!bm.any_marked(), "a fresh bitmap has nothing to clear");
        bm.clear();
        assert!(!bm.any_marked());

        // A mark that LANDED arms the flag...
        assert!(bm.try_mark(0x1000));
        assert!(bm.any_marked());
        bm.clear();
        assert!(!bm.any_marked(), "clear disarms");
        assert!(!bm.is_marked(0x1000), "and actually cleared the bit");

        // ...and a re-mark of an ALREADY-set bit must keep it armed, or the
        // second clear of a cycle would skip a populated bitmap.
        assert!(bm.try_mark(0x1008));
        assert!(!bm.try_mark(0x1008), "second mark is a no-op on the bit");
        assert!(bm.any_marked(), "but must not disarm the flag");
        bm.clear();
        assert!(!bm.is_marked(0x1008));
    }

    /// An out-of-range mark must not arm the flag into claiming work that
    /// cannot exist -- and, more importantly, must not be reported as newly
    /// marked.
    #[test]
    fn out_of_range_mark_does_not_arm_the_flag() {
        let bm = MarkBitmap::new(0x1000, 1024);
        assert!(!bm.try_mark(0x0));
        assert!(!bm.try_mark(0x9000));
        assert!(!bm.any_marked());
    }

    /// The constructor hands back a genuinely zeroed bitmap.
    ///
    /// It builds one from `vec![0u64; n]` and reinterprets the allocation as
    /// `[AtomicU64]` rather than constructing each atomic; this pins the bit
    /// pattern that conversion assumes.
    #[test]
    fn alloc_zeroed_bitmap_reads_as_empty() {
        let bm = MarkBitmap::new(0x1_0000, 1 << 20);
        assert_eq!(bm.marked_count(), 0);
        for i in (0..(1usize << 20)).step_by(4096) {
            assert!(!bm.is_marked(0x1_0000 + i), "stale bit at +{i:#x}");
        }
    }

    #[test]
    fn concurrent_marking() {
        use std::sync::Arc;
        let bm = Arc::new(MarkBitmap::new(0x0, 8192));
        let mut handles = Vec::new();

        for t in 0..4 {
            let bm = bm.clone();
            handles.push(std::thread::spawn(move || {
                let mut newly_marked = 0usize;
                for i in 0..256 {
                    let addr = ((t * 256 + i) * MARK_GRANULARITY) % 8192;
                    if bm.try_mark(addr) {
                        newly_marked += 1;
                    }
                }
                newly_marked
            }));
        }

        let total: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        // Each address should be marked exactly once (no overlap in ranges)
        assert_eq!(total, bm.marked_count());
    }
}
