//! Card table for the generational garbage collector.
//!
//! The card table tracks which regions of the old generation contain
//! references to young-generation objects. Each "card" covers a fixed-size
//! region (512 bytes). When a reference store from old gen to young gen is
//! detected by the write barrier, the corresponding card is marked dirty.
//!
//! During a minor GC, only dirty cards need to be scanned for old→young
//! references, avoiding a full scan of the old generation.

use parking_lot::Mutex;
use std::cell::RefCell;

/// Number of bytes covered by a single card.
pub const CARD_SIZE: usize = 512;

/// Card state: no old→young references detected since last GC.
pub const CARD_CLEAN: u8 = 0;

/// Card state: a reference store from old gen to young gen occurred.
pub const CARD_DIRTY: u8 = 1;

/// T5.5.2 — number of entries after which the per-thread buffer is
/// auto-flushed into the shared card table.
pub const THREAD_BUFFER_FLUSH_THRESHOLD: usize = 64;

thread_local! {
    /// T5.5.2 — per-thread write buffer of byte offsets (into the
    /// region covered by the card table) that have been dirtied.
    ///
    /// Mutators write here on the fast path instead of touching the
    /// shared `CardTable`. When the buffer fills or a safepoint fires,
    /// [`CardTable::flush_dirty_buffer`] drains it into the
    /// shared-side `pending_offsets` vector under the mutex.
    static THREAD_DIRTY_BUFFER: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// A byte-map card table for tracking old→young cross-generation references.
///
/// Each byte covers `CARD_SIZE` bytes of the old generation's address space.
/// The write barrier marks cards dirty; the minor GC scans dirty cards and
/// then clears them.
pub struct CardTable {
    /// One byte per card (CARD_CLEAN or CARD_DIRTY).
    cards: Vec<u8>,
    /// Base address of the memory region this table covers.
    base_addr: usize,
    /// Total size of the covered region in bytes.
    region_size: usize,
    /// Tracking list of card indices that have been dirtied since the last scan.
    /// Updated on `mark_dirty`, cleared on `clear_all` or `take_dirty_cards`.
    dirty_cards: Vec<usize>,
    /// T5.5.2 — pending byte offsets submitted by thread-local buffers
    /// via [`CardTable::flush_dirty_buffer`], waiting to be folded into
    /// `cards`/`dirty_cards` at the next safepoint.
    pending_offsets: Mutex<Vec<usize>>,
}

impl CardTable {
    /// Create a new card table covering a memory region starting at `base_addr`
    /// with the given `region_size` in bytes.
    pub fn new(base_addr: usize, region_size: usize) -> Self {
        let num_cards = region_size.div_ceil(CARD_SIZE);
        Self {
            cards: vec![CARD_CLEAN; num_cards],
            base_addr,
            region_size,
            dirty_cards: Vec::new(),
            pending_offsets: Mutex::new(Vec::new()),
        }
    }

    /// Mark the card containing `addr` as dirty.
    ///
    /// Addresses outside the covered region are silently ignored. This is safe
    /// because the write barrier may fire for allocations that straddle region
    /// boundaries during concurrent GC promotion.
    #[inline]
    pub fn mark_dirty(&mut self, addr: usize) {
        if addr < self.base_addr || addr >= self.base_addr + self.region_size {
            return;
        }
        let index = (addr - self.base_addr) / CARD_SIZE;
        if index < self.cards.len() {
            if self.cards[index] != CARD_DIRTY {
                self.cards[index] = CARD_DIRTY;
                self.dirty_cards.push(index);
            }
        }
    }

    /// Check if a specific card is dirty.
    pub fn is_dirty(&self, card_index: usize) -> bool {
        self.cards.get(card_index).copied() == Some(CARD_DIRTY)
    }

    /// Clear all cards (set to CARD_CLEAN).
    pub fn clear_all(&mut self) {
        self.cards.fill(CARD_CLEAN);
        self.dirty_cards.clear();
        self.pending_offsets.get_mut().clear();
    }

    /// Iterate over indices of dirty cards.
    ///
    /// Note: For bulk processing, prefer [`take_dirty_cards`] which uses the
    /// O(dirty) tracking list instead of scanning all cards.
    pub fn dirty_card_indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.cards
            .iter()
            .enumerate()
            .filter(|(_, &card)| card == CARD_DIRTY)
            .map(|(i, _)| i)
    }

    /// Return the tracked list of dirty card indices and clear the tracking list.
    ///
    /// This is O(dirty cards) rather than O(total cards), making it much faster
    /// when only a small fraction of cards are dirty.
    pub fn take_dirty_cards(&mut self) -> Vec<usize> {
        std::mem::take(&mut self.dirty_cards)
    }

    /// Get the start address of the region covered by `card_index`.
    ///
    /// Uses checked arithmetic to avoid overflow on extremely large card indices.
    /// Returns `None` if the computation would overflow.
    pub fn card_start_addr(&self, card_index: usize) -> usize {
        let offset = card_index.checked_mul(CARD_SIZE).unwrap_or_else(|| {
            tracing::error!("card_table: card_start_addr overflow in card_index({}) * CARD_SIZE({})", card_index, CARD_SIZE);
            // Return a clamped value instead of aborting the entire VM.
            // The caller should never pass an index this large; this is a
            // defensive fallback so GC can skip the card instead of crashing.
            usize::MAX
        });
        self.base_addr.saturating_add(offset)
    }

    /// Get the exclusive end address of the region covered by `card_index`.
    ///
    /// Uses checked arithmetic to avoid overflow on extremely large card indices.
    /// Clamps the result to `base_addr + region_size`.
    pub fn card_end_addr(&self, card_index: usize) -> usize {
        let next_index = card_index.saturating_add(1);
        let offset = next_index.checked_mul(CARD_SIZE).unwrap_or(usize::MAX);
        let end = self
            .base_addr
            .checked_add(offset)
            .unwrap_or(usize::MAX);
        let region_end = self
            .base_addr
            .saturating_add(self.region_size);
        end.min(region_end)
    }

    /// The number of cards in this table.
    pub fn num_cards(&self) -> usize {
        self.cards.len()
    }

    /// The base address of the covered region.
    pub fn base_addr(&self) -> usize {
        self.base_addr
    }

    /// The size of the covered region.
    pub fn region_size(&self) -> usize {
        self.region_size
    }

    // -----------------------------------------------------------------
    // T5.5.2 — thread-local batched dirty path
    // -----------------------------------------------------------------

    /// T5.5.2 — fast path for the write barrier.
    ///
    /// Appends `offset` (a byte offset from [`Self::base_addr`]) to the
    /// calling thread's private buffer. This path takes no lock; it is
    /// intended to be called from the mutator for every cross-generation
    /// reference store. When the buffer reaches
    /// [`THREAD_BUFFER_FLUSH_THRESHOLD`] entries it auto-flushes into
    /// the shared pending list via [`Self::flush_dirty_buffer`].
    ///
    /// The final update to the shared [`cards`](Self::cards) bitmap is
    /// deferred to a safepoint, where a GC thread calls
    /// [`Self::drain_pending`] with `&mut self`.
    pub fn thread_local_dirty(&self, offset: usize) {
        let should_flush = THREAD_DIRTY_BUFFER.with(|buf| {
            let mut b = buf.borrow_mut();
            b.push(offset);
            b.len() >= THREAD_BUFFER_FLUSH_THRESHOLD
        });
        if should_flush {
            self.flush_dirty_buffer();
        }
    }

    /// T5.5.2 — Flush the calling thread's private dirty buffer into
    /// the shared pending list. Called on safepoint entry and whenever
    /// a per-thread buffer hits the auto-flush threshold.
    ///
    /// The offsets themselves are *not* yet resolved to card indices —
    /// that happens in [`Self::drain_pending`] under `&mut self`, so
    /// the hot-path cost remains a single lock acquisition.
    pub fn flush_dirty_buffer(&self) {
        THREAD_DIRTY_BUFFER.with(|buf| {
            let mut local = buf.borrow_mut();
            if local.is_empty() {
                return;
            }
            let mut shared = self.pending_offsets.lock();
            shared.reserve(local.len());
            shared.append(&mut *local);
        });
    }

    /// T5.5.2 — Fold any thread-submitted offsets from
    /// [`Self::pending_offsets`] into the authoritative `cards` bitmap
    /// and the tracking `dirty_cards` list.
    ///
    /// This is the single pass promised in the design: at a safepoint
    /// the GC thread calls this once and every offset becomes a card
    /// update in O(pending) time.
    ///
    /// Returns the number of distinct new cards that became dirty.
    pub fn drain_pending(&mut self) -> usize {
        let pending = std::mem::take(&mut *self.pending_offsets.get_mut());
        let mut newly_dirtied = 0usize;
        for offset in pending {
            let addr = self.base_addr.wrapping_add(offset);
            if addr < self.base_addr || addr >= self.base_addr + self.region_size {
                continue;
            }
            let index = (addr - self.base_addr) / CARD_SIZE;
            if index < self.cards.len() && self.cards[index] != CARD_DIRTY {
                self.cards[index] = CARD_DIRTY;
                self.dirty_cards.push(index);
                newly_dirtied += 1;
            }
        }
        newly_dirtied
    }

    /// T5.5.2 — How many offsets are currently queued in the shared
    /// pending list waiting to be drained.
    pub fn pending_count(&self) -> usize {
        self.pending_offsets.lock().len()
    }

    /// T5.5.2 — Convenience wrapper: convert a raw address to an
    /// offset and route it through the thread-local fast path.
    ///
    /// Out-of-range addresses are silently dropped, matching the
    /// behaviour of [`Self::mark_dirty`].
    pub fn thread_local_dirty_addr(&self, addr: usize) {
        if addr < self.base_addr || addr >= self.base_addr + self.region_size {
            return;
        }
        self.thread_local_dirty(addr - self.base_addr);
    }
}

impl std::fmt::Debug for CardTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let dirty_count = self.cards.iter().filter(|&&c| c == CARD_DIRTY).count();
        f.debug_struct("CardTable")
            .field("num_cards", &self.cards.len())
            .field("dirty_cards", &dirty_count)
            .field("base_addr", &format_args!("{:#x}", self.base_addr))
            .field("region_size", &self.region_size)
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
    fn new_card_table_all_clean() {
        let ct = CardTable::new(0x1000, 4096);
        assert_eq!(ct.num_cards(), 8); // 4096 / 512
        for i in 0..ct.num_cards() {
            assert!(!ct.is_dirty(i));
        }
    }

    #[test]
    fn mark_dirty_and_check() {
        let mut ct = CardTable::new(0x1000, 4096);
        // Mark card covering address 0x1000 (card 0)
        ct.mark_dirty(0x1000);
        assert!(ct.is_dirty(0));
        assert!(!ct.is_dirty(1));

        // Mark card covering address 0x1400 (offset 0x400 = 1024, card 2)
        ct.mark_dirty(0x1400);
        assert!(ct.is_dirty(2));
    }

    #[test]
    fn mark_dirty_last_byte_of_card() {
        let mut ct = CardTable::new(0x0, 2048);
        // Last byte of card 0: offset 511
        ct.mark_dirty(511);
        assert!(ct.is_dirty(0));
        assert!(!ct.is_dirty(1));

        // First byte of card 1: offset 512
        ct.mark_dirty(512);
        assert!(ct.is_dirty(1));
    }

    #[test]
    fn clear_all() {
        let mut ct = CardTable::new(0x0, 2048);
        ct.mark_dirty(0);
        ct.mark_dirty(512);
        ct.mark_dirty(1024);
        assert_eq!(ct.dirty_card_indices().count(), 3);

        ct.clear_all();
        assert_eq!(ct.dirty_card_indices().count(), 0);
        for i in 0..ct.num_cards() {
            assert!(!ct.is_dirty(i));
        }
    }

    #[test]
    fn dirty_card_indices() {
        let mut ct = CardTable::new(0x0, 4096);
        ct.mark_dirty(0); // card 0
        ct.mark_dirty(1536); // card 3 (1536 / 512 = 3)
        ct.mark_dirty(3584); // card 7 (3584 / 512 = 7)

        let dirty: Vec<usize> = ct.dirty_card_indices().collect();
        assert_eq!(dirty, vec![0, 3, 7]);
    }

    #[test]
    fn card_start_and_end_addr() {
        let ct = CardTable::new(0x1000, 2048);
        assert_eq!(ct.card_start_addr(0), 0x1000);
        assert_eq!(ct.card_end_addr(0), 0x1000 + 512);
        assert_eq!(ct.card_start_addr(1), 0x1000 + 512);
        assert_eq!(ct.card_end_addr(1), 0x1000 + 1024);

        // Last card's end should be clamped to base + region_size
        assert_eq!(ct.card_end_addr(3), 0x1000 + 2048);
    }

    #[test]
    fn non_aligned_region_size() {
        // 1000 bytes → 2 cards (ceil(1000/512) = 2)
        let ct = CardTable::new(0x0, 1000);
        assert_eq!(ct.num_cards(), 2);
        // Last card's end is clamped
        assert_eq!(ct.card_end_addr(1), 1000);
    }

    #[test]
    fn is_dirty_out_of_bounds_returns_false() {
        let ct = CardTable::new(0x0, 1024);
        assert!(!ct.is_dirty(999));
    }

    // -----------------------------------------------------------------------
    // Additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn mark_dirty_outside_region_is_ignored() {
        let mut ct = CardTable::new(0x1000, 1024);
        // Address below base
        ct.mark_dirty(0x0);
        // Address above region
        ct.mark_dirty(0x1000 + 1024);
        ct.mark_dirty(0xFFFF);

        // No cards should be dirty
        assert_eq!(ct.dirty_card_indices().count(), 0);
    }

    #[test]
    fn mark_dirty_first_card() {
        let mut ct = CardTable::new(0x0, 4096);
        ct.mark_dirty(0);
        assert!(ct.is_dirty(0));
        assert!(!ct.is_dirty(1));
    }

    #[test]
    fn mark_dirty_last_card() {
        let mut ct = CardTable::new(0x0, 4096);
        // Last card covers bytes 3584..4095
        ct.mark_dirty(4095);
        let last_card = ct.num_cards() - 1;
        assert!(ct.is_dirty(last_card));
    }

    #[test]
    fn dirty_multiple_regions() {
        let mut ct = CardTable::new(0x0, 8192);
        // Dirty cards 0, 3, 7, 15
        ct.mark_dirty(0);       // card 0
        ct.mark_dirty(1536);    // card 3
        ct.mark_dirty(3584);    // card 7
        ct.mark_dirty(7680);    // card 15

        let dirty: Vec<usize> = ct.dirty_card_indices().collect();
        assert_eq!(dirty, vec![0, 3, 7, 15]);
    }

    #[test]
    fn clear_then_dirty_again() {
        let mut ct = CardTable::new(0x0, 2048);
        ct.mark_dirty(0);
        ct.mark_dirty(512);
        assert_eq!(ct.dirty_card_indices().count(), 2);

        ct.clear_all();
        assert_eq!(ct.dirty_card_indices().count(), 0);

        // Re-dirty after clear
        ct.mark_dirty(1024);
        let dirty: Vec<usize> = ct.dirty_card_indices().collect();
        assert_eq!(dirty, vec![2]);
    }

    #[test]
    fn card_addresses_with_nonzero_base() {
        let base = 0x8000usize;
        let ct = CardTable::new(base, 2048);

        assert_eq!(ct.card_start_addr(0), 0x8000);
        assert_eq!(ct.card_end_addr(0), 0x8000 + CARD_SIZE);
        assert_eq!(ct.card_start_addr(1), 0x8000 + CARD_SIZE);
        assert_eq!(ct.card_end_addr(1), 0x8000 + 2 * CARD_SIZE);
    }

    #[test]
    fn mark_same_card_idempotent() {
        let mut ct = CardTable::new(0x0, 4096);
        ct.mark_dirty(100);
        ct.mark_dirty(200);
        ct.mark_dirty(300);
        // All within card 0 (offsets 0..511)
        assert!(ct.is_dirty(0));
        assert_eq!(ct.dirty_card_indices().count(), 1);
    }

    #[test]
    fn all_cards_dirty() {
        let mut ct = CardTable::new(0x0, 2048);
        let num = ct.num_cards();
        for i in 0..num {
            ct.mark_dirty(i * CARD_SIZE);
        }
        assert_eq!(ct.dirty_card_indices().count(), num);
    }

    #[test]
    fn single_byte_region() {
        let ct = CardTable::new(0x0, 1);
        assert_eq!(ct.num_cards(), 1);
        assert_eq!(ct.card_end_addr(0), 1);
    }

    #[test]
    fn card_size_boundary_region() {
        // Region exactly CARD_SIZE bytes
        let ct = CardTable::new(0x0, CARD_SIZE);
        assert_eq!(ct.num_cards(), 1);
        assert_eq!(ct.card_start_addr(0), 0);
        assert_eq!(ct.card_end_addr(0), CARD_SIZE);
    }

    #[test]
    fn card_size_plus_one_region() {
        let ct = CardTable::new(0x0, CARD_SIZE + 1);
        assert_eq!(ct.num_cards(), 2);
        assert_eq!(ct.card_end_addr(1), CARD_SIZE + 1);
    }

    #[test]
    fn mark_dirty_at_exact_base() {
        let mut ct = CardTable::new(0x5000, 4096);
        ct.mark_dirty(0x5000);
        assert!(ct.is_dirty(0));
    }

    #[test]
    fn mark_dirty_at_region_end_minus_one() {
        let mut ct = CardTable::new(0x5000, 4096);
        ct.mark_dirty(0x5000 + 4095);
        let last = ct.num_cards() - 1;
        assert!(ct.is_dirty(last));
    }

    #[test]
    fn mark_dirty_at_exact_region_end_is_ignored() {
        let mut ct = CardTable::new(0x5000, 4096);
        // Exactly at base + region_size is out of range
        ct.mark_dirty(0x5000 + 4096);
        assert_eq!(ct.dirty_card_indices().count(), 0);
    }

    // -----------------------------------------------------------------
    // T5.5.2 — Thread-local batched dirty tests
    // -----------------------------------------------------------------

    #[test]
    fn thread_local_dirty_accumulates_without_touching_cards() {
        let mut ct = CardTable::new(0x0, 8192);
        // Dirty one offset via the fast path — stays in the thread-local
        // buffer; the shared table sees nothing until a flush.
        ct.thread_local_dirty(0);
        // Not enough to auto-flush.
        assert_eq!(ct.pending_count(), 0);
        assert_eq!(ct.dirty_card_indices().count(), 0);

        // Explicit flush + drain.
        ct.flush_dirty_buffer();
        assert_eq!(ct.pending_count(), 1);
        let newly = ct.drain_pending();
        assert_eq!(newly, 1);
        assert!(ct.is_dirty(0));
    }

    #[test]
    fn thread_local_dirty_auto_flushes_at_threshold() {
        // Use a large region so the threshold-worth of distinct card
        // offsets is well-defined.
        let mut ct = CardTable::new(
            0x0,
            CARD_SIZE * THREAD_BUFFER_FLUSH_THRESHOLD * 2,
        );
        // Push exactly THREAD_BUFFER_FLUSH_THRESHOLD offsets — the last
        // one should auto-flush the buffer.
        for i in 0..THREAD_BUFFER_FLUSH_THRESHOLD {
            ct.thread_local_dirty(i * CARD_SIZE);
        }
        assert_eq!(ct.pending_count(), THREAD_BUFFER_FLUSH_THRESHOLD);

        let newly = ct.drain_pending();
        assert_eq!(newly, THREAD_BUFFER_FLUSH_THRESHOLD);
        for i in 0..THREAD_BUFFER_FLUSH_THRESHOLD {
            assert!(ct.is_dirty(i));
        }
    }

    #[test]
    fn flush_dirty_buffer_is_idempotent_when_empty() {
        let ct = CardTable::new(0x0, 4096);
        ct.flush_dirty_buffer();
        assert_eq!(ct.pending_count(), 0);
        ct.flush_dirty_buffer();
        assert_eq!(ct.pending_count(), 0);
    }

    #[test]
    fn drain_pending_deduplicates_same_card() {
        let mut ct = CardTable::new(0x0, 4096);
        // All offsets within card 0.
        ct.thread_local_dirty(0);
        ct.thread_local_dirty(100);
        ct.thread_local_dirty(500);
        ct.flush_dirty_buffer();
        let newly = ct.drain_pending();
        // Three offsets but only one new card.
        assert_eq!(newly, 1);
        assert!(ct.is_dirty(0));
        assert_eq!(ct.dirty_card_indices().count(), 1);
    }

    #[test]
    fn drain_pending_rejects_out_of_range_offset() {
        let mut ct = CardTable::new(0x0, 1024);
        // Offset past region_size — should be silently dropped.
        ct.thread_local_dirty(1024);
        ct.thread_local_dirty(99999);
        ct.flush_dirty_buffer();
        let newly = ct.drain_pending();
        assert_eq!(newly, 0);
    }

    #[test]
    fn thread_local_dirty_addr_wraps_address_to_offset() {
        let mut ct = CardTable::new(0x10_0000, 4096);
        ct.thread_local_dirty_addr(0x10_0000 + 513); // card 1
        ct.flush_dirty_buffer();
        let newly = ct.drain_pending();
        assert_eq!(newly, 1);
        assert!(ct.is_dirty(1));
    }

    #[test]
    fn clear_all_also_resets_pending() {
        let mut ct = CardTable::new(0x0, 4096);
        ct.thread_local_dirty(0);
        ct.flush_dirty_buffer();
        assert_eq!(ct.pending_count(), 1);
        ct.clear_all();
        assert_eq!(ct.pending_count(), 0);
    }
}
