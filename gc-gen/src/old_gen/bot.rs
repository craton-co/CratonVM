// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Old-generation BLOCK-OFFSET TABLE and the dirty-card object walk built on
//! it (gen r4w3/cards3, 2026-09-23).
//!
//! # The problem
//!
//! A young collection turns its dirty cards into byte ranges and asks the old
//! generation for the objects in them. The old generation had no index from a
//! card to an object start, so [`OldGen::walk_objects_in_card_ranges`] decoded
//! every old-gen header from offset 0 up to the last dirty range — a header
//! decode, a size computation and (for compact objects) a layout lookup per
//! OBJECT below the highest dirty card, on every young pause
//! (`docs/feature-designs/generative-gc-architecture.md`).
//!
//! # The table
//!
//! One `u32` per card ([`BOT_CARD`] = the card table's `CARD_SIZE`), holding
//! an ANCHOR in 8-byte units: an offset at or below the card's first byte from
//! which walking forward header by header reaches the object covering it.
//! [`OldGen::walk_card_ranges`] starts each dirty range at its card's anchor
//! instead of at offset 0, so a young pause decodes the objects near its dirty
//! cards and not the rest of the generation.
//!
//! # The invariant (INV), and the three hook points that keep it
//!
//! For every card `c < valid_cards` whose first byte `cs` is ALLOCATED (not in
//! a free block): `entries[c]` is an object start, `<= cs`, and no free block
//! intersects `[entries[c], cs]`. Cards whose first byte is free are never
//! consulted — the walk skips to that free block's end instead.
//!
//! * **alloc** (`OldGen::alloc_from_buckets_scan`, the one site that hands
//!   storage out): every card whose first byte lies in the new block
//!   `[off, off + size)` gets anchor `off`. The block is filled with objects
//!   end to end from `off` (one object, or a promotion buffer bump-carved from
//!   its start whose unused tail goes back through `release_unused_tail`), so
//!   `off` is an object start and `[off, cs]` is allocated. An allocation
//!   only turns free bytes into allocated ones, so it cannot break INV for any
//!   other card. O(cards in the block).
//! * **free** (`OldGen::free`, the in-place and concurrent sweeps): freeing
//!   `[x, x + s)` can break INV only for cards whose first byte is above `x`,
//!   so `valid_cards` drops to `x / BOT_CARD + 1`. O(1). The cards above are
//!   re-derived by the next walk that needs them (below).
//! * **compaction** (`OldGen::compact_walked`): everything moves, so
//!   `valid_cards = 0`.
//!
//! `release_unused_tail` needs no hook: it frees a SUFFIX of a block, and
//! every card above that block anchors at or above the block's end (its own
//! block, or an exact object start found by a walk that ran while the tail
//! was still free space) — so no interval `[anchor, cs]` can contain the tail.
//! `coalesce_free_blocks` only merges free blocks and changes no allocated
//! byte.
//!
//! # Lazily re-derived prefix
//!
//! Cards at or above `valid_cards` are never trusted. When a walk needs a card
//! above it, [`OldGen::bot_extend`] walks forward from the last trusted anchor
//! and writes the EXACT covering-object start of every card it passes — one
//! linear pass over the objects between the old and the new `valid_cards`.
//! So after an old-gen sweep the first young pause pays one walk up to its
//! highest dirty card (what every pause used to pay) and the following ones
//! pay only for the objects near their dirty cards.
//!
//! # Fallback
//!
//! The walk refuses the table — and the caller takes the linear walk — when
//! the free list is not disjoint (a double free, `OLD_FREE_LIST_OVERLAPS`).
//! `CRATONVM_GC_OLD_BOT=0` builds every `OldGen` without it.
//!
//! On a sane heap the anchored walk returns exactly the objects the linear
//! walk returns (`tests::the_anchored_walk_matches_the_linear_walk_on_randomized_heaps`).
//! On a heap whose walk desyncs (a corrupt header) the two can differ: the
//! linear walk abandons the rest of the region at the bad header, the anchored
//! walk may start past it and still report what lies beyond.
//!
//! # Walk gaps (gen r5w1/oldgen5, 2026-09-26)
//!
//! A header no walk can size hides every object behind it in its allocated
//! region. Before this, both walks skipped those bytes silently, so a young
//! object referenced only from a field in there was not a young root
//! (`gengc-r4w4-oldgen4-a-walk-gap-hides-old-to-young-roots-from-the-card-scan`).
//! Now:
//!
//! * [`OldGen::walk_card_ranges_with_gaps`] also returns GAP WINDOWS: for
//!   each unparseable stretch `[break, resume)` that a dirty range reaches,
//!   `[max(break, first dirty byte), resume)`. An object whose header card is
//!   dirty and whose header lies in the stretch has all its fields in that
//!   window (`resume` is an object start or the region's end), so a
//!   conservative scan of the window sees every slot the card could describe.
//!   The caller scans the words and must PIN, never rewrite, what they name.
//! * the anchored walk resumes at the first TRUSTED anchor above the break in
//!   the same region (an object start by INV), not at the region's end, so
//!   the stretch is at most one allocation block;
//! * re-deriving the table over the break no longer latches it off for the
//!   whole generation: every card whose first byte lies in the unparseable
//!   stretch is anchored AT the break (a walk started there meets the same
//!   header at once and reports the gap), and the re-derivation continues in
//!   the next region. INV is kept with "object start" read as "a walk
//!   boundary", which the break is.

use super::{FreeBlock, OldGen};
use cratonvm_gc_core::card_table::CARD_SIZE;
use cratonvm_gc_core::heap::{
    array_data_size, ArrayElementType, ObjectHeader, ObjectKind, ARRAY_DATA_OFFSET, HEADER_SIZE,
};

/// Bytes per block-offset card: the card table's granularity, so a dirty
/// card range maps onto whole entries.
pub const BOT_CARD: usize = CARD_SIZE;

/// One object reported by [`OldGen::walk_card_ranges`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CardRangeHit {
    /// The object's header.
    pub ptr: *mut u8,
    /// Its footprint in bytes, as the walk sized it.
    pub size: usize,
    /// `None`: the object's HEADER lies in a dirty range — scan all of it,
    /// which is the pre-2026-09-23 contract every header-card mark relies on.
    /// `Some((lo, hi))`: a REFERENCE ARRAY whose header card is clean but
    /// which overlaps a dirty range; `[lo, hi)` is the overlap as byte offsets
    /// from `ptr`. Only produced when the caller asked for overlaps.
    pub window: Option<(usize, usize)>,
}

/// Counters for one [`OldGen`]'s block-offset table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockOffsetStats {
    /// The table is built and not latched off.
    pub enabled: bool,
    /// Cards whose anchors are currently trusted.
    pub valid_cards: usize,
    /// Anchor slots allocated (tracks the old-gen high-water mark).
    pub entries: usize,
    /// Walks answered from the table.
    pub walks: u64,
    /// Walks the table refused (non-disjoint free list), answered linearly.
    pub fallbacks: u64,
    /// Dirty ranges started at an anchor (rather than a region start).
    pub anchored_ranges: u64,
    /// Objects decoded while re-deriving anchors after a free or compaction.
    pub rederived_objects: u64,
    /// Times a free or compaction lowered `valid_cards`.
    pub invalidations: u64,
    /// An anchor failed its plausibility check, or a walk met a header it
    /// could not size. Expected to stay 0.
    pub anomalies: u64,
    /// gen r5w1/oldgen5 — gap windows the walks reported (a dirty range
    /// reached bytes behind a header no walk can size), whichever walk
    /// answered. Expected to stay 0; non-zero means the young pause scanned
    /// old-gen words conservatively.
    pub gap_windows: u64,
    /// gen r5w1/oldgen5 — breaks the anchored walk resumed at a trusted anchor
    /// inside the same region instead of at the region's end.
    pub gap_anchor_resumes: u64,
    /// gen r5w1/oldgen5 — re-derivations that met a header they cannot size
    /// and anchored the stretch at the break (these used to latch the table
    /// off for the generation).
    pub gap_rederivations: u64,
    /// gen r5w5/old9 — object-base queries ([`OldGen::object_at`]), the ones
    /// it could not answer ([`ObjectAt::Unknown`]), the headers it decoded
    /// while striding from a card's anchor, and the cards it re-anchored
    /// exactly on the way (path compression; see `bot_compress`).
    #[cfg(test)]
    pub oracle_queries: u64,
    #[cfg(test)]
    pub oracle_unknown: u64,
    #[cfg(test)]
    pub oracle_strides: u64,
    #[cfg(test)]
    pub oracle_compressed: u64,
    // gcg g6/og: `rebuilds` (`rebuild_block_offsets_from_live`) and
    // `carved_spans_anchored` (`anchor_carved_span`) went with the O(live)
    // sweep's API.
}

/// gen r5w5/old9 (2026-09-27) — what [`OldGen::object_at`] found at an
/// absolute address: the block-offset table's answer to "which object, if
/// any, holds this byte". The stop-the-world old-gen mark used to answer that
/// question only by a binary search over a walk of EVERY allocated object
/// (`walk_objects_with_gaps`), which is what made each old-gen collection
/// O(allocated) even when almost nothing survived
/// (`gengc-r5w3-oldgen7-proposal-bot-object-base-oracle-for-an-o-live-sweep`).
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectAt {
    /// The address is an object's base; the object is `size` bytes (the
    /// footprint the walk would report).
    Base { size: usize },
    /// The address lies strictly inside the object at absolute `base`.
    Interior { base: usize, size: usize },
    /// Outside the generation's storage, or inside a free block: no object.
    Free,
    /// The table cannot answer: it is off (or latched off), the free list
    /// overlaps (a double free), or the walk from the card's anchor met a
    /// header it cannot size (a walk gap). A caller must not decide liveness
    /// from this answer; the live sweep abandons its attempt and the
    /// collection runs the walked path instead.
    Unknown,
}

/// The table itself. Owned by [`OldGen`] behind a `RefCell`; see the module
/// doc for the invariant each method keeps.
pub struct BlockOffsetTable {
    enabled: bool,
    /// `entries[c]` = anchor offset of card `c`, in 8-byte units.
    entries: Vec<u32>,
    /// Cards `[0, valid_cards)` satisfy INV.
    valid_cards: usize,
    /// `free_list_seq` the disjointness verdict below was computed at.
    free_seq_seen: Option<u64>,
    free_disjoint: bool,
    stats: BlockOffsetStats,
}

impl BlockOffsetTable {
    /// A table for a generation of `capacity` bytes. `requested` is
    /// `CRATONVM_GC_OLD_BOT`; an anchor is a `u32` of 8-byte units, so a
    /// generation above 32 GiB is built without one.
    pub fn new(capacity: usize, requested: bool) -> Self {
        Self {
            enabled: requested && Self::encodable(capacity),
            entries: Vec::new(),
            // A new generation is one free block: INV constrains only cards
            // whose first byte is allocated, so it holds for every card, and
            // from here on `note_alloc` anchors each card as it is covered.
            valid_cards: capacity.div_ceil(BOT_CARD),
            free_seq_seen: None,
            free_disjoint: false,
            stats: BlockOffsetStats::default(),
        }
    }

    fn encodable(capacity: usize) -> bool {
        (capacity as u64) / 8 <= u64::from(u32::MAX)
    }

    /// `[offset, offset + size)` was just handed out (hook in
    /// `alloc_from_buckets_scan`).
    #[inline]
    pub fn note_alloc(&mut self, offset: usize, size: usize) {
        if !self.enabled || size == 0 {
            return;
        }
        if offset % 8 != 0 {
            // An anchor is stored in 8-byte units, so an unaligned start
            // cannot be represented. Every production allocation is 8-aligned
            // (`OldGen::new` debug-asserts the base, and `align >= 8`); latch
            // off rather than store an anchor below the real start.
            self.latch_off("an old-gen allocation is not 8-byte aligned");
            return;
        }
        let first = offset.div_ceil(BOT_CARD);
        let last = (offset + size).div_ceil(BOT_CARD);
        if first >= last {
            return;
        }
        if self.entries.len() < last {
            self.entries.resize(last, 0);
        }
        let anchor = (offset / 8) as u32;
        for e in &mut self.entries[first..last] {
            *e = anchor;
        }
    }

    /// An object at `offset` was freed (hook in `OldGen::free`). Cards whose
    /// first byte is at or below `offset` keep INV; the rest are re-derived
    /// before they are trusted again.
    #[inline]
    pub fn note_free(&mut self, offset: usize) {
        if !self.enabled {
            return;
        }
        let keep = offset / BOT_CARD + 1;
        if keep < self.valid_cards {
            self.valid_cards = keep;
            self.stats.invalidations += 1;
        }
    }

    /// Every object may have moved (hook in `OldGen::compact_walked`).
    pub fn note_relayout(&mut self) {
        if !self.enabled {
            return;
        }
        if self.valid_cards > 0 {
            self.stats.invalidations += 1;
        }
        self.valid_cards = 0;
    }

    #[cold]
    fn latch_off(&mut self, why: &str) {
        if self.enabled {
            tracing::warn!(
                target: "cratonvm::gc",
                why,
                "old-gen block-offset table latched OFF for this generation; the \
                 dirty-card object walk falls back to the linear walk (gen r4w3/cards3)",
            );
        }
        self.enabled = false;
        self.stats.anomalies += 1;
        self.valid_cards = 0;
        self.entries = Vec::new();
    }

    fn anchor(&self, card: usize) -> Option<usize> {
        self.entries.get(card).map(|&e| (e as usize) * 8)
    }
}

/// Where an offset falls relative to the (disjoint, offset-sorted) free list.
enum Loc {
    /// Inside a free block that ends at `end`.
    Free { end: usize },
    /// In the allocated region `[start, end)` between two free blocks.
    Allocated { start: usize, end: usize },
}

fn locate(sorted: &[FreeBlock], p: usize, len: usize) -> Loc {
    let i = sorted.partition_point(|b| b.offset <= p);
    let mut start = 0;
    if i > 0 {
        let b = sorted[i - 1];
        let end = b.offset + b.size;
        if p < end {
            return Loc::Free { end };
        }
        start = end;
    }
    Loc::Allocated {
        start,
        end: sorted.get(i).map_or(len, |b| b.offset),
    }
}

/// The anchored walk needs "the free block covering `p`" to be a well-defined
/// question, i.e. no two blocks overlap. A double free breaks that; the
/// linear walk tolerates it (`cursor.max(..)`), so the caller falls back.
pub(super) fn free_list_is_disjoint(sorted: &[FreeBlock], len: usize) -> bool {
    let ends_ok = sorted
        .iter()
        .all(|b| b.offset.checked_add(b.size).is_some_and(|e| e <= len));
    ends_ok
        && sorted
            .windows(2)
            .all(|w| w[0].offset + w[0].size <= w[1].offset)
}

/// gen r5w1/oldgen5 — the part of the unparseable stretch `[g0, g1)` a
/// conservative scan must cover for the sorted, disjoint dirty `ranges`: from
/// the first dirty byte inside it to its end, or `None` when no range reaches
/// it.
///
/// Why to the END and not only the dirty bytes: the card barrier dirties the
/// card of the holder's HEADER, and a holder in the stretch can be larger than
/// its card; its fields then lie in clean cards up to its end, which is at or
/// before `g1` (`g1` is an object start or the region's end). An
/// element-precise mark on a reference array in the stretch dirties the
/// element's own card, which the window also covers.
pub(super) fn gap_window(
    ranges: &[(usize, usize)],
    g0: usize,
    g1: usize,
) -> Option<(usize, usize)> {
    if g0 >= g1 {
        return None;
    }
    let i = ranges.partition_point(|&(_, hi)| hi <= g0);
    let &(lo, _) = ranges.get(i)?;
    (lo < g1).then_some((lo.max(g0), g1))
}

/// Append `w` to an ascending window list, merging it into the last window
/// when the two touch.
pub(super) fn push_gap(gaps: &mut Vec<(usize, usize)>, w: (usize, usize)) {
    if let Some(last) = gaps.last_mut() {
        if w.0 <= last.1 {
            last.1 = last.1.max(w.1);
            return;
        }
    }
    gaps.push(w);
}

/// Report object `[o, o + size)` against the ranges from `*ri` on.
///
/// The object is WHOLE iff its header lies in a range — the first range that
/// ends above `o` is the only one that can contain it, because the ranges are
/// sorted and disjoint. Otherwise, when `overlaps` is set and the object is a
/// reference array, each later range it overlaps yields a WINDOW. `*ri` only
/// ever moves past ranges that end at or below `o`, so a later object still
/// sees every range it can start in.
#[allow(clippy::too_many_arguments)]
fn visit(
    base: usize,
    o: usize,
    size: usize,
    is_ref_array: bool,
    ranges: &[(usize, usize)],
    ri: &mut usize,
    overlaps: bool,
    out: &mut Vec<CardRangeHit>,
) {
    let end = o + size;
    while *ri < ranges.len() && ranges[*ri].1 <= o {
        *ri += 1;
    }
    let ptr = (base + o) as *mut u8;
    let mut j = *ri;
    while j < ranges.len() && ranges[j].0 < end {
        let (lo, hi) = ranges[j];
        if lo <= o {
            out.push(CardRangeHit {
                ptr,
                size,
                window: None,
            });
            return;
        }
        if !(overlaps && is_ref_array) {
            return;
        }
        out.push(CardRangeHit {
            ptr,
            size,
            window: Some((lo - o, hi.min(end) - o)),
        });
        j += 1;
    }
}

impl OldGen {
    /// Is the block-offset table built and in use for this generation?
    pub fn block_offset_table_enabled(&self) -> bool {
        self.bot.borrow().enabled
    }

    /// Turn the block-offset table on or off for this generation (A/B tests).
    /// Turning it on starts from an empty trusted prefix, so it is correct
    /// whatever happened while it was off.
    pub fn set_block_offset_table_enabled(&mut self, on: bool) {
        let cap = self.data.len();
        // Never allocated from: one free block, so every card is trusted (see
        // `BlockOffsetTable::new`). Otherwise nothing is, until re-derived —
        // `note_alloc` did not run while the table was off.
        let pristine = self.used_bytes == 0 && self.high_water == 0;
        let bot = self.bot.get_mut();
        bot.enabled = on && BlockOffsetTable::encodable(cap);
        bot.valid_cards = if pristine { cap.div_ceil(BOT_CARD) } else { 0 };
        bot.entries.clear();
        bot.free_seq_seen = None;
    }

    /// Counters for the block-offset table (`[rset-verify]`, tests).
    pub fn block_offset_stats(&self) -> BlockOffsetStats {
        let bot = self.bot.borrow();
        BlockOffsetStats {
            enabled: bot.enabled,
            valid_cards: bot.valid_cards,
            entries: bot.entries.len(),
            ..bot.stats
        }
    }

    /// The young pause's dirty-card object walk (gen r4w3/cards3).
    ///
    /// `dirty_ranges` is what `walk_objects_in_card_ranges` takes: `[start,
    /// end)` byte offsets, sorted, disjoint. Every object whose HEADER lies in
    /// a range is reported with `window: None` — exactly the set
    /// `walk_objects_in_card_ranges` returns, in the same order. With
    /// `overlaps`, each reference array that overlaps a range without starting
    /// in one is ALSO reported, once per range, with the overlap as its
    /// window; that is what element-precise card marks need
    /// (`CardTable::precise_ref_array_marks`).
    ///
    /// Answered from the block-offset table when it is enabled, which bounds
    /// the headers decoded by the objects near the dirty ranges instead of by
    /// every object below the last one. Otherwise, or if the table refuses
    /// (see the module doc), by a linear walk from offset 0.
    pub fn walk_card_ranges(
        &self,
        dirty_ranges: &[(usize, usize)],
        overlaps: bool,
    ) -> Vec<CardRangeHit> {
        self.walk_card_ranges_with_gaps(dirty_ranges, overlaps).0
    }

    /// [`Self::walk_card_ranges`], plus the GAP WINDOWS (gen r5w1/oldgen5):
    /// byte-offset ranges `[lo, hi)`, ascending and disjoint, of old-gen bytes
    /// the walk could not parse (behind a header it cannot size) that a dirty
    /// range reaches. See the module doc's "Walk gaps". Every object the walk
    /// COULD parse is in the hit list exactly as before; a gap window holds
    /// what it could not, and its words must be treated as conservative roots
    /// (pinned, never rewritten). Empty on a sane heap.
    pub fn walk_card_ranges_with_gaps(
        &self,
        dirty_ranges: &[(usize, usize)],
        overlaps: bool,
    ) -> (Vec<CardRangeHit>, Vec<(usize, usize)>) {
        let mut gaps: Vec<(usize, usize)> = Vec::new();
        if dirty_ranges.is_empty() {
            return (Vec::new(), gaps);
        }
        {
            let mut bot = self.bot.borrow_mut();
            if bot.enabled {
                let answered = self.with_sorted_free_blocks(|sorted| {
                    self.bot_walk(&mut bot, sorted, dirty_ranges, overlaps, &mut gaps)
                });
                if let Some(hits) = answered {
                    bot.stats.walks += 1;
                    bot.stats.gap_windows += gaps.len() as u64;
                    return (hits, gaps);
                }
                bot.stats.fallbacks += 1;
                // A refused walk may have stopped part way; the linear walk
                // below derives its own windows from scratch.
                gaps.clear();
            }
        }
        let hits = if !overlaps {
            self.walk_objects_in_card_ranges_with_gaps(dirty_ranges, &mut gaps)
                .into_iter()
                .map(|(ptr, size)| CardRangeHit {
                    ptr,
                    size,
                    window: None,
                })
                .collect()
        } else {
            self.with_sorted_free_blocks(|sorted| {
                self.linear_touch_walk(sorted, dirty_ranges, &mut gaps)
            })
        };
        if !gaps.is_empty() {
            self.bot.borrow_mut().stats.gap_windows += gaps.len() as u64;
        }
        (hits, gaps)
    }

    /// Size the object at `offset`, exactly as `scan_region_filtered` does,
    /// without panicking on an overflowing array length. `None` for a header
    /// the walk cannot trust (invalid tags, `HumongousFiller`, overflow, or
    /// smaller than a header). Also says whether it is a reference array.
    fn bot_object_extent(&self, offset: usize) -> Option<(usize, bool)> {
        object_extent_at(self.data.as_ptr() as usize, offset)
    }
}

/// [`OldGen::bot_object_extent`] against a bare storage base, so the parallel
/// walk ([`CardWalkView`]) can size objects without sharing the generation.
fn object_extent_at(base: usize, offset: usize) -> Option<(usize, bool)> {
    let ptr = (base + offset) as *mut u8;
    let kind = OldGen::validate_header_tags_or_desync(ptr, offset)?;
    if kind == ObjectKind::HumongousFiller {
        return None;
    }
    // SAFETY: `offset` is an object boundary inside an allocated region of
    // this generation's storage (every caller derives it from a free-block
    // end, a trusted anchor, or the previous object's end), and the
    // kind/element-type tag bytes were validated just above, so forming
    // the typed header is sound.
    let header = unsafe { &*(ptr as *const ObjectHeader) };
    let (raw, is_ref_array) = if kind == ObjectKind::Array {
        let et = header.element_type();
        let data = array_data_size(header.array_length() as usize, et).ok()?;
        (
            ARRAY_DATA_OFFSET.checked_add(data)?,
            et == ArrayElementType::Reference,
        )
    } else {
        (cratonvm_types::object_instance_size(header), false)
    };
    let total = raw.checked_add(7)? & !7;
    (total >= HEADER_SIZE).then_some((total, is_ref_array))
}

impl OldGen {
    /// Where a walk that must see every object CONTAINING or STARTING AFTER
    /// `p` may begin. A free block covering `p` answers its end; otherwise the
    /// card's trusted anchor, or — when the card is not trusted, or its first
    /// byte is in the free block below `p`'s region — the region's start,
    /// which is always an object start and always correct.
    fn bot_walk_start(&self, bot: &mut BlockOffsetTable, sorted: &[FreeBlock], p: usize) -> usize {
        match locate(sorted, p, self.data.len()) {
            Loc::Free { end } => end,
            Loc::Allocated { start, .. } => {
                let card = p / BOT_CARD;
                let cs = card * BOT_CARD;
                if card >= bot.valid_cards || cs < start {
                    return start;
                }
                match bot.anchor(card) {
                    Some(a) if a >= start && a <= cs => {
                        bot.stats.anchored_ranges += 1;
                        a
                    }
                    _ => {
                        // INV says this cannot happen. The region start is
                        // correct anyway; count it so a test can insist on 0.
                        bot.stats.anomalies += 1;
                        start
                    }
                }
            }
        }
    }

    /// Make cards `[0, need)` trusted, re-deriving the untrusted ones by one
    /// forward walk from the last trusted anchor. Writes the EXACT start of
    /// the object covering each card's first byte (or the byte itself for a
    /// card that starts in a free block, which is never consulted). `false` —
    /// and the table latched off — only if the walk runs past the generation.
    ///
    /// gen r5w1/oldgen5: a header it cannot size no longer latches the table
    /// off. Every card whose first byte lies between that header and the end
    /// of its allocated region is anchored AT the header: a walk started there
    /// meets it at once and reports the stretch as a gap
    /// ([`Self::walk_card_ranges_with_gaps`]), which is what a walk from the
    /// region's start would report after parsing the objects in front of it.
    /// The re-derivation then continues in the next region, so one corrupt
    /// header costs the table that one stretch, not the whole generation.
    fn bot_extend(&self, bot: &mut BlockOffsetTable, sorted: &[FreeBlock], need: usize) -> bool {
        let v = bot.valid_cards;
        if v >= need {
            return true;
        }
        let len = self.data.len();
        if bot.entries.len() < need {
            bot.entries.resize(need, 0);
        }
        let mut cursor = if v == 0 {
            0
        } else {
            self.bot_walk_start(bot, sorted, v * BOT_CARD - 1)
        };
        // The last object passed, as `[start, end)`; `None` after a free block.
        let mut last: Option<(usize, usize)> = None;
        let mut region_end = 0usize;
        let mut in_region = false;
        let mut decoded: u64 = 0;
        // The last unparseable stretch passed, as `[break, region_end)`.
        let mut gap: Option<(usize, usize)> = None;
        for card in v..need {
            let cs = card * BOT_CARD;
            while cursor <= cs {
                if cursor >= len {
                    bot.latch_off("block-offset re-derivation ran past the generation");
                    return false;
                }
                if !in_region || cursor >= region_end {
                    match locate(sorted, cursor, len) {
                        Loc::Free { end } => {
                            cursor = end;
                            last = None;
                            in_region = false;
                            continue;
                        }
                        Loc::Allocated { end, .. } => {
                            region_end = end;
                            in_region = true;
                        }
                    }
                }
                match self.bot_object_extent(cursor) {
                    Some((size, _)) if cursor + size <= region_end => {
                        last = Some((cursor, cursor + size));
                        cursor += size;
                        decoded += 1;
                    }
                    _ => {
                        // gen r5w1/oldgen5: anchor the stretch at the break
                        // (see the doc above) instead of latching off.
                        bot.stats.anomalies += 1;
                        bot.stats.gap_rederivations += 1;
                        gap = Some((cursor, region_end));
                        cursor = region_end;
                        last = None;
                        in_region = false;
                    }
                }
            }
            let anchor = match (gap, last) {
                (Some((g0, g1)), _) if g0 <= cs && cs < g1 => g0,
                (_, Some((s, e))) if s <= cs && cs < e => s,
                _ => cs,
            };
            bot.entries[card] = (anchor / 8) as u32;
        }
        bot.valid_cards = need;
        bot.stats.rederived_objects += decoded;
        true
    }

    /// gen r5w1/oldgen5 — where the anchored walk resumes after a header it
    /// cannot size at `o`, inside the allocated region ending at `region_end`:
    /// the first TRUSTED anchor above `o` in the region, else `region_end`.
    ///
    /// By INV a trusted anchor of a card whose first byte is allocated is a
    /// walk boundary at or below that byte with no free block in between; one
    /// above `o` and at or below a first byte inside `[o, region_end)` is
    /// therefore in this region, and the objects from it on tile the region
    /// exactly as allocation laid them down. Anchors at or below `o` (the
    /// cards of the allocation block the break sits in, and the cards
    /// [`Self::bot_extend`] anchored AT a break) name nothing past it.
    fn bot_resume_after_break(bot: &mut BlockOffsetTable, o: usize, region_end: usize) -> usize {
        let first = o / BOT_CARD + 1;
        let last = region_end.div_ceil(BOT_CARD).min(bot.valid_cards);
        for card in first..last {
            let cs = card * BOT_CARD;
            if cs >= region_end {
                break;
            }
            if let Some(a) = bot.anchor(card) {
                if a > o && a <= cs {
                    bot.stats.gap_anchor_resumes += 1;
                    return a;
                }
            }
        }
        region_end
    }

    /// The anchored walk. `None` asks the caller for the linear walk.
    ///
    /// gen r5w1/oldgen5: `gaps` receives the gap windows (see
    /// [`Self::walk_card_ranges_with_gaps`]).
    fn bot_walk(
        &self,
        bot: &mut BlockOffsetTable,
        sorted: &[FreeBlock],
        ranges: &[(usize, usize)],
        overlaps: bool,
        gaps: &mut Vec<(usize, usize)>,
    ) -> Option<Vec<CardRangeHit>> {
        let len = self.data.len();
        let seq = self.free_list_seq();
        if bot.free_seq_seen != Some(seq) {
            bot.free_disjoint = free_list_is_disjoint(sorted, len);
            bot.free_seq_seen = Some(seq);
        }
        if !bot.free_disjoint || len == 0 {
            return None;
        }
        // Every range is started from the card of its `lo`; the last one is
        // the highest card the walk will consult.
        let last_lo = ranges[ranges.len() - 1].0.min(len - 1);
        if !self.bot_extend(bot, sorted, last_lo / BOT_CARD + 1) {
            return None;
        }
        let base = self.data.as_ptr() as usize;
        let mut out = Vec::new();
        // Every object starting below `cursor` has been reported or ruled out.
        let mut cursor = 0usize;
        let mut ri = 0usize;
        let mut region_end = 0usize;
        let mut in_region = false;
        for &(lo, hi) in ranges {
            if hi <= cursor {
                // Wholly inside an object already visited (which reported its
                // window for this range, if it had one) or a skipped free block.
                continue;
            }
            if lo > cursor {
                let w = self.bot_walk_start(bot, sorted, lo);
                if w > cursor {
                    cursor = w;
                    in_region = false;
                }
            }
            while cursor < hi && cursor < len {
                if !in_region || cursor >= region_end {
                    match locate(sorted, cursor, len) {
                        Loc::Free { end } => {
                            cursor = end;
                            in_region = false;
                            continue;
                        }
                        Loc::Allocated { end, .. } => {
                            region_end = end;
                            in_region = true;
                        }
                    }
                }
                match self.bot_object_extent(cursor) {
                    Some((size, is_ref)) if cursor + size <= region_end => {
                        visit(
                            base, cursor, size, is_ref, ranges, &mut ri, overlaps, &mut out,
                        );
                        cursor += size;
                    }
                    _ => {
                        // gen r5w1/oldgen5: the objects behind a header this
                        // walk cannot size are invisible to it. Report the
                        // stretch a dirty range reaches as a gap window, and
                        // resume at the first trusted anchor past the break
                        // (the region's end when there is none — what this
                        // walk, like the linear one, always did).
                        bot.stats.anomalies += 1;
                        let resume = Self::bot_resume_after_break(bot, cursor, region_end);
                        if let Some(w) = gap_window(ranges, cursor, resume) {
                            push_gap(gaps, w);
                        }
                        cursor = resume;
                        if cursor >= region_end {
                            in_region = false;
                        }
                    }
                }
            }
        }
        Some(out)
    }

    /// The linear walk, reporting overlap windows too — the fallback for
    /// `overlaps == true` when the table is off or refuses. Same region
    /// derivation as `walk_objects_in_card_ranges` (including its `max` guard
    /// against a nested free block), same stop-at-the-last-range early exit.
    fn linear_touch_walk(
        &self,
        sorted: &[FreeBlock],
        ranges: &[(usize, usize)],
        gaps: &mut Vec<(usize, usize)>,
    ) -> Vec<CardRangeHit> {
        let base = self.data.as_ptr() as usize;
        let len = self.data.len();
        let last_hi = ranges[ranges.len() - 1].1;
        let mut out = Vec::new();
        let mut ri = 0usize;
        let mut cursor = 0usize;
        for block in sorted {
            if block.offset > cursor
                && !self.touch_region(
                    base,
                    cursor,
                    block.offset,
                    ranges,
                    &mut ri,
                    last_hi,
                    &mut out,
                    gaps,
                )
            {
                return out;
            }
            cursor = cursor.max(block.offset + block.size);
            if cursor >= last_hi {
                return out;
            }
        }
        if cursor < len {
            self.touch_region(base, cursor, len, ranges, &mut ri, last_hi, &mut out, gaps);
        }
        out
    }

    /// One allocated region of [`Self::linear_touch_walk`]. `false` once no
    /// later object can touch a range. gen r5w1/oldgen5: a header it cannot
    /// size ends the region as before, and the rest of the region goes to
    /// `gaps` as a window when a dirty range reaches it.
    #[allow(clippy::too_many_arguments)]
    fn touch_region(
        &self,
        base: usize,
        start: usize,
        end: usize,
        ranges: &[(usize, usize)],
        ri: &mut usize,
        last_hi: usize,
        out: &mut Vec<CardRangeHit>,
        gaps: &mut Vec<(usize, usize)>,
    ) -> bool {
        let mut o = start;
        while o < end {
            if o >= last_hi {
                return false;
            }
            let fits = self
                .bot_object_extent(o)
                .filter(|&(size, _)| o + size <= end);
            let Some((size, is_ref)) = fits else {
                if let Some(w) = gap_window(ranges, o, end) {
                    push_gap(gaps, w);
                }
                break;
            };
            visit(base, o, size, is_ref, ranges, ri, true, out);
            if *ri >= ranges.len() {
                return false;
            }
            o += size;
        }
        true
    }

    /// gcj j1yg-pause: run `f` over a read-only [`CardWalkView`] of the
    /// anchored walk for `dirty_ranges`, so the young pause can split the walk
    /// across its evacuation workers. Prepares exactly what [`Self::bot_walk`]
    /// prepares (the free-list verdict, the re-derived prefix up to the last
    /// range's card) on this thread first; the view itself mutates nothing.
    /// `None` -- `f` not run -- when the table is off or refuses, which is
    /// when [`Self::walk_card_ranges_with_gaps`] takes the linear walk.
    pub fn with_card_walk_view<R>(
        &self,
        dirty_ranges: &[(usize, usize)],
        f: impl FnOnce(&CardWalkView<'_>) -> R,
    ) -> Option<R> {
        let len = self.data.len();
        if dirty_ranges.is_empty() || len == 0 {
            return None;
        }
        self.with_sorted_free_blocks(|sorted| {
            {
                let mut bot = self.bot.borrow_mut();
                if !bot.enabled {
                    return None;
                }
                let seq = self.free_list_seq();
                if bot.free_seq_seen != Some(seq) {
                    bot.free_disjoint = free_list_is_disjoint(sorted, len);
                    bot.free_seq_seen = Some(seq);
                }
                if !bot.free_disjoint {
                    return None;
                }
                let last_lo = dirty_ranges[dirty_ranges.len() - 1].0.min(len - 1);
                if !self.bot_extend(&mut bot, sorted, last_lo / BOT_CARD + 1) {
                    return None;
                }
            }
            let (answer, anchored) = {
                let bot = self.bot.borrow();
                let view = CardWalkView {
                    base: self.data.as_ptr() as usize,
                    len,
                    sorted,
                    entries: &bot.entries,
                    valid_cards: bot.valid_cards,
                    anchored: std::sync::atomic::AtomicU64::new(0),
                };
                let answer = f(&view);
                (answer, view.anchored.into_inner())
            };
            let mut bot = self.bot.borrow_mut();
            bot.stats.walks += 1;
            bot.stats.anchored_ranges += anchored;
            Some(answer)
        })
    }
}

/// gcj j1yg-pause: the anchored dirty-card walk as read-only shared state
/// ([`OldGen::with_card_walk_view`]): the storage base, the sorted free list
/// and the trusted block-offset prefix. [`Self::walk_group`] walks one
/// contiguous group of the dirty ranges, so disjoint groups can run on
/// different threads; the groups' hits, concatenated in group order, are the
/// serial walk's hits in the serial walk's order.
pub struct CardWalkView<'a> {
    base: usize,
    len: usize,
    sorted: &'a [FreeBlock],
    entries: &'a [u32],
    valid_cards: usize,
    anchored: std::sync::atomic::AtomicU64,
}

/// A group walk met what the serial walk handles with its anomaly paths (a
/// header it cannot size, an implausible anchor). The caller discards every
/// group's output and takes the serial walk, which reports the gap windows.
#[derive(Debug)]
pub struct CardWalkBreak;

impl CardWalkView<'_> {
    /// [`OldGen::bot_walk_start`] without its anomaly fallback.
    fn walk_start(&self, p: usize) -> Result<usize, CardWalkBreak> {
        match locate(self.sorted, p, self.len) {
            Loc::Free { end } => Ok(end),
            Loc::Allocated { start, .. } => {
                let card = p / BOT_CARD;
                let cs = card * BOT_CARD;
                if card >= self.valid_cards || cs < start {
                    return Ok(start);
                }
                match self.entries.get(card).map(|&e| (e as usize) * 8) {
                    Some(a) if a >= start && a <= cs => {
                        self.anchored
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        Ok(a)
                    }
                    _ => Err(CardWalkBreak),
                }
            }
        }
    }

    /// Walk the group `ranges[g0..g1]` of the sorted, disjoint `ranges` the
    /// view was made for, and give `out` every hit the serial walk
    /// ([`OldGen::bot_walk`]) reports against a range of the group:
    ///
    /// * an object whose header lies in a range of the group: whole;
    /// * a reference array (with `overlaps`) overlapping a range of the group
    ///   whose header lies in no range at all: one window per such range --
    ///   the serial walk's `visit` reports the same windows, and the earlier
    ///   groups report the windows of their own ranges;
    /// * nothing for an object whose header lies in an EARLIER group's range:
    ///   the serial walk reports it whole there and no window for it.
    pub fn walk_group(
        &self,
        ranges: &[(usize, usize)],
        g0: usize,
        g1: usize,
        overlaps: bool,
        out: &mut Vec<CardRangeHit>,
    ) -> Result<(), CardWalkBreak> {
        let group = &ranges[g0..g1];
        let len = self.len;
        let mut cursor = 0usize;
        let mut ri = 0usize;
        let mut region_end = 0usize;
        let mut in_region = false;
        for &(lo, hi) in group {
            if hi <= cursor {
                continue;
            }
            if lo > cursor {
                let w = self.walk_start(lo)?;
                if w > cursor {
                    cursor = w;
                    in_region = false;
                }
            }
            while cursor < hi && cursor < len {
                if !in_region || cursor >= region_end {
                    match locate(self.sorted, cursor, len) {
                        Loc::Free { end } => {
                            cursor = end;
                            in_region = false;
                            continue;
                        }
                        Loc::Allocated { end, .. } => {
                            region_end = end;
                            in_region = true;
                        }
                    }
                }
                match object_extent_at(self.base, cursor) {
                    Some((size, is_ref)) if cursor + size <= region_end => {
                        let earlier = cursor < group[0].0 && {
                            let i = ranges[..g0].partition_point(|&(_, h)| h <= cursor);
                            i < g0 && ranges[i].0 <= cursor
                        };
                        if !earlier {
                            visit(
                                self.base, cursor, size, is_ref, group, &mut ri, overlaps, out,
                            );
                        }
                        cursor += size;
                    }
                    _ => return Err(CardWalkBreak),
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// gen r5w5/old9 (2026-09-27): the object-base ORACLE and the exact rebuild.
//
// `gengc-r5w3-oldgen7-proposal-bot-object-base-oracle-for-an-o-live-sweep`,
// steps 1 and 4. A separate `impl` block so the hunks stay apart from the
// dirty-card walk above.
// ---------------------------------------------------------------------------

impl OldGen {
    /// Which object, if any, holds the byte at absolute address `addr`?
    ///
    /// The block-offset table's replacement for the stop-the-world mark's
    /// admission oracle (a binary search over `walk_objects_with_gaps`, which
    /// visits every allocated object, dead ones included, on every
    /// collection). Answered from the free list (which allocated region, if
    /// any, holds `addr`) and the card's trusted anchor: a forward stride from
    /// the anchor, header by header, to the object containing `addr`. The
    /// stride is bounded by the objects between the anchor and `addr`, i.e.
    /// by one card plus one object when the card's anchor is exact — which it
    /// is after a re-derivation
    /// ([`Self::bot_extend`]), and, for every card a stride PASSES, after this
    /// function (`bot_compress`). A card anchored at an allocation block's
    /// start (a promotion buffer: up to 256 KiB of objects) is the long
    /// stride, paid once per buffer.
    ///
    /// Exactly the answer the walk gives, on every address, on a heap whose
    /// walk has no gap (`tests::the_object_oracle_matches_the_walk_*`). Where
    /// the answer would depend on a header nobody can size — a walk gap — or
    /// the free list overlaps, it is [`ObjectAt::Unknown`], never a guess.
    ///
    /// `&self` like the dirty-card walk: the table and the sorted free-list
    /// view are behind `RefCell`s. Callers hold the old-gen lock (the mark
    /// runs inside the pause), so nothing mutates the generation meanwhile.
    #[cfg(test)]
    pub fn object_at(&self, addr: usize) -> ObjectAt {
        let base = self.data.as_ptr() as usize;
        let len = self.data.len();
        let Some(p) = addr.checked_sub(base).filter(|&p| p < len) else {
            return ObjectAt::Free;
        };
        let mut bot = self.bot.borrow_mut();
        if !bot.enabled {
            return ObjectAt::Unknown;
        }
        bot.stats.oracle_queries += 1;
        let answer =
            self.with_sorted_free_blocks(|sorted| self.bot_object_at(&mut bot, sorted, base, p));
        if answer == ObjectAt::Unknown {
            bot.stats.oracle_unknown += 1;
        }
        answer
    }

    /// The body of [`Self::object_at`] for offset `p < len`.
    #[cfg(test)]
    fn bot_object_at(
        &self,
        bot: &mut BlockOffsetTable,
        sorted: &[FreeBlock],
        base: usize,
        p: usize,
    ) -> ObjectAt {
        let len = self.data.len();
        let seq = self.free_list_seq();
        if bot.free_seq_seen != Some(seq) {
            bot.free_disjoint = free_list_is_disjoint(sorted, len);
            bot.free_seq_seen = Some(seq);
        }
        if !bot.free_disjoint {
            return ObjectAt::Unknown;
        }
        let (start, region_end) = match locate(sorted, p, len) {
            Loc::Free { .. } => return ObjectAt::Free,
            Loc::Allocated { start, end } => (start, end),
        };
        let card = p / BOT_CARD;
        if !self.bot_extend(bot, sorted, card + 1) || !bot.enabled {
            return ObjectAt::Unknown;
        }
        // Where to start: the card's trusted anchor (INV: a walk boundary at
        // or below the card's first byte with no free block in between), or
        // the region's start when the card's first byte lies in the free
        // block below this region (then `p - start < BOT_CARD`).
        let cs = card * BOT_CARD;
        let mut cursor = if cs < start {
            start
        } else {
            match bot.anchor(card) {
                Some(a) if a >= start && a <= cs => a,
                _ => {
                    // INV says this cannot happen; the region start is correct
                    // anyway, as in `bot_walk_start`.
                    bot.stats.anomalies += 1;
                    start
                }
            }
        };
        while cursor <= p {
            match self.bot_object_extent(cursor) {
                Some((size, _)) if cursor + size <= region_end => {
                    bot.stats.oracle_strides += 1;
                    let end = cursor + size;
                    // Every card whose first byte this object covers may be
                    // anchored exactly at its start.
                    Self::bot_compress(bot, cursor, end);
                    if p < end {
                        return if p == cursor {
                            ObjectAt::Base { size }
                        } else {
                            ObjectAt::Interior {
                                base: base + cursor,
                                size,
                            }
                        };
                    }
                    cursor = end;
                }
                _ => {
                    // A header no walk can size (or an extent past its
                    // region): a walk gap. Refuse rather than guess.
                    bot.stats.anomalies += 1;
                    return ObjectAt::Unknown;
                }
            }
        }
        // Unreachable on a table that keeps INV: the stride starts at or below
        // `p` and returns from inside the object that covers it.
        bot.stats.anomalies += 1;
        ObjectAt::Unknown
    }

    /// Path compression for [`Self::object_at`]: every TRUSTED card whose
    /// first byte lies in the object `[start, end)` is anchored at `start`.
    ///
    /// Always INV-preserving: the object covering a card's first byte is the
    /// highest walk boundary at or below it, and `[start, cs]` lies inside
    /// one object, so no free block intersects it. The loop stops at the
    /// first card already anchored at `start` or above: after that the cards
    /// were compressed (or anchored exactly) before, and stopping early only
    /// ever compresses less, never wrongly.
    #[cfg(test)]
    fn bot_compress(bot: &mut BlockOffsetTable, start: usize, end: usize) {
        let anchor = (start / 8) as u32;
        let first = start.div_ceil(BOT_CARD);
        let last = end
            .div_ceil(BOT_CARD)
            .min(bot.valid_cards)
            .min(bot.entries.len());
        for c in first..last {
            if bot.entries[c] >= anchor {
                break;
            }
            bot.entries[c] = anchor;
            bot.stats.oracle_compressed += 1;
        }
    }

    // gcg g6/og (2026-10-02): `rebuild_block_offsets_from_live`,
    // `kept_object_size` and `anchor_carved_span` (old10 item 4, never wired)
    // were deleted with the O(live) sweep's API; nothing else called them.
}

#[cfg(test)]
mod tests;
