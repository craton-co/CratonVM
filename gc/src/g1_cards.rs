// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! F-05 — G1's card table: the per-address half of a remembered set whose
//! other half is per-REGION.
//!
//! # The finding
//!
//! G1's [`RememberedSet`](crate::region::RememberedSet) records SOURCE REGION
//! INDICES. To act on one entry, Phase 2
//! ([`G1Collector::scan_source_region_for_cset_refs`](crate::g1)) linearly
//! walks the whole source region `[0, cursor)` — validating every object
//! header, dispatching on layout, and visiting every reference slot — to find
//! the handful of slots that point into the collection set. The cost is
//! proportional to BYTES IN THE SOURCE REGION, not to the number of edges, so
//! one remembered edge into a 1 MiB Old region costs a megabyte walk. HotSpot's
//! G1 pays per dirty 512-byte card instead.
//!
//! This table is that card granularity. It does **not** replace the
//! region-index set: that set is what tells a pause WHICH regions to look at,
//! and this one tells it WHERE INSIDE one. Two questions, two structures.
//!
//! # Why not [`crate::card_table::CardTable`]
//!
//! The generational backend already has a card table and it does not fit here,
//! for two independent reasons:
//!
//! * its dirty path is a per-thread `Vec` buffer behind a `Mutex`, drained into
//!   the authoritative `Mutex<CardCells>` at a safepoint (see its module docs:
//!   `THREAD_BUFFER_FLUSH_THRESHOLD`, `BUFFER_REGISTRY`, `flush_all`). That
//!   design exists so the generational collector can drain a *queue* of dirty
//!   offsets; G1's post-write barrier is already on the critical path of every
//!   reference store and cannot afford a buffered push plus an eventual
//!   mutex-protected fold. It also cannot be expressed as inline machine code,
//!   which is precisely what F-08 needs;
//! * it is indexed relative to a `base_addr` that a `GenerationalHeap` owns,
//!   and carries table-id scoping to keep several live instances from stealing
//!   each other's buffered offsets. G1 has exactly one arena for the collector's
//!   whole life, so none of that machinery buys anything here.
//!
//! What is shared is the CARD SIZE (512 bytes) and the vocabulary, so that a
//! reader who knows one knows the other.
//!
//! # A byte per card, not a bit
//!
//! One bit per card would be 8x smaller (256 bytes of metadata per 1 MiB
//! region rather than 2 KiB). It is the wrong trade here, and the reason is
//! correctness rather than speed: setting one bit of a shared byte is a
//! read-modify-write, so two mutators dirtying two different cards in the same
//! 4 KiB of heap can lose one of the two updates unless the RMW is atomic. A
//! lost dirty card is a live cross-region edge that Phase 2 never scans, whose
//! referent is therefore not evacuated, and whose region Phase 5 then frees —
//! a use-after-free. Making the RMW atomic (`lock or byte`) costs roughly
//! twenty cycles on the hottest path in the write barrier.
//!
//! A byte per card is a plain unsynchronised store of a constant. That is what
//! HotSpot does, for the same reason, and it is what makes F-08's inline
//! barrier a single `mov byte [table + idx], 1` instead of a locked
//! instruction. The cost is 1/512 of the heap: 512 KiB for the 256 MiB default,
//! 64 MiB for a 32 GiB heap.
//!
//! # Lifetime: dirty until the region is RESET, and no sooner
//!
//! HotSpot cleans a card once it has been scanned, and re-dirties it during
//! evacuation for any slot whose referent moved. This table deliberately does
//! not: a card goes clean only in [`G1CardTable::clear_range`], which
//! `G1Region::reset` calls when the region is recycled and zero-filled.
//!
//! The reason is an invariant Phase 2 leans on. `scan_source_region_for_cset_refs`
//! skips a source region outright when none of its cards is dirty, which is
//! sound exactly because "the remembered set names region S" implies "S has a
//! dirty card" — the card and the rset entry are written by the same call, and
//! the rset entry is itself additive (its only pruning is `cleanup`'s
//! recycled-source pass). Cleaning a card at the end of a pause while leaving
//! the rset entry in place breaks that implication in the dangerous direction:
//! a slot rewritten by Phase 2 to a survivor's new address still points into a
//! region that may be in the NEXT pause's collection set, and its card would be
//! clean. HotSpot avoids this by re-dirtying during evacuation; matching the
//! rset's own additive lifetime is the cheaper way to be sure.
//!
//! What is lost is that a long-lived Old region accumulates dirty cards and
//! eventually screens nothing. That is the residual, and it is stated in the
//! commit message. What would falsify the "never clean" choice is a measurement
//! showing an old generation whose cards saturate; the fix would then be a
//! HotSpot-style re-dirty during Phase 2's own slot rewrite, not a bare clean.

use std::sync::atomic::{AtomicU8, Ordering};

/// log2 of the bytes one card covers. 512 bytes, matching
/// [`crate::card_table::CARD_SIZE`].
pub const G1_CARD_SHIFT: u32 = 9;

/// Bytes of heap one card covers.
pub const G1_CARD_BYTES: usize = 1 << G1_CARD_SHIFT;

/// No cross-region reference has been stored into this card since the region
/// was last recycled.
pub const G1_CARD_CLEAN: u8 = 0;

/// A cross-region reference store landed in this card.
pub const G1_CARD_DIRTY: u8 = 1;

/// One byte per [`G1_CARD_BYTES`] of the G1 arena.
///
/// Indexed by `(addr - base) >> G1_CARD_SHIFT`. The arena is a single
/// allocation whose regions are `base + i * region_size` slices of it
/// (`G1Collector::new`), and `region_size` is a power of two of at least 1 MiB,
/// so every region boundary is also a card boundary *relative to `base`* — no
/// card is ever shared between two regions, whatever `base`'s own alignment is.
/// That is what makes [`Self::clear_range`] at region reset exact rather than
/// approximate.
#[derive(Debug)]
pub struct G1CardTable {
    /// Arena base address; the origin of the card index.
    base: usize,
    /// Arena length in bytes.
    len: usize,
    /// `cards[i]` covers `[base + i*G1_CARD_BYTES, base + (i+1)*G1_CARD_BYTES)`.
    ///
    /// `AtomicU8` with `Relaxed` accesses throughout: this is a plain
    /// `mov byte` on every target this runs on, but going through the atomic
    /// type is what makes concurrent mutator dirtying defined behaviour in Rust
    /// rather than a data race. Nothing here ever does a read-modify-write, so
    /// no ordering is needed between two dirtiers — they store the same value.
    cards: Box<[AtomicU8]>,
}

impl G1CardTable {
    /// A table covering `[base, base + len)`, all cards clean.
    pub fn new(base: usize, len: usize) -> Self {
        let count = len.div_ceil(G1_CARD_BYTES);
        // `vec![0u8; n]` reaches `calloc`, so a 64 MiB table for a 32 GiB heap
        // costs a zero page mapping rather than 64 MiB of stores. Building it
        // as `Vec<AtomicU8>` through `collect` would not.
        let zeros: Box<[u8]> = vec![0u8; count].into_boxed_slice();
        // SAFETY: `AtomicU8` is `#[repr(transparent)]` over `UnsafeCell<u8>`,
        // which has the same size and alignment as `u8`. The allocation is
        // owned here and never aliased as `[u8]` afterwards.
        let cards: Box<[AtomicU8]> =
            unsafe { Box::from_raw(Box::into_raw(zeros) as *mut [AtomicU8]) };
        Self { base, len, cards }
    }

    /// The card index owning `addr`, or `None` when `addr` is outside the
    /// arena this table describes.
    #[inline]
    fn index_of(&self, addr: usize) -> Option<usize> {
        if addr < self.base || addr >= self.base + self.len {
            return None;
        }
        Some((addr - self.base) >> G1_CARD_SHIFT)
    }

    /// Mark the card owning `addr` dirty. A no-op for an address outside the
    /// arena (the caller's own region lookup has already returned `None` for
    /// such an address, so there is nothing to remember).
    #[inline]
    pub fn dirty_addr(&self, addr: usize) {
        if let Some(i) = self.index_of(addr) {
            // Relaxed: an unconditional store of a constant. Ordering against
            // the rset insert that accompanies it is not needed because both
            // are read only at a stop-the-world safepoint, after every mutator
            // has passed through a full barrier to park.
            self.cards[i].store(G1_CARD_DIRTY, Ordering::Relaxed);
        }
    }

    /// Is the card owning `addr` dirty? `false` for an out-of-arena address.
    #[inline]
    pub fn is_dirty_addr(&self, addr: usize) -> bool {
        match self.index_of(addr) {
            Some(i) => self.cards[i].load(Ordering::Relaxed) != G1_CARD_CLEAN,
            None => false,
        }
    }

    /// The half-open card index range covering `[start, start + span)`,
    /// clamped to the arena. Empty when the range misses the arena entirely.
    #[inline]
    fn card_range(&self, start: usize, span: usize) -> std::ops::Range<usize> {
        let arena_end = self.base + self.len;
        let lo = start.max(self.base);
        let hi = start.saturating_add(span).min(arena_end);
        if lo >= hi {
            return 0..0;
        }
        let first = (lo - self.base) >> G1_CARD_SHIFT;
        let last = (hi - 1 - self.base) >> G1_CARD_SHIFT;
        first..last + 1
    }

    /// Does any card overlapping `[start, start + span)` carry a dirty mark?
    ///
    /// This is both the whole-region screen (`span` = the region's filled
    /// bytes) and the per-object screen (`span` = one object's size). The
    /// per-object case is the hot one and is normally one or two byte loads,
    /// because objects are small relative to a card.
    #[inline]
    pub fn any_dirty_in(&self, start: usize, span: usize) -> bool {
        self.card_range(start, span)
            .any(|i| self.cards[i].load(Ordering::Relaxed) != G1_CARD_CLEAN)
    }

    /// How many cards overlapping `[start, start + span)` are dirty. Diagnostic
    /// only — the engagement counter's numerator.
    pub fn dirty_cards_in(&self, start: usize, span: usize) -> usize {
        self.card_range(start, span)
            .filter(|&i| self.cards[i].load(Ordering::Relaxed) != G1_CARD_CLEAN)
            .count()
    }

    /// How many cards overlap `[start, start + span)` at all — the engagement
    /// counter's denominator.
    pub fn cards_in(&self, start: usize, span: usize) -> usize {
        self.card_range(start, span).len()
    }

    /// Clean every card overlapping `[start, start + span)`.
    ///
    /// The ONLY cleaner, and its one production caller is `G1Region::reset`.
    /// See the module docs for why a scanned card is not cleaned: the whole-
    /// region screen in Phase 2 is sound only while "the rset names S" implies
    /// "S has a dirty card", and the rset entry outlives the pause that read it.
    ///
    /// Because a region boundary is always a card boundary relative to `base`
    /// (see the type docs), clearing a region's range never clears a byte that
    /// belongs to its neighbour.
    pub fn clear_range(&self, start: usize, span: usize) {
        for i in self.card_range(start, span) {
            self.cards[i].store(G1_CARD_CLEAN, Ordering::Relaxed);
        }
    }

    /// Address of card 0, for the F-08 inline barrier's baked immediate.
    ///
    /// Stable for the table's life: `cards` is a `Box<[AtomicU8]>` allocated
    /// once in [`Self::new`] and never resized.
    #[inline]
    pub fn cards_base_addr(&self) -> usize {
        self.cards.as_ptr() as usize
    }

    /// Arena base — the origin the card index is relative to.
    #[inline]
    pub fn arena_base(&self) -> usize {
        self.base
    }

    /// Arena length in bytes.
    #[inline]
    pub fn arena_len(&self) -> usize {
        self.len
    }

    /// Total number of cards.
    #[inline]
    pub fn card_count(&self) -> usize {
        self.cards.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic arena: the table never dereferences the addresses it is
    /// given, so a plausible base is enough.
    const BASE: usize = 0x1000_0000;

    #[test]
    fn a_fresh_table_is_entirely_clean() {
        let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
        assert_eq!(t.card_count(), 4);
        assert!(!t.any_dirty_in(BASE, 4 * G1_CARD_BYTES));
        assert_eq!(t.dirty_cards_in(BASE, 4 * G1_CARD_BYTES), 0);
    }

    #[test]
    fn dirtying_an_address_dirties_exactly_its_own_card() {
        let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
        t.dirty_addr(BASE + G1_CARD_BYTES + 17);
        assert!(!t.is_dirty_addr(BASE));
        assert!(t.is_dirty_addr(BASE + G1_CARD_BYTES));
        assert!(t.is_dirty_addr(BASE + 2 * G1_CARD_BYTES - 1));
        assert!(!t.is_dirty_addr(BASE + 2 * G1_CARD_BYTES));
        assert_eq!(t.dirty_cards_in(BASE, 4 * G1_CARD_BYTES), 1);
    }

    /// The per-object screen: an object straddling a card boundary must be
    /// scanned when EITHER card is dirty, because a reference slot anywhere in
    /// it may be the one the barrier recorded.
    #[test]
    fn a_straddling_span_is_dirty_if_any_card_it_touches_is() {
        let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
        t.dirty_addr(BASE + 2 * G1_CARD_BYTES);
        // A span ending one byte inside card 2.
        assert!(t.any_dirty_in(BASE + 2 * G1_CARD_BYTES - 8, 9));
        // The same span one byte shorter stops before card 2.
        assert!(!t.any_dirty_in(BASE + 2 * G1_CARD_BYTES - 8, 8));
    }

    #[test]
    fn clearing_a_range_leaves_its_neighbours_alone() {
        let t = G1CardTable::new(BASE, 8 * G1_CARD_BYTES);
        for c in 0..8 {
            t.dirty_addr(BASE + c * G1_CARD_BYTES);
        }
        t.clear_range(BASE + 2 * G1_CARD_BYTES, 3 * G1_CARD_BYTES);
        assert!(t.is_dirty_addr(BASE + G1_CARD_BYTES));
        assert!(!t.is_dirty_addr(BASE + 2 * G1_CARD_BYTES));
        assert!(!t.is_dirty_addr(BASE + 3 * G1_CARD_BYTES));
        assert!(!t.is_dirty_addr(BASE + 4 * G1_CARD_BYTES));
        assert!(t.is_dirty_addr(BASE + 5 * G1_CARD_BYTES));
        assert_eq!(t.dirty_cards_in(BASE, 8 * G1_CARD_BYTES), 5);
    }

    /// Out-of-arena addresses are silently ignored rather than panicking or
    /// aliasing card 0 — the write barrier hands this table whatever the
    /// mutator stored, including addresses in metaspace or off-heap.
    #[test]
    fn addresses_outside_the_arena_touch_nothing() {
        let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
        t.dirty_addr(BASE - 1);
        t.dirty_addr(BASE + 4 * G1_CARD_BYTES);
        t.dirty_addr(0);
        assert!(!t.any_dirty_in(BASE, 4 * G1_CARD_BYTES));
        assert!(!t.is_dirty_addr(BASE - 1));
    }

    /// A span that starts before the arena and ends inside it is clamped, not
    /// wrapped: `card_range` must not compute a huge index from an underflow.
    #[test]
    fn a_span_straddling_the_arena_base_is_clamped() {
        let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
        t.dirty_addr(BASE);
        assert!(t.any_dirty_in(BASE - 4096, 4096 + 8));
        assert_eq!(t.cards_in(BASE - 4096, 4096), 0);
    }
}
