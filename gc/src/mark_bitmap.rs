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

use std::sync::atomic::{AtomicU64, Ordering};

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

        let word = self.words[word_index].load(Ordering::Acquire);
        word & (1u64 << bit_within_word) != 0
    }

    /// Clear all bits (prepare for next GC cycle).
    pub fn clear(&self) {
        for word in &self.words {
            word.store(0, Ordering::Release);
        }
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
