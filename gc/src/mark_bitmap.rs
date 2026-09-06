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

use std::sync::atomic::{AtomicBool, Ordering};

/// Granularity of the mark bitmap: one bit per 8 bytes of heap.
/// This matches the minimum object alignment (8-byte aligned headers).
pub const MARK_GRANULARITY: usize = 8;

/// A concurrent mark bitmap backed by atomic u64 words.
///
/// Thread-safe: multiple marker threads can set bits concurrently using
/// atomic CAS operations. The bitmap is allocated once per GC cycle and
/// cleared between cycles.
pub struct MarkBitmap {
    /// The storage, the atomics, the `alloc_zeroed`, the alignment-exact
    /// `locate` and the relaxed-store `clear_all` all live in
    /// [`crate::heap_bitmap::HeapBitmap`] now. This type is the mark-bitmap
    /// VOCABULARY over it -- `try_mark` / `is_marked` / `clear` -- kept so G1's
    /// per-region bitmap and `ConcurrentMarker`'s old-gen bitmap read as they
    /// did.
    ///
    /// # What the callers gain from the swap
    ///
    /// [`crate::heap_bitmap::HeapBitmap::claim`] is `try_mark`'s exact
    /// semantics plus a plain load before the read-modify-write. That test is
    /// asked of every EDGE rather than every object, and most edges point at
    /// something already marked -- a shared graph is why marking is a traversal
    /// and not a walk. The unconditional `fetch_or` this replaces made each of
    /// those an exclusive-state acquisition of a cache line every other marking
    /// worker is also writing.
    ///
    /// It is also alignment-EXACT, which this type was not: `addr` and
    /// `addr + 4` used to land on the same bit, so an unaligned candidate
    /// silently marked its neighbour. Mark bitmaps are only ever asked about
    /// object bases, which are 8-aligned, so no live caller changes behaviour --
    /// but the direction of the difference is that a bug becomes impossible
    /// rather than merely unlikely.
    bits: crate::heap_bitmap::HeapBitmap,
    /// Has ANY bit been set since the last [`Self::clear`]?
    ///
    /// Kept HERE rather than pushed down into `HeapBitmap`, because this is the
    /// only one of the two whose clear is speculative: G1 calls `clear()` from
    /// `G1Region::reset()` for every region a cleanup frees, and a young pause
    /// marks into almost none of them. `HeapBitmap`'s other instances -- ZGC's
    /// object-start registry and its mark bits -- are cleared only when they are
    /// known to be populated, so the flag would be pure cost there and one more
    /// thing for `insert` / `set_range` / `spill` to keep in step.
    ///
    /// Set BEFORE the claim, and that order is the safety argument: a bit must
    /// never be observable without the flag already true, or `clear` would skip
    /// a populated bitmap and leave a black bit from the previous cycle -- a
    /// live object reaped.
    any_marked: AtomicBool,
}

impl MarkBitmap {
    /// Create a new zeroed bitmap covering a heap region.
    pub fn new(base_addr: usize, region_size: usize) -> Self {
        Self {
            bits: crate::heap_bitmap::HeapBitmap::labelled(base_addr, region_size, "mark"),
            any_marked: AtomicBool::new(false),
        }
    }

    /// Attempt to mark the bit for the given address. Returns `true` if the
    /// bit was newly set (was 0, now 1). Returns `false` if already marked.
    ///
    /// Lock-free and safe to call from multiple marker threads concurrently.
    #[inline]
    pub fn try_mark(&self, addr: usize) -> bool {
        // RANGE SCREEN FIRST, and it is not optional.
        //
        // `HeapBitmap::claim` SPILLS an address its grid cannot encode into an
        // overflow set and reports it newly claimed. That is right for an
        // object-start registry, where a base the grid cannot represent must
        // still be recorded and losing it would lose an object. It is wrong
        // here, and dangerously so: a mark bitmap is bounded to ONE region and
        // G1 asks every region's bitmap about addresses that belong to other
        // regions as a matter of course. Answering "newly marked" for one of
        // those puts a foreign address on the mark queue and grows the overflow
        // set without bound.
        //
        // `MarkBitmap`'s contract has always been "not mine -> false", and
        // `out_of_range_ignored` is the test that says so. It failed the moment
        // this type started delegating, which is exactly what it is for.
        if self.bits.locate(addr).is_none() {
            return false;
        }
        // Sequenced before the claim -- see `any_marked`. The read-first keeps
        // the line read-shared once the first mark of a cycle has happened.
        if !self.any_marked.load(Ordering::Relaxed) {
            self.any_marked.store(true, Ordering::Release);
        }
        self.bits.claim(addr)
    }

    /// Check if the bit for the given address is marked.
    #[inline]
    pub fn is_marked(&self, addr: usize) -> bool {
        // Screened for the same reason `try_mark` is: `contains` consults the
        // overflow set, and this type must answer for its own region only.
        self.bits.locate(addr).is_some() && self.bits.contains(addr)
    }

    /// Clear all bits (prepare for next GC cycle).
    ///
    /// **STOP-THE-WORLD ONLY.** Every caller runs inside a pause:
    /// `ConcurrentMarker::initial_mark` / `abort_cycle` / the post-sweep reset,
    /// and `G1Region::reset` from G1's cleanup. A call with the world alive
    /// would erase marks out from under a running marker, which is a
    /// use-after-free rather than a torn read -- so the ordering is not what
    /// protects it, the caller's safepoint is. `HeapBitmap::clear_all` documents
    /// the same contract and the measurement behind its relaxed stores.
    ///
    /// The early return is the one thing this adds: see [`Self::any_marked`].
    pub fn clear(&self) {
        if !self.any_marked.load(Ordering::Acquire) {
            return;
        }
        self.bits.clear_all();
        self.any_marked.store(false, Ordering::Relaxed);
    }

    /// Has anything been marked into this bitmap since the last [`Self::clear`]?
    #[inline]
    pub fn any_marked(&self) -> bool {
        self.any_marked.load(Ordering::Acquire)
    }

    /// Count the total number of marked bits (for statistics).
    pub fn marked_count(&self) -> usize {
        (0..self.bits.word_count())
            .map(|w| self.bits.word_at(w).count_ones() as usize)
            .sum()
    }

    /// The base address of the covered region.
    pub fn base_addr(&self) -> usize {
        self.bits.base
    }

    /// The size of the covered region.
    pub fn region_size(&self) -> usize {
        self.bits.span
    }
}

impl std::fmt::Debug for MarkBitmap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MarkBitmap")
            .field("base_addr", &format_args!("{:#x}", self.base_addr()))
            .field("region_size", &self.region_size())
            .field("num_words", &self.bits.word_count())
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

    /// An address outside this bitmap's region must not be recorded ANYWHERE.
    ///
    /// The companion to `out_of_range_ignored`, and the reason it is a separate
    /// test: that one checks the return value, this one checks that nothing was
    /// stored. `HeapBitmap::claim` spills an unencodable address into an
    /// overflow set — correct for an object-start registry, where losing such a
    /// base loses an object, and wrong for a bounded mark bitmap that G1 asks
    /// about other regions' addresses as a matter of course. Without the range
    /// screen in `try_mark` the overflow set would grow without bound and a
    /// foreign address would be reported newly marked.
    #[test]
    fn an_out_of_region_mark_does_not_reach_the_overflow_set() {
        let bm = MarkBitmap::new(0x1_0000, 4096);
        assert!(!bm.try_mark(0x0), "below the region");
        assert!(!bm.try_mark(0x9_0000), "above the region");
        assert!(!bm.try_mark(0x1_0004), "inside, but not on the 8-byte grid");
        assert!(
            !bm.bits.has_spill(),
            "a mark bitmap is bounded to one region; nothing outside it may be              recorded, and an overflow entry is both a wrong answer and an              unbounded leak"
        );
        assert_eq!(bm.marked_count(), 0);
        assert!(!bm.any_marked());
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
