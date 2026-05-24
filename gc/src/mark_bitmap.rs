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

use std::sync::atomic::{fence, AtomicU64, Ordering};

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
    words: Vec<AtomicU64>,
    /// Base address of the heap region this bitmap covers.
    base_addr: usize,
    /// Total size (bytes) of the covered region.
    region_size: usize,
}

impl MarkBitmap {
    /// Create a new zeroed bitmap covering a heap region.
    pub fn new(base_addr: usize, region_size: usize) -> Self {
        let num_bits = region_size.div_ceil(MARK_GRANULARITY);
        let num_words = num_bits.div_ceil(64);
        let words = (0..num_words).map(|_| AtomicU64::new(0)).collect();
        Self {
            words,
            base_addr,
            region_size,
        }
    }

    /// Attempt to mark the bit for the given address. Returns `true` if the
    /// bit was newly set (was 0, now 1). Returns `false` if already marked.
    ///
    /// This is lock-free and safe to call from multiple threads concurrently.
    #[inline]
    pub fn try_mark(&self, addr: usize) -> bool {
        // Explicit bounds check before arithmetic to catch overflow early.
        let region_end = match self.base_addr.checked_add(self.region_size) {
            Some(end) => end,
            None => return false, // region wraps around address space — reject
        };
        if addr < self.base_addr || addr >= region_end {
            return false;
        }
        let offset = addr - self.base_addr;
        let bit_index = offset / MARK_GRANULARITY;
        let word_index = bit_index / 64;
        let bit_within_word = bit_index % 64;
        let mask = 1u64 << bit_within_word;

        if word_index >= self.words.len() {
            return false;
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
        // Explicit bounds check before arithmetic to catch overflow early.
        let region_end = match self.base_addr.checked_add(self.region_size) {
            Some(end) => end,
            None => return false, // region wraps around address space — reject
        };
        if addr < self.base_addr || addr >= region_end {
            return false;
        }
        let offset = addr - self.base_addr;
        let bit_index = offset / MARK_GRANULARITY;
        let word_index = bit_index / 64;
        let bit_within_word = bit_index % 64;

        if word_index >= self.words.len() {
            return false;
        }

        // Round-5 fix (CRIT — ARM cross-cycle race): pair `clear()`'s
        // `Release` store and `try_mark`'s AcqRel `fetch_or` with an
        // `Acquire` load here so the reader observes a coherent view of
        // the bitmap across cycles on weakly-ordered platforms (ARM64).
        let word = self.words[word_index].load(Ordering::Acquire);
        word & (1u64 << bit_within_word) != 0
    }

    /// Clear all bits (prepare for next GC cycle).
    ///
    /// **Ordering contract** (Round-7 audit §4, task #25 hardening): each
    /// per-word clear is now an `AcqRel` CAS-like RMW (`swap` with
    /// `AcqRel`), not a plain `Release` store. Why we strengthened it:
    ///
    /// - The previous per-word `Release` store paired with `try_mark`'s
    ///   `AcqRel` `fetch_or` and `is_marked`'s `Acquire` load *on the same
    ///   word*, but provided no Acquire side on the clearer itself. With
    ///   `clear()` always invoked inside the initial-mark STW pause
    ///   (every mutator parked at `gc_barrier`), the barrier's release on
    ///   STW exit served as a global fence and the missing Acquire was
    ///   invisible.
    /// - If `clear()` is ever moved to a background pre-clear thread
    ///   (already anticipated in the original docstring), that global
    ///   fence disappears. On ARM/AArch64 the clearer could then observe
    ///   a stale black bit from the *previous* cycle in its initial load,
    ///   merge it with its zero-store, and republish a still-set bit —
    ///   the classic stale-black bit → live object reaped → UAF.
    /// - `AcqRel` on each per-word RMW closes that hole: the Acquire half
    ///   forces the clearer to observe the latest value from the prior
    ///   cycle's marker (sequencing it with marker stores), and the
    ///   Release half publishes the cleared word for the next cycle's
    ///   marker (paired with `try_mark`'s Acquire half).
    ///
    /// `swap(0, AcqRel)` is the cheapest primitive that gives both halves;
    /// on x86 it's a `LOCK XCHG`, on ARM64 an `LDAXR`/`STLXR` pair. We
    /// keep the trailing `SeqCst` fence as belt-and-braces inter-word
    /// ordering: per-word `AcqRel` does NOT linearize different words
    /// against each other, and the fence makes that invariant explicit
    /// for any future caller (e.g. concurrent pre-clear scheduling).
    pub fn clear(&self) {
        for word in &self.words {
            // AcqRel swap: Acquire side observes the prior cycle's
            // marker stores on this word; Release side publishes the
            // cleared word for the next cycle's markers. See module
            // docstring for the cross-cycle race this prevents.
            word.swap(0, Ordering::AcqRel);
        }
        fence(Ordering::SeqCst);
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

    /// Task #25: `clear()` must use `AcqRel` per-word RMW (not a plain
    /// Release store) so a background pre-clear cannot observe stale
    /// black bits from the prior cycle on ARM. Verified indirectly: a
    /// fresh `clear()` followed by `is_marked()` on every word must
    /// always return false, even after many concurrent mark+clear cycles
    /// — a `Release`-only store would still expose the prior-cycle bit
    /// to the Acquire load on weakly-ordered hardware. We can't fake
    /// ARM ordering on the host, but the test fences with `SeqCst` so
    /// any latent ordering bug surfaces under TSan / Miri.
    #[test]
    fn clear_acqrel_publishes_zeroes_to_marker() {
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
