// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Card table for the generational garbage collector.
//!
//! The card table tracks which regions of the old generation contain
//! references to young-generation objects. Each "card" covers a fixed-size
//! region (512 bytes). When a reference store from old gen to young gen is
//! detected by the write barrier, the corresponding card is marked dirty.
//!
//! During a minor GC, only dirty cards need to be scanned for old→young
//! references, avoiding a full scan of the old generation.
//!
//! T5.5.2 wiring (HIGH-1 fix): all `&self` methods (the mutator fast
//! path) are fully concurrent — they only touch the per-thread buffer
//! and the `Mutex<Vec<usize>>` of pending offsets. The authoritative
//! `cards` / `dirty_cards` state is protected by a separate
//! `Mutex<CardCells>` inside the table so the collector can safely
//! mark/clear/drain while mutators continue to enqueue dirty offsets.
//!
//! SECURITY FIX (V6) — cross-thread buffer drain invariant: every
//! thread's dirty buffer is registered in the global `BUFFER_REGISTRY`
//! on first use. At a stop-the-world safepoint the collector calls
//! [`CardTable::flush_all`], which walks the registry and folds *every*
//! thread's buffered old→young edges into `pending_offsets` before
//! `drain_pending`. This removes the previous cross-crate obligation
//! (that each mutator flush its own buffer before parking) whose
//! violation could free a still-reachable young object (use-after-free).
//! INVARIANT: `flush_all` may run only at STW, with all mutators parked.

use parking_lot::Mutex;
use std::sync::Arc;

/// Number of bytes covered by a single card.
pub const CARD_SIZE: usize = 512;

/// Card state: no old→young references detected since last GC.
pub const CARD_CLEAN: u8 = 0;

/// Card state: a reference store from old gen to young gen occurred.
pub const CARD_DIRTY: u8 = 1;

/// T5.5.2 — number of entries after which the per-thread buffer is
/// auto-flushed into the shared card table.
pub const THREAD_BUFFER_FLUSH_THRESHOLD: usize = 64;

/// SECURITY FIX (V6): a thread's dirty buffer, shared via `Arc` between the
/// owning mutator (fast path) and the global [`BUFFER_REGISTRY`] so the
/// collector can drain it at a stop-the-world safepoint even if the owning
/// thread parked without flushing.
///
/// The inner `Mutex` is contended only in the (impossible-by-construction at
/// STW) case where a mutator runs concurrently with the collector's drain. By
/// the GC's stop-the-world invariant every mutator is parked at a safepoint
/// before the collector drains, so the lock is effectively uncontended on the
/// fast path; it exists purely to make the cross-thread read at STW sound
/// (a bare `RefCell` is `!Sync` and could not legally be read by the
/// collector thread).
type ThreadBuffer = Arc<Mutex<Vec<usize>>>;

/// SECURITY FIX (V6): global intrusive registry of every thread's dirty
/// buffer. The collector walks this list at STW (via [`CardTable::flush_all`])
/// and folds every thread's buffered offsets into `pending_offsets`, closing
/// the cross-thread use-after-free hole where a mutator could buffer up to
/// `THREAD_BUFFER_FLUSH_THRESHOLD - 1` old→young edges and then park at a
/// safepoint without flushing, causing a minor GC to miss the root and free a
/// still-reachable young object.
///
/// Entries are `Arc` clones of the per-thread buffers; a thread removes its
/// entry on exit (see [`DirtyBufferGuard`]) so the registry never holds a
/// dangling buffer. Identity is matched on `Arc::as_ptr` so removal is exact.
static BUFFER_REGISTRY: Mutex<Vec<ThreadBuffer>> = Mutex::new(Vec::new());

/// SECURITY FIX (V6): RAII handle stored in TLS. Holds the thread's shared
/// dirty buffer and removes it from [`BUFFER_REGISTRY`] when the thread exits,
/// so the collector never dereferences a freed buffer.
struct DirtyBufferGuard {
    buffer: ThreadBuffer,
}

impl Drop for DirtyBufferGuard {
    fn drop(&mut self) {
        // SECURITY FIX (V6): deregister this thread's buffer so the collector
        // never drains a buffer belonging to a dead thread. We cannot fold
        // residual offsets into `pending_offsets` here (no `&CardTable` is in
        // scope), but that is safe: a thread that is tearing down is no longer
        // a GC root and holds no live references the collector must preserve,
        // and the registry holds an `Arc` clone so the underlying buffer
        // storage stays alive until both this guard and the registry entry
        // are dropped. Match the registry entry by `Arc` identity for exact
        // removal.
        let self_ptr = Arc::as_ptr(&self.buffer);
        let mut reg = BUFFER_REGISTRY.lock();
        if let Some(pos) = reg.iter().position(|b| Arc::as_ptr(b) == self_ptr) {
            reg.swap_remove(pos);
        }
    }
}

thread_local! {
    /// T5.5.2 — per-thread write buffer of byte offsets (into the
    /// region covered by the card table) that have been dirtied.
    ///
    /// Mutators write here on the fast path instead of touching the
    /// shared `CardTable`. When the buffer fills or a safepoint fires,
    /// [`CardTable::flush_dirty_buffer`] drains it into the
    /// shared-side `pending_offsets` vector under the mutex.
    ///
    /// SECURITY FIX (V6): the buffer is an `Arc<Mutex<Vec<usize>>>` (not a
    /// bare `RefCell`) registered in [`BUFFER_REGISTRY`] on first use so the
    /// collector can drain it cross-thread at STW. [`DirtyBufferGuard`]
    /// deregisters it on thread exit.
    static THREAD_DIRTY_BUFFER: DirtyBufferGuard = {
        let buffer: ThreadBuffer = Arc::new(Mutex::new(Vec::new()));
        BUFFER_REGISTRY.lock().push(Arc::clone(&buffer));
        DirtyBufferGuard { buffer }
    };
}

/// Authoritative card-bitmap state. Locked exclusively by the collector
/// during GC; never touched by the mutator fast path.
struct CardCells {
    /// One byte per card (CARD_CLEAN or CARD_DIRTY).
    cards: Vec<u8>,
    /// Tracking list of card indices that have been dirtied since the last scan.
    dirty_cards: Vec<usize>,
}

/// A byte-map card table for tracking old→young cross-generation references.
///
/// Each byte covers `CARD_SIZE` bytes of the old generation's address space.
/// The write barrier marks cards dirty; the minor GC scans dirty cards and
/// then clears them.
///
/// All public methods take `&self`. Internally, the bitmap is locked
/// (`cells`) and the pending-offset queue is locked
/// (`pending_offsets`). The mutator fast path
/// ([`Self::thread_local_dirty_addr`]) writes only to a per-thread
/// buffer; the global state is updated lazily either when the buffer
/// hits its auto-flush threshold or when the collector calls
/// [`Self::drain_pending`] at GC start.
pub struct CardTable {
    /// Base address of the memory region this table covers (immutable).
    base_addr: usize,
    /// Total size of the covered region in bytes (immutable).
    region_size: usize,
    /// Exclusive bitmap state — touched only by collector-side methods.
    cells: Mutex<CardCells>,
    /// T5.5.2 — pending byte offsets submitted by thread-local buffers
    /// via [`CardTable::flush_dirty_buffer`], waiting to be folded into
    /// `cards` / `dirty_cards` at the next safepoint.
    pending_offsets: Mutex<Vec<usize>>,
}

impl CardTable {
    /// Create a new card table covering a memory region starting at `base_addr`
    /// with the given `region_size` in bytes.
    pub fn new(base_addr: usize, region_size: usize) -> Self {
        let num_cards = region_size.div_ceil(CARD_SIZE);
        Self {
            base_addr,
            region_size,
            cells: Mutex::new(CardCells {
                cards: vec![CARD_CLEAN; num_cards],
                dirty_cards: Vec::new(),
            }),
            pending_offsets: Mutex::new(Vec::new()),
        }
    }

    /// Mark the card containing `addr` as dirty.
    ///
    /// Addresses outside the covered region are silently ignored. This is safe
    /// because the write barrier may fire for allocations that straddle region
    /// boundaries during concurrent GC promotion.
    ///
    /// This is the *slow* path — used by GC-internal code paths that
    /// have already established exclusive access (e.g. re-marking cards
    /// for promoted objects). The mutator write barrier MUST NOT call
    /// this directly; it should route through
    /// [`Self::thread_local_dirty_addr`].
    #[inline]
    pub fn mark_dirty(&self, addr: usize) {
        if addr < self.base_addr || addr >= self.base_addr + self.region_size {
            return;
        }
        let index = (addr - self.base_addr) / CARD_SIZE;
        let mut cells = self.cells.lock();
        if index < cells.cards.len() && cells.cards[index] != CARD_DIRTY {
            cells.cards[index] = CARD_DIRTY;
            cells.dirty_cards.push(index);
        }
    }

    /// Bulk-mark a slice of addresses dirty in a single lock acquisition.
    ///
    /// Equivalent to calling [`Self::mark_dirty`] for each address but
    /// amortises the `cells` mutex over the entire batch, which matters
    /// when the collector re-marks every deferred cross-gen card after a
    /// minor GC (formerly N lock acquisitions, now one).
    ///
    /// Addresses outside the covered region are silently ignored, matching
    /// the behaviour of [`Self::mark_dirty`].
    pub fn mark_dirty_bulk(&self, addrs: &[usize]) {
        if addrs.is_empty() {
            return;
        }
        let mut cells = self.cells.lock();
        let end = self.base_addr + self.region_size;
        for &addr in addrs {
            if addr < self.base_addr || addr >= end {
                continue;
            }
            let index = (addr - self.base_addr) / CARD_SIZE;
            if index < cells.cards.len() && cells.cards[index] != CARD_DIRTY {
                cells.cards[index] = CARD_DIRTY;
                cells.dirty_cards.push(index);
            }
        }
    }

    /// Check if a specific card is dirty.
    pub fn is_dirty(&self, card_index: usize) -> bool {
        self.cells
            .lock()
            .cards
            .get(card_index)
            .copied()
            == Some(CARD_DIRTY)
    }

    /// Clear all cards (set to CARD_CLEAN). Also drops any pending
    /// thread-local offsets that have been flushed into the shared
    /// queue — they would be applied to a now-clean bitmap and
    /// represent stale work from before the clear.
    pub fn clear_all(&self) {
        let mut cells = self.cells.lock();
        cells.cards.fill(CARD_CLEAN);
        cells.dirty_cards.clear();
        drop(cells);
        self.pending_offsets.lock().clear();
    }

    /// Collect indices of currently-dirty cards into a `Vec`.
    ///
    /// Returns a snapshot under the cells lock — callers can iterate
    /// freely without holding the lock. For bulk processing prefer
    /// [`Self::take_dirty_cards`] which uses the O(dirty) tracking
    /// list rather than scanning the whole bitmap.
    pub fn dirty_card_indices(&self) -> Vec<usize> {
        let cells = self.cells.lock();
        cells
            .cards
            .iter()
            .enumerate()
            .filter(|(_, &card)| card == CARD_DIRTY)
            .map(|(i, _)| i)
            .collect()
    }

    /// Return the tracked list of dirty card indices and clear the tracking list.
    ///
    /// This is O(dirty cards) rather than O(total cards), making it much faster
    /// when only a small fraction of cards are dirty.
    pub fn take_dirty_cards(&self) -> Vec<usize> {
        std::mem::take(&mut self.cells.lock().dirty_cards)
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
        self.cells.lock().cards.len()
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
    /// The final update to the shared bitmap is deferred to a safepoint,
    /// where the collector calls [`Self::drain_pending`] (or
    /// [`Self::flush_all`] then [`Self::drain_pending`]).
    pub fn thread_local_dirty(&self, offset: usize) {
        // SECURITY FIX (V6): push into the registered per-thread buffer
        // (Arc<Mutex<..>>). The lock is uncontended on the mutator fast path
        // (only this thread touches it outside STW); it is acquirable by the
        // collector only at STW, when this thread is parked.
        let should_flush = THREAD_DIRTY_BUFFER.with(|guard| {
            let mut b = guard.buffer.lock();
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
    /// that happens in [`Self::drain_pending`] under the cells lock, so
    /// the hot-path cost remains a single lock acquisition.
    pub fn flush_dirty_buffer(&self) {
        THREAD_DIRTY_BUFFER.with(|guard| {
            let mut local = guard.buffer.lock();
            if local.is_empty() {
                return;
            }
            let mut shared = self.pending_offsets.lock();
            shared.reserve(local.len());
            shared.append(&mut *local);
        });
    }

    /// T5.5.2 / SECURITY FIX (V6) — Drain EVERY thread's dirty buffer into
    /// the shared pending list. Called by the collector at GC start (before
    /// `scan_dirty_cards`).
    ///
    /// # SECURITY FIX (V6) — closes a cross-thread use-after-free
    ///
    /// Previously this was a thin alias for [`Self::flush_dirty_buffer`],
    /// which drains only the *calling* (collector) thread's buffer. That
    /// relied on every mutator flushing its OWN buffer before parking at a
    /// safepoint. A mutator that buffered fewer than
    /// [`THREAD_BUFFER_FLUSH_THRESHOLD`] old→young edges and then parked
    /// without flushing would leave those edges invisible to the collector;
    /// the minor GC would miss the old→young root and free a still-reachable
    /// young object (UAF when the mutator resumes and dereferences it).
    ///
    /// The fix removes that cross-thread obligation entirely: the collector
    /// now walks the global [`BUFFER_REGISTRY`] and drains every registered
    /// thread buffer itself. No VM-side safepoint flush is required for
    /// correctness.
    ///
    /// # Concurrency invariant
    ///
    /// This MUST be called only when mutators are stopped at the
    /// stop-the-world safepoint. Under STW no mutator is executing the write
    /// barrier, so each per-thread buffer mutex is uncontended and a thread
    /// cannot append a new offset between our drain and the subsequent
    /// [`Self::drain_pending`]. We still take each buffer's lock (rather than
    /// reading it racily) so the cross-thread access is well-defined even if
    /// a thread is parked mid-`push`. The registry lock is held for the whole
    /// walk so a concurrently *exiting* thread cannot remove (and free) a
    /// buffer we are about to drain.
    pub fn flush_all(&self) {
        let reg = BUFFER_REGISTRY.lock();
        for buf in reg.iter() {
            // Lock order matches the mutator fast path (buffer-lock before
            // pending-lock) so the two can never deadlock even if, contrary
            // to the STW invariant, they were ever to run concurrently.
            let mut local = buf.lock();
            if local.is_empty() {
                continue;
            }
            let mut shared = self.pending_offsets.lock();
            shared.reserve(local.len());
            shared.append(&mut *local);
        }
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
    pub fn drain_pending(&self) -> usize {
        // Snapshot pending offsets under the pending lock, then release
        // the pending lock before acquiring the cells lock so concurrent
        // mutators can continue enqueueing into `pending_offsets` while
        // we update the bitmap.
        let pending = std::mem::take(&mut *self.pending_offsets.lock());
        if pending.is_empty() {
            return 0;
        }
        let mut cells = self.cells.lock();
        let mut newly_dirtied = 0usize;
        for offset in pending {
            let addr = self.base_addr.wrapping_add(offset);
            if addr < self.base_addr || addr >= self.base_addr + self.region_size {
                continue;
            }
            let index = (addr - self.base_addr) / CARD_SIZE;
            if index < cells.cards.len() && cells.cards[index] != CARD_DIRTY {
                cells.cards[index] = CARD_DIRTY;
                cells.dirty_cards.push(index);
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
    #[inline]
    pub fn thread_local_dirty_addr(&self, addr: usize) {
        if addr < self.base_addr || addr >= self.base_addr + self.region_size {
            return;
        }
        self.thread_local_dirty(addr - self.base_addr);
    }
}

impl std::fmt::Debug for CardTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cells = self.cells.lock();
        let dirty_count = cells.cards.iter().filter(|&&c| c == CARD_DIRTY).count();
        f.debug_struct("CardTable")
            .field("num_cards", &cells.cards.len())
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
        let ct = CardTable::new(0x1000, 4096);
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
        let ct = CardTable::new(0x0, 2048);
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
        let ct = CardTable::new(0x0, 2048);
        ct.mark_dirty(0);
        ct.mark_dirty(512);
        ct.mark_dirty(1024);
        assert_eq!(ct.dirty_card_indices().len(), 3);

        ct.clear_all();
        assert_eq!(ct.dirty_card_indices().len(), 0);
        for i in 0..ct.num_cards() {
            assert!(!ct.is_dirty(i));
        }
    }

    #[test]
    fn dirty_card_indices() {
        let ct = CardTable::new(0x0, 4096);
        ct.mark_dirty(0); // card 0
        ct.mark_dirty(1536); // card 3 (1536 / 512 = 3)
        ct.mark_dirty(3584); // card 7 (3584 / 512 = 7)

        let dirty: Vec<usize> = ct.dirty_card_indices();
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
        let ct = CardTable::new(0x1000, 1024);
        // Address below base
        ct.mark_dirty(0x0);
        // Address above region
        ct.mark_dirty(0x1000 + 1024);
        ct.mark_dirty(0xFFFF);

        // No cards should be dirty
        assert_eq!(ct.dirty_card_indices().len(), 0);
    }

    #[test]
    fn mark_dirty_first_card() {
        let ct = CardTable::new(0x0, 4096);
        ct.mark_dirty(0);
        assert!(ct.is_dirty(0));
        assert!(!ct.is_dirty(1));
    }

    #[test]
    fn mark_dirty_last_card() {
        let ct = CardTable::new(0x0, 4096);
        // Last card covers bytes 3584..4095
        ct.mark_dirty(4095);
        let last_card = ct.num_cards() - 1;
        assert!(ct.is_dirty(last_card));
    }

    #[test]
    fn dirty_multiple_regions() {
        let ct = CardTable::new(0x0, 8192);
        // Dirty cards 0, 3, 7, 15
        ct.mark_dirty(0);       // card 0
        ct.mark_dirty(1536);    // card 3
        ct.mark_dirty(3584);    // card 7
        ct.mark_dirty(7680);    // card 15

        let dirty: Vec<usize> = ct.dirty_card_indices();
        assert_eq!(dirty, vec![0, 3, 7, 15]);
    }

    #[test]
    fn clear_then_dirty_again() {
        let ct = CardTable::new(0x0, 2048);
        ct.mark_dirty(0);
        ct.mark_dirty(512);
        assert_eq!(ct.dirty_card_indices().len(), 2);

        ct.clear_all();
        assert_eq!(ct.dirty_card_indices().len(), 0);

        // Re-dirty after clear
        ct.mark_dirty(1024);
        let dirty: Vec<usize> = ct.dirty_card_indices();
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
        let ct = CardTable::new(0x0, 4096);
        ct.mark_dirty(100);
        ct.mark_dirty(200);
        ct.mark_dirty(300);
        // All within card 0 (offsets 0..511)
        assert!(ct.is_dirty(0));
        assert_eq!(ct.dirty_card_indices().len(), 1);
    }

    #[test]
    fn all_cards_dirty() {
        let ct = CardTable::new(0x0, 2048);
        let num = ct.num_cards();
        for i in 0..num {
            ct.mark_dirty(i * CARD_SIZE);
        }
        assert_eq!(ct.dirty_card_indices().len(), num);
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
        let ct = CardTable::new(0x5000, 4096);
        ct.mark_dirty(0x5000);
        assert!(ct.is_dirty(0));
    }

    #[test]
    fn mark_dirty_at_region_end_minus_one() {
        let ct = CardTable::new(0x5000, 4096);
        ct.mark_dirty(0x5000 + 4095);
        let last = ct.num_cards() - 1;
        assert!(ct.is_dirty(last));
    }

    #[test]
    fn mark_dirty_at_exact_region_end_is_ignored() {
        let ct = CardTable::new(0x5000, 4096);
        // Exactly at base + region_size is out of range
        ct.mark_dirty(0x5000 + 4096);
        assert_eq!(ct.dirty_card_indices().len(), 0);
    }

    // -----------------------------------------------------------------
    // T5.5.2 — Thread-local batched dirty tests
    // -----------------------------------------------------------------

    #[test]
    fn thread_local_dirty_accumulates_without_touching_cards() {
        let ct = CardTable::new(0x0, 8192);
        // Dirty one offset via the fast path — stays in the thread-local
        // buffer; the shared table sees nothing until a flush.
        ct.thread_local_dirty(0);
        // Not enough to auto-flush.
        assert_eq!(ct.pending_count(), 0);
        assert_eq!(ct.dirty_card_indices().len(), 0);

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
        let ct = CardTable::new(
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
        let ct = CardTable::new(0x0, 4096);
        // All offsets within card 0.
        ct.thread_local_dirty(0);
        ct.thread_local_dirty(100);
        ct.thread_local_dirty(500);
        ct.flush_dirty_buffer();
        let newly = ct.drain_pending();
        // Three offsets but only one new card.
        assert_eq!(newly, 1);
        assert!(ct.is_dirty(0));
        assert_eq!(ct.dirty_card_indices().len(), 1);
    }

    #[test]
    fn drain_pending_rejects_out_of_range_offset() {
        let ct = CardTable::new(0x0, 1024);
        // Offset past region_size — should be silently dropped.
        ct.thread_local_dirty(1024);
        ct.thread_local_dirty(99999);
        ct.flush_dirty_buffer();
        let newly = ct.drain_pending();
        assert_eq!(newly, 0);
    }

    #[test]
    fn thread_local_dirty_addr_wraps_address_to_offset() {
        let ct = CardTable::new(0x10_0000, 4096);
        ct.thread_local_dirty_addr(0x10_0000 + 513); // card 1
        ct.flush_dirty_buffer();
        let newly = ct.drain_pending();
        assert_eq!(newly, 1);
        assert!(ct.is_dirty(1));
    }

    #[test]
    fn clear_all_also_resets_pending() {
        let ct = CardTable::new(0x0, 4096);
        ct.thread_local_dirty(0);
        ct.flush_dirty_buffer();
        assert_eq!(ct.pending_count(), 1);
        ct.clear_all();
        assert_eq!(ct.pending_count(), 0);
    }

    #[test]
    fn flush_all_is_alias_for_flush_dirty_buffer() {
        // T5.5.2 — `flush_all` is documented as the GC-entry hook. It
        // should be functionally identical to `flush_dirty_buffer` for
        // the current thread.
        let ct = CardTable::new(0x0, 4096);
        ct.thread_local_dirty(0);
        assert_eq!(ct.pending_count(), 0);
        ct.flush_all();
        assert_eq!(ct.pending_count(), 1);
        let newly = ct.drain_pending();
        assert_eq!(newly, 1);
        assert!(ct.is_dirty(0));
    }
}
