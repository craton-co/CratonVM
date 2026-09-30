// SPDX-License-Identifier: Apache-2.0
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
//! (`docs/internal/gc/gengc-r4-cards-dirty-card-object-walk-is-o-old-live-FIXED-20260923.md`).
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
use crate::card_table::CARD_SIZE;
use crate::heap::{
    array_data_size, ArrayElementType, ObjectHeader, ObjectKind, ARRAY_DATA_OFFSET, HEADER_SIZE,
};

/// Bytes per block-offset card: the card table's granularity, so a dirty
/// card range maps onto whole entries.
pub(crate) const BOT_CARD: usize = CARD_SIZE;

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
    pub oracle_queries: u64,
    pub oracle_unknown: u64,
    pub oracle_strides: u64,
    pub oracle_compressed: u64,
    /// gen r5w5/old9 — whole-table rebuilds from a live sweep's kept objects
    /// ([`OldGen::rebuild_block_offsets_from_live`]).
    pub rebuilds: u64,
}

/// gen r5w5/old9 (2026-09-27) — what [`OldGen::object_at`] found at an
/// absolute address: the block-offset table's answer to "which object, if
/// any, holds this byte". The stop-the-world old-gen mark used to answer that
/// question only by a binary search over a walk of EVERY allocated object
/// (`walk_objects_with_gaps`), which is what made each old-gen collection
/// O(allocated) even when almost nothing survived
/// (`gengc-r5w3-oldgen7-proposal-bot-object-base-oracle-for-an-o-live-sweep`).
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
pub(crate) struct BlockOffsetTable {
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
    pub(crate) fn new(capacity: usize, requested: bool) -> Self {
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
    pub(crate) fn note_alloc(&mut self, offset: usize, size: usize) {
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
    pub(crate) fn note_free(&mut self, offset: usize) {
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
    pub(crate) fn note_relayout(&mut self) {
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
    ends_ok && sorted.windows(2).all(|w| w[0].offset + w[0].size <= w[1].offset)
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
pub(super) fn gap_window(ranges: &[(usize, usize)], g0: usize, g1: usize) -> Option<(usize, usize)> {
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
        let ptr = (self.data.as_ptr() as usize + offset) as *mut u8;
        let kind = Self::validate_header_tags_or_desync(ptr, offset)?;
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
                        visit(base, cursor, size, is_ref, ranges, &mut ri, overlaps, &mut out);
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
                && !self.touch_region(base, cursor, block.offset, ranges, &mut ri, last_hi, &mut out, gaps)
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
            let fits = self.bot_object_extent(o).filter(|&(size, _)| o + size <= end);
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
    /// is after a live sweep's rebuild
    /// ([`Self::rebuild_block_offsets_from_live`]), after a re-derivation
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

    /// gen r5w5/old9 — re-anchor the whole table from the objects a live
    /// sweep KEPT (`live_offsets`: `(offset, size)`, ascending, disjoint),
    /// and trust every card.
    ///
    /// Call it right after that sweep freed every other allocated byte
    /// ([`Self::sweep_dead_runs_around`]): then every card whose first byte is
    /// allocated has it inside a kept object, and anchoring it at that
    /// object's start is exact (INV with the highest possible anchor); every
    /// other card's first byte is free and never consulted. O(kept objects +
    /// cards they cover) — the live set, not the generation — and it is what
    /// makes the NEXT collection's oracle queries one-card strides instead of
    /// a re-derivation from the lowest byte the sweep freed (each `free`
    /// lowers the trusted prefix to its own card).
    pub(crate) fn rebuild_block_offsets_from_live(&mut self, live_offsets: &[(usize, usize)]) {
        let cards = self.growth_max().div_ceil(BOT_CARD);
        let bot = self.bot.get_mut();
        if !bot.enabled {
            return;
        }
        let need = live_offsets
            .iter()
            .map(|&(o, s)| (o + s).div_ceil(BOT_CARD))
            .max()
            .unwrap_or(0);
        if bot.entries.len() < need {
            bot.entries.resize(need, 0);
        }
        for &(o, s) in live_offsets {
            let anchor = (o / 8) as u32;
            let first = o.div_ceil(BOT_CARD);
            let last = (o + s).div_ceil(BOT_CARD);
            if first < last {
                for e in &mut bot.entries[first..last] {
                    *e = anchor;
                }
            }
        }
        bot.valid_cards = cards.max(need);
        // The free list changed under the sweep: re-judge its disjointness.
        bot.free_seq_seen = None;
        bot.stats.rebuilds += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::SLOT_SIZE;
    use cratonvm_types::ClassId;

    /// Deterministic xorshift, so a failing seed can be replayed.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Footprint of a legacy object with `slots` 16-byte cells.
    fn object_size(slots: u32) -> usize {
        HEADER_SIZE + SLOT_SIZE * slots as usize
    }

    /// Footprint of a reference array of `len` elements.
    fn ref_array_size(len: u32) -> usize {
        let data = array_data_size(len as usize, ArrayElementType::Reference).unwrap();
        (ARRAY_DATA_OFFSET + data + 7) & !7
    }

    /// Footprint of an `int[]` of `len` elements.
    fn int_array_size(len: u32) -> usize {
        let data = array_data_size(len as usize, ArrayElementType::Int).unwrap();
        (ARRAY_DATA_OFFSET + data + 7) & !7
    }

    /// One object to lay down: `(footprint, header)`.
    fn random_object(rng: &mut Rng) -> (usize, ObjectHeader) {
        match rng.below(5) {
            0 | 1 => {
                let slots = rng.below(6) as u32;
                (
                    object_size(slots),
                    ObjectHeader::new(ClassId::new(1), ObjectKind::Object, ArrayElementType::Reference, 0, slots),
                )
            }
            2 => {
                // Up to ~4 cards of references, so arrays straddle cards.
                let len = rng.below(260) as u32;
                (
                    ref_array_size(len),
                    ObjectHeader::new(ClassId::new(2), ObjectKind::Array, ArrayElementType::Reference, len, len),
                )
            }
            3 => {
                // Occasionally a big one spanning many cards.
                let len = if rng.below(8) == 0 { 2000 + rng.below(3000) as u32 } else { rng.below(40) as u32 };
                (
                    ref_array_size(len),
                    ObjectHeader::new(ClassId::new(2), ObjectKind::Array, ArrayElementType::Reference, len, len),
                )
            }
            _ => {
                let len = rng.below(300) as u32;
                (
                    int_array_size(len),
                    ObjectHeader::new(ClassId::new(3), ObjectKind::Array, ArrayElementType::Int, len, len),
                )
            }
        }
    }

    /// Allocate one object (its own block) and write its header.
    fn alloc_one(og: &mut OldGen, rng: &mut Rng) -> Option<(usize, usize)> {
        let (size, header) = random_object(rng);
        let ptr = og.alloc(size, 8)?;
        // SAFETY: `ptr` is a fresh, zeroed old-gen block of `size` bytes.
        unsafe { std::ptr::write(ptr as *mut ObjectHeader, header) };
        Some((ptr as usize - og.base_ptr() as usize, size))
    }

    /// A promotion-buffer-shaped block: carve objects end to end from its
    /// start, then hand the unused tail back — what `gen_evac` does.
    fn alloc_buffer(og: &mut OldGen, rng: &mut Rng, out: &mut Vec<(usize, usize)>) {
        let cap = 4096 + rng.below(8) * 1024;
        let Some(p) = og.alloc_unzeroed_buffer(cap, 8) else {
            return;
        };
        let base = og.base_ptr() as usize;
        let start = p as usize - base;
        let mut cursor = 0usize;
        while rng.below(12) != 0 {
            let (size, header) = random_object(rng);
            if cursor + size > cap {
                break;
            }
            // `gen_evac::old_lab_alloc`'s rule: never leave a tail that cannot
            // go back to the free list (0 or at least a header).
            let rest = cap - cursor - size;
            if rest != 0 && rest < HEADER_SIZE {
                break;
            }
            // SAFETY: `[p + cursor, p + cursor + size)` lies in the buffer.
            unsafe {
                std::ptr::write_bytes(p.add(cursor), 0, size);
                std::ptr::write(p.add(cursor) as *mut ObjectHeader, header);
            }
            out.push((start + cursor, size));
            cursor += size;
        }
        // 0, or at least HEADER_SIZE by the rule above (cap >= 4096 > size).
        let tail = cap - cursor;
        if tail > 0 {
            // SAFETY: `[p + cursor, p + cap)` is the never-written tail of the
            // buffer (the whole buffer when nothing was carved).
            unsafe { og.release_unused_tail(p.add(cursor), tail) };
        }
    }

    /// The oracle: every object from the unfiltered `walk_objects`, reported
    /// the way `walk_card_ranges` promises.
    fn oracle(og: &OldGen, ranges: &[(usize, usize)], overlaps: bool) -> Vec<CardRangeHit> {
        let base = og.base_ptr() as usize;
        let mut out = Vec::new();
        for (ptr, size) in og.walk_objects() {
            let o = ptr as usize - base;
            let end = o + size;
            if ranges.iter().any(|&(lo, hi)| o >= lo && o < hi) {
                out.push(CardRangeHit { ptr, size, window: None });
                continue;
            }
            if !overlaps {
                continue;
            }
            // SAFETY: `walk_objects` yields valid headers.
            let h = unsafe { &*(ptr as *const ObjectHeader) };
            if h.kind() != ObjectKind::Array || h.element_type() != ArrayElementType::Reference {
                continue;
            }
            for &(lo, hi) in ranges {
                if lo > o && lo < end {
                    out.push(CardRangeHit {
                        ptr,
                        size,
                        window: Some((lo - o, hi.min(end) - o)),
                    });
                }
            }
        }
        out
    }

    /// Random dirty ranges, card-aligned and coalesced exactly the way
    /// `scan_dirty_cards_inner` builds them.
    fn random_ranges(rng: &mut Rng, len: usize) -> Vec<(usize, usize)> {
        let cards = len / BOT_CARD;
        let mut idx: Vec<usize> = (0..rng.below(24)).map(|_| rng.below(cards)).collect();
        if rng.below(4) == 0 {
            idx.extend([0, cards - 1]);
        }
        idx.sort_unstable();
        idx.dedup();
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for c in idx {
            let (lo, hi) = (c * BOT_CARD, (c + 1) * BOT_CARD);
            if let Some(last) = ranges.last_mut() {
                if last.1 >= lo {
                    last.1 = last.1.max(hi);
                    continue;
                }
            }
            ranges.push((lo, hi));
        }
        ranges
    }

    /// THE equivalence test. A randomized old generation — single-object
    /// blocks, promotion-buffer blocks with released tails, arrays spanning
    /// many cards, frees (so blocks are re-used at different boundaries),
    /// coalescing, and a compaction now and then — with walks interleaved
    /// between the mutations, so the trusted prefix is extended, cut back by
    /// frees and re-derived many times. After every mutation batch the
    /// anchored walk must return EXACTLY what the linear walk returns, and,
    /// with overlaps, exactly what the unfiltered-walk oracle says.
    #[test]
    fn the_anchored_walk_matches_the_linear_walk_on_randomized_heaps() {
        for seed in 1..=12u64 {
            let mut rng = Rng(0xD1B5_4A32_D192_ED03 ^ (seed * 0x9E37_79B9));
            let mut og = OldGen::new(512 * 1024);
            og.set_block_offset_table_enabled(true);
            let mut live: Vec<(usize, usize)> = Vec::new();
            for round in 0..60 {
                // Mutate.
                for _ in 0..rng.below(40) {
                    match rng.below(10) {
                        0..=4 => {
                            if let Some(o) = alloc_one(&mut og, &mut rng) {
                                live.push(o);
                            }
                        }
                        5 | 6 => alloc_buffer(&mut og, &mut rng, &mut live),
                        _ => {
                            if !live.is_empty() {
                                let (off, size) = live.swap_remove(rng.below(live.len()));
                                let p = (og.base_ptr() as usize + off) as *mut u8;
                                // SAFETY: `(off, size)` is a live object of this
                                // generation, freed exactly once.
                                unsafe { og.free(p, size) };
                            }
                        }
                    }
                }
                if rng.below(5) == 0 {
                    og.coalesce_free_blocks();
                }
                if rng.below(25) == 0 {
                    // A compaction with everything live: relocates, so the
                    // table must start over.
                    for (off, _) in &live {
                        let h = (og.base_ptr() as usize + off) as *const ObjectHeader;
                        // SAFETY: a live header of this generation.
                        unsafe { &*h }.add_gc_flags(crate::heap::GC_FLAG_MARKED);
                    }
                    let map = og.compact();
                    let base = og.base_ptr() as usize;
                    for (off, _) in live.iter_mut() {
                        if let Some(&n) = map.get(&(base + *off)) {
                            *off = n - base;
                        }
                    }
                }
                // Walk, several times per round.
                for _ in 0..3 {
                    let ranges = random_ranges(&mut rng, og.capacity());
                    for overlaps in [false, true] {
                        let got = og.walk_card_ranges(&ranges, overlaps);
                        assert_eq!(
                            got,
                            oracle(&og, &ranges, overlaps),
                            "seed {seed} round {round} overlaps={overlaps}: anchored walk vs oracle"
                        );
                    }
                    let linear: Vec<CardRangeHit> = og
                        .walk_objects_in_card_ranges(&ranges)
                        .into_iter()
                        .map(|(ptr, size)| CardRangeHit { ptr, size, window: None })
                        .collect();
                    assert_eq!(
                        og.walk_card_ranges(&ranges, false),
                        linear,
                        "seed {seed} round {round}: anchored walk vs walk_objects_in_card_ranges"
                    );
                }
            }
            let stats = og.block_offset_stats();
            assert!(stats.enabled, "seed {seed}: the table must not have latched off");
            assert_eq!(stats.anomalies, 0, "seed {seed}: {stats:?}");
            // gen r5w1/oldgen5: a sane heap has no walk gaps.
            assert_eq!(stats.gap_windows, 0, "seed {seed}: {stats:?}");
            assert_eq!(stats.gap_rederivations, 0, "seed {seed}: {stats:?}");
            assert!(stats.walks > 0 && stats.anchored_ranges > 0, "seed {seed}: {stats:?}");
            assert!(stats.invalidations > 0, "seed {seed}: frees must have cut the prefix");
        }
    }

    /// The linear overlap walk (the fallback when the table is off) agrees
    /// with the oracle too.
    #[test]
    fn the_linear_overlap_walk_matches_the_oracle() {
        for seed in 1..=6u64 {
            let mut rng = Rng(0xA076_1D64_78BD_642F ^ seed);
            let mut og = OldGen::new(256 * 1024);
            og.set_block_offset_table_enabled(false);
            let mut live = Vec::new();
            for _ in 0..200 {
                if rng.below(3) == 0 {
                    alloc_buffer(&mut og, &mut rng, &mut live);
                } else if let Some(o) = alloc_one(&mut og, &mut rng) {
                    live.push(o);
                }
                if rng.below(4) == 0 && !live.is_empty() {
                    let (off, size) = live.swap_remove(rng.below(live.len()));
                    // SAFETY: a live object, freed once.
                    unsafe { og.free((og.base_ptr() as usize + off) as *mut u8, size) };
                }
            }
            for _ in 0..20 {
                let ranges = random_ranges(&mut rng, og.capacity());
                assert_eq!(og.walk_card_ranges(&ranges, true), oracle(&og, &ranges, true), "seed {seed}");
                assert_eq!(og.walk_card_ranges(&ranges, false), oracle(&og, &ranges, false), "seed {seed}");
            }
            assert_eq!(og.block_offset_stats().walks, 0, "the table is off");
        }
    }

    /// The point of the table: a dirty card near the TOP of a generation full
    /// of small objects decodes the objects near that card, not all of them.
    /// Measured in headers decoded while re-deriving, which is the cost the
    /// linear walk paid on every pause.
    #[test]
    fn a_dirty_card_at_the_top_does_not_decode_the_whole_generation() {
        let mut og = OldGen::new(1024 * 1024);
        og.set_block_offset_table_enabled(true);
        let size = object_size(1);
        let mut offs = Vec::new();
        while let Some(p) = og.alloc(size, 8) {
            // SAFETY: fresh block.
            unsafe {
                std::ptr::write(
                    p as *mut ObjectHeader,
                    ObjectHeader::new(ClassId::new(1), ObjectKind::Object, ArrayElementType::Reference, 0, 1),
                )
            };
            offs.push(p as usize - og.base_ptr() as usize);
        }
        let n = offs.len();
        assert!(n > 20_000);
        let top = offs[n - 3];
        let card = top / BOT_CARD;
        let ranges = [(card * BOT_CARD, (card + 1) * BOT_CARD)];
        // Every card was anchored at allocation: nothing needs re-deriving.
        let hits = og.walk_card_ranges(&ranges, false);
        assert!(hits.iter().any(|h| h.ptr as usize - og.base_ptr() as usize == top));
        let s = og.block_offset_stats();
        assert_eq!(s.rederived_objects, 0, "allocation anchored every card: {s:?}");
        assert_eq!(s.anchored_ranges, 1);

        // Free one object near the bottom: the prefix above it is re-derived
        // ONCE, and the next pause pays nothing again.
        // SAFETY: a live object, freed once.
        unsafe { og.free((og.base_ptr() as usize + offs[10]) as *mut u8, size) };
        og.walk_card_ranges(&ranges, false);
        let once = og.block_offset_stats().rederived_objects;
        assert!(once > 0 && once <= n as u64);
        og.walk_card_ranges(&ranges, false);
        assert_eq!(og.block_offset_stats().rederived_objects, once, "re-derived once, then trusted");
    }

    /// A double free leaves overlapping free blocks; the anchored walk cannot
    /// ask "which block covers p" of such a list, so it must hand the walk to
    /// the linear one — whose `max` guard already copes.
    #[test]
    fn an_overlapping_free_list_falls_back_to_the_linear_walk() {
        let mut og = OldGen::new(64 * 1024);
        og.set_block_offset_table_enabled(true);
        let size = object_size(2);
        let mut offs = Vec::new();
        for _ in 0..8 {
            let p = og.alloc(size, 8).unwrap();
            // SAFETY: fresh block.
            unsafe {
                std::ptr::write(
                    p as *mut ObjectHeader,
                    ObjectHeader::new(ClassId::new(1), ObjectKind::Object, ArrayElementType::Reference, 0, 2),
                )
            };
            offs.push(p as usize - og.base_ptr() as usize);
        }
        let base = og.base_ptr() as usize;
        // SAFETY: the modelled defect — a span freed, then a piece of it again.
        unsafe {
            og.free((base + offs[2]) as *mut u8, size * 4);
            og.free((base + offs[3]) as *mut u8, 24);
        }
        let full = [(0usize, offs[7] + size)];
        let want: Vec<CardRangeHit> = og
            .walk_objects_in_card_ranges(&full)
            .into_iter()
            .map(|(ptr, size)| CardRangeHit { ptr, size, window: None })
            .collect();
        assert_eq!(og.walk_card_ranges(&full, false), want);
        assert_eq!(og.block_offset_stats().fallbacks, 1);
    }

    // --- gen r5w1/oldgen5: walk gaps ----------------------------------------

    /// Carve `count` legacy objects of `slots` slots end to end out of one
    /// promotion-buffer-shaped block, consumed to the byte (no tail). Returns
    /// their offsets.
    fn carve_buffer(og: &mut OldGen, count: usize, slots: u32) -> Vec<usize> {
        let size = object_size(slots);
        let p = og.alloc_unzeroed_buffer(size * count, 8).expect("room for the buffer");
        let base = og.base_ptr() as usize;
        let mut offs = Vec::with_capacity(count);
        for i in 0..count {
            // SAFETY: `[p + i*size, p + (i+1)*size)` lies inside the buffer.
            unsafe {
                let q = p.add(i * size);
                std::ptr::write_bytes(q, 0, size);
                std::ptr::write(
                    q as *mut ObjectHeader,
                    ObjectHeader::new(ClassId::new(1), ObjectKind::Object, ArrayElementType::Reference, 0, slots),
                );
            }
            offs.push(p as usize - base + i * size);
        }
        offs
    }

    /// One legacy object on a block of its own.
    fn own_block(og: &mut OldGen, slots: u32) -> usize {
        let p = og.alloc(object_size(slots), 8).expect("room");
        // SAFETY: a fresh, zeroed block of that size.
        unsafe {
            std::ptr::write(
                p as *mut ObjectHeader,
                ObjectHeader::new(ClassId::new(1), ObjectKind::Object, ArrayElementType::Reference, 0, slots),
            )
        };
        p as usize - og.base_ptr() as usize
    }

    /// Make the header at `off` one no walk can size (an invalid kind tag).
    fn break_header(og: &OldGen, off: usize) {
        let p = og.base_ptr() as usize + off + cratonvm_types::KIND_TAGS_BYTE_OFFSET;
        // SAFETY: a byte inside a live header of this generation.
        unsafe { std::ptr::write(p as *mut u8, 0xFF) };
    }

    fn rel(og: &OldGen, hits: &[CardRangeHit]) -> Vec<usize> {
        let base = og.base_ptr() as usize;
        hits.iter().map(|h| h.ptr as usize - base).collect()
    }

    #[test]
    fn a_gap_window_runs_from_the_first_dirty_byte_to_the_resume_point() {
        let ranges = [(512, 1024), (2048, 2560)];
        // No range reaches the stretch.
        assert_eq!(gap_window(&ranges, 1024, 2048), None);
        assert_eq!(gap_window(&ranges, 3000, 4000), None);
        // A range starting inside it: from that range's start to the end.
        assert_eq!(gap_window(&ranges, 1100, 4000), Some((2048, 4000)));
        // A range the break lies inside: from the break.
        assert_eq!(gap_window(&ranges, 600, 700), Some((600, 700)));
        assert_eq!(gap_window(&ranges, 600, 600), None);
        let mut gaps = Vec::new();
        push_gap(&mut gaps, (100, 200));
        push_gap(&mut gaps, (200, 300));
        push_gap(&mut gaps, (400, 500));
        assert_eq!(gaps, vec![(100, 300), (400, 500)]);
    }

    /// The anchored walk reports the unparseable stretch as a window and
    /// resumes at the next allocation block's trusted anchor, so the objects
    /// of that block are still reported precisely; the linear walk reports the
    /// rest of the region as the window.
    #[test]
    fn a_walk_gap_is_a_window_and_the_anchored_walk_resumes_at_the_next_block() {
        for bot_on in [true, false] {
            for overlaps in [false, true] {
                let mut og = OldGen::new(64 * 1024);
                og.set_block_offset_table_enabled(bot_on);
                // Two buffers back to back: one allocated region.
                let first = carve_buffer(&mut og, 26, 4);
                let second = carve_buffer(&mut og, 26, 4);
                assert_eq!(second[0], first[25] + object_size(4), "contiguous");
                assert!(second[0] > BOT_CARD, "the second buffer owns a card's first byte");
                break_header(&og, first[1]);
                let end = second[25] + object_size(4);
                let everything = [(0usize, end.div_ceil(BOT_CARD) * BOT_CARD)];
                let (hits, gaps) = og.walk_card_ranges_with_gaps(&everything, overlaps);
                if bot_on {
                    let mut want = vec![first[0]];
                    want.extend_from_slice(&second);
                    assert_eq!(rel(&og, &hits), want, "overlaps={overlaps}");
                    assert_eq!(gaps, vec![(first[1], second[0])], "overlaps={overlaps}");
                    let s = og.block_offset_stats();
                    assert!(s.enabled);
                    assert_eq!(s.gap_anchor_resumes, 1, "{s:?}");
                    assert_eq!(s.gap_windows, 1, "{s:?}");
                } else {
                    assert_eq!(rel(&og, &hits), vec![first[0]], "overlaps={overlaps}");
                    assert_eq!(gaps, vec![(first[1], end)], "overlaps={overlaps}");
                }
                // A dirty range that ends at the break sees no window.
                let front = [(0usize, first[1])];
                let (front_hits, none) = og.walk_card_ranges_with_gaps(&front, overlaps);
                assert_eq!(rel(&og, &front_hits), vec![first[0]]);
                assert!(none.is_empty(), "bot_on={bot_on} overlaps={overlaps}: {none:?}");
            }
        }
    }

    /// Re-deriving the table over a header it cannot size used to latch the
    /// table off for the whole generation. It now anchors the stretch at the
    /// break: the walk reports it as a window, a later region is still walked
    /// from its own anchors, and the table stays on.
    #[test]
    fn re_deriving_over_a_walk_gap_keeps_the_table_on() {
        let mut og = OldGen::new(64 * 1024);
        og.set_block_offset_table_enabled(true);
        let first = carve_buffer(&mut og, 26, 4);
        let second = carve_buffer(&mut og, 26, 4);
        let region_end = second[25] + object_size(4);
        let spacer = own_block(&mut og, 1);
        let z = own_block(&mut og, 1);
        assert_eq!(spacer, region_end);
        break_header(&og, first[1]);
        let base = og.base_ptr() as usize;
        // SAFETY: live objects of this generation, each freed once. The first
        // free lowers the trusted prefix below the break; the second puts a
        // free block between the broken region and `z`.
        unsafe {
            og.free((base + first[0]) as *mut u8, object_size(4));
            og.free((base + spacer) as *mut u8, object_size(1));
        }
        let last_card = second[25] / BOT_CARD * BOT_CARD;
        let z_card_end = (z / BOT_CARD + 1) * BOT_CARD;
        let ranges = [(last_card, z_card_end)];
        let (hits, gaps) = og.walk_card_ranges_with_gaps(&ranges, false);
        assert_eq!(rel(&og, &hits), vec![z], "the later region is still walked");
        assert_eq!(gaps, vec![(last_card, region_end)], "the stretch from the dirty byte on");
        let s = og.block_offset_stats();
        assert!(s.enabled, "the table must stay on: {s:?}");
        assert_eq!(s.gap_rederivations, 1, "{s:?}");
        // The same answer the linear walk gives.
        og.set_block_offset_table_enabled(false);
        let (lin_hits, lin_gaps) = og.walk_card_ranges_with_gaps(&ranges, false);
        assert_eq!(lin_hits, hits);
        assert_eq!(lin_gaps, gaps);
    }

    /// An allocation anchors exactly the cards whose first byte it covers.
    #[test]
    fn note_alloc_anchors_the_cards_whose_first_byte_the_block_covers() {
        let mut t = BlockOffsetTable::new(1 << 20, true);
        t.note_alloc(BOT_CARD - 16, 3 * BOT_CARD);
        // Card 0 starts before the block; cards 1..=3 start inside it.
        assert_eq!(t.anchor(0), Some(0), "untouched (resized with 0)");
        for c in 1..=3 {
            assert_eq!(t.anchor(c), Some(BOT_CARD - 16), "card {c}");
        }
        assert_eq!(t.anchor(4), None);
        // A free lowers the trusted prefix to the freed object's card.
        t.valid_cards = 4;
        t.note_free(2 * BOT_CARD + 8);
        assert_eq!(t.valid_cards, 3);
        t.note_free(5 * BOT_CARD);
        assert_eq!(t.valid_cards, 3, "a free above the prefix changes nothing");
        t.note_relayout();
        assert_eq!(t.valid_cards, 0);
        // Too big to encode: built off.
        assert!(!BlockOffsetTable::new(usize::MAX, true).enabled);
        assert!(!BlockOffsetTable::new(1 << 20, false).enabled);
    }

    // --- gen r5w5/old9: the object-base oracle ------------------------------

    /// What the WALK says is at absolute address `a`: the answer
    /// [`OldGen::object_at`] must give on a heap whose walk covers every
    /// allocated byte.
    fn walked_answer(walked: &[(*mut u8, usize)], a: usize) -> ObjectAt {
        let i = walked.partition_point(|&(p, _)| p as usize <= a);
        if i > 0 {
            let (p, s) = (walked[i - 1].0 as usize, walked[i - 1].1);
            if a < p + s {
                return if a == p {
                    ObjectAt::Base { size: s }
                } else {
                    ObjectAt::Interior { base: p, size: s }
                };
            }
        }
        ObjectAt::Free
    }

    /// The differential: every walked base, three interior bytes of every
    /// walked object, `samples` random addresses across the whole capacity and
    /// both ends outside the storage must get exactly the walk's answer.
    fn assert_oracle_matches_walk(og: &OldGen, rng: &mut Rng, samples: usize, what: &str) {
        let base = og.base_ptr() as usize;
        let walked = og.walk_objects();
        let walked_bytes: usize = walked.iter().map(|&(_, s)| s).sum();
        assert_eq!(walked_bytes, og.used(), "{what}: precondition, the walk covers every allocated byte");
        for &(p, size) in &walked {
            let p = p as usize;
            assert_eq!(og.object_at(p), ObjectAt::Base { size }, "{what}: base +{:#x}", p - base);
            for q in [p + 8, p + size / 2, p + size - 1] {
                if q > p {
                    assert_eq!(
                        og.object_at(q),
                        ObjectAt::Interior { base: p, size },
                        "{what}: interior +{:#x} of +{:#x}",
                        q - base,
                        p - base,
                    );
                }
            }
        }
        let cap = og.capacity();
        for _ in 0..samples {
            let a = base + rng.below(cap);
            assert_eq!(og.object_at(a), walked_answer(&walked, a), "{what}: sample +{:#x}", a - base);
        }
        assert_eq!(og.object_at(base - 8), ObjectAt::Free, "{what}: below the storage");
        assert_eq!(og.object_at(base + cap), ObjectAt::Free, "{what}: past the storage");
        assert_eq!(og.object_at(0), ObjectAt::Free, "{what}: null");
    }

    /// FRAGMENTED layouts: the randomized heap of the anchored-walk test —
    /// single-object blocks, promotion buffers with released tails, frees at
    /// random, coalescing, and a compaction now and then — with the oracle
    /// asked about every object after every mutation batch. The trusted prefix
    /// is cut back by the frees and re-derived by the oracle itself.
    #[test]
    fn the_object_oracle_matches_the_walk_on_fragmented_heaps() {
        for seed in 1..=10u64 {
            let mut rng = Rng(0x5851_F42D_4C95_7F2D ^ (seed * 0x9E37_79B9));
            let mut og = OldGen::new(512 * 1024);
            og.set_block_offset_table_enabled(true);
            let mut live: Vec<(usize, usize)> = Vec::new();
            for round in 0..40 {
                for _ in 0..rng.below(40) {
                    match rng.below(10) {
                        0..=4 => {
                            if let Some(o) = alloc_one(&mut og, &mut rng) {
                                live.push(o);
                            }
                        }
                        5 | 6 => alloc_buffer(&mut og, &mut rng, &mut live),
                        _ => {
                            if !live.is_empty() {
                                let (off, size) = live.swap_remove(rng.below(live.len()));
                                // SAFETY: a live object of this generation,
                                // freed exactly once.
                                unsafe { og.free((og.base_ptr() as usize + off) as *mut u8, size) };
                            }
                        }
                    }
                }
                if rng.below(5) == 0 {
                    og.coalesce_free_blocks();
                }
                if rng.below(20) == 0 {
                    for (off, _) in &live {
                        let h = (og.base_ptr() as usize + off) as *const ObjectHeader;
                        // SAFETY: a live header of this generation.
                        unsafe { &*h }.add_gc_flags(crate::heap::GC_FLAG_MARKED);
                    }
                    let map = og.compact();
                    let base = og.base_ptr() as usize;
                    for (off, _) in live.iter_mut() {
                        if let Some(&n) = map.get(&(base + *off)) {
                            *off = n - base;
                        }
                    }
                }
                assert_oracle_matches_walk(&og, &mut rng, 300, &format!("seed {seed} round {round}"));
            }
            let s = og.block_offset_stats();
            assert!(s.enabled, "seed {seed}: {s:?}");
            assert_eq!(s.oracle_unknown, 0, "seed {seed}: a sane heap is always answerable: {s:?}");
            assert_eq!(s.anomalies, 0, "seed {seed}: {s:?}");
            assert!(s.oracle_queries > 0 && s.oracle_strides > 0, "seed {seed}: {s:?}");
        }
    }

    /// HUMONGOUS layouts: multi-megabyte arrays on blocks of their own (one
    /// placed from the top of the generation, which leaves a free gap below
    /// it), small objects and buffers around and between them, frees on both
    /// sides. A query deep inside a humongous array is ONE stride: its every
    /// card is anchored at its start.
    #[test]
    fn the_object_oracle_matches_the_walk_on_a_humongous_layout() {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let mut og = OldGen::new(16 * 1024 * 1024);
        og.set_block_offset_table_enabled(true);
        let mut live: Vec<(usize, usize)> = Vec::new();
        let humongous = |og: &mut OldGen, len: u32, top: bool| -> usize {
            let size = ref_array_size(len);
            let p = if top {
                og.alloc_from_top(size, 8, true)
            } else {
                og.alloc(size, 8)
            }
            .expect("room for a humongous array");
            // SAFETY: a fresh, zeroed block of `size` bytes.
            unsafe {
                std::ptr::write(
                    p as *mut ObjectHeader,
                    ObjectHeader::new(ClassId::new(2), ObjectKind::Array, ArrayElementType::Reference, len, len),
                )
            };
            p as usize
        };
        for _ in 0..50 {
            if let Some(o) = alloc_one(&mut og, &mut rng) {
                live.push(o);
            }
        }
        let low = humongous(&mut og, 400_000, false);
        for _ in 0..8 {
            alloc_buffer(&mut og, &mut rng, &mut live);
        }
        let high = humongous(&mut og, 300_000, true);
        for _ in 0..50 {
            if let Some(o) = alloc_one(&mut og, &mut rng) {
                live.push(o);
            }
        }
        // Frees on both sides of the arrays.
        for _ in 0..30 {
            let (off, size) = live.swap_remove(rng.below(live.len()));
            // SAFETY: a live object, freed once.
            unsafe { og.free((og.base_ptr() as usize + off) as *mut u8, size) };
        }
        assert_oracle_matches_walk(&og, &mut rng, 2000, "humongous");
        for (h, len) in [(low, 400_000u32), (high, 300_000u32)] {
            let size = ref_array_size(len);
            let before = og.block_offset_stats().oracle_strides;
            let deep = h + size - 3 * BOT_CARD - 5;
            assert_eq!(og.object_at(deep), ObjectAt::Interior { base: h, size });
            assert_eq!(
                og.block_offset_stats().oracle_strides - before,
                1,
                "one stride from the array's own anchor",
            );
        }
        // Freeing the low array leaves a multi-megabyte hole the oracle
        // reports as free, and the high one is still answered exactly.
        // SAFETY: the array was allocated above, freed once.
        unsafe { og.free(low as *mut u8, ref_array_size(400_000)) };
        assert_eq!(og.object_at(low + 1024 * 1024), ObjectAt::Free);
        assert_oracle_matches_walk(&og, &mut rng, 2000, "humongous after the low array died");
        assert_eq!(og.block_offset_stats().oracle_unknown, 0);
    }

    /// PINNED layouts: after `compact_around_pins` slides the unpinned
    /// survivors down around pinned islands (the table is discarded by the
    /// relayout), the oracle re-derives and matches the walk again — gaps in
    /// front of pins, pins at the end, dead objects dropped.
    #[test]
    fn the_object_oracle_matches_the_walk_after_a_pinned_compaction() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut og = OldGen::new(1024 * 1024);
        og.set_block_offset_table_enabled(true);
        let mut objs: Vec<usize> = Vec::new();
        objs.extend(carve_buffer(&mut og, 40, 2));
        for _ in 0..20 {
            objs.push(own_block(&mut og, 3));
        }
        objs.extend(carve_buffer(&mut og, 40, 1));
        let base = og.base_ptr() as usize;
        assert_oracle_matches_walk(&og, &mut rng, 500, "before");
        // Two in three survive; two pins, one of them on a dead object (the
        // pin retains it), one near the top.
        for (i, &o) in objs.iter().enumerate() {
            if i % 3 != 0 {
                // SAFETY: a live header of this generation.
                unsafe { &*((base + o) as *const ObjectHeader) }.add_gc_flags(crate::heap::GC_FLAG_MARKED);
            }
        }
        let pins = vec![base + objs[30], base + objs[95]];
        let walked = og.walk_objects();
        let seq = og.free_list_seq();
        let done = og
            .compact_around_pins(&walked, seq, pins, (0, 0), &std::collections::HashMap::new())
            .expect("a sane heap compacts");
        assert_eq!(done.pinned, 2);
        assert!(done.moved > 0);
        assert_oracle_matches_walk(&og, &mut rng, 1000, "after the pinned compaction");
        // The pins kept their addresses and are still bases.
        assert!(matches!(og.object_at(base + objs[30]), ObjectAt::Base { .. }));
        assert!(matches!(og.object_at(base + objs[95]), ObjectAt::Base { .. }));
        assert_eq!(og.block_offset_stats().oracle_unknown, 0);
    }

    /// A header no walk can size is a walk gap: the oracle refuses to answer
    /// behind it (never a guess), while an object whose card is anchored past
    /// the break is still answered exactly.
    #[test]
    fn the_object_oracle_refuses_a_walk_gap_and_an_overlapping_free_list() {
        let mut og = OldGen::new(64 * 1024);
        og.set_block_offset_table_enabled(true);
        let first = carve_buffer(&mut og, 26, 4);
        let second = carve_buffer(&mut og, 26, 4);
        break_header(&og, first[1]);
        let base = og.base_ptr() as usize;
        assert_eq!(og.object_at(base + first[0]), ObjectAt::Base { size: object_size(4) });
        assert_eq!(og.object_at(base + first[2]), ObjectAt::Unknown, "behind the break");
        let last = second[25];
        assert!(last / BOT_CARD * BOT_CARD >= second[0], "the card is the second buffer's own");
        assert_eq!(og.object_at(base + last), ObjectAt::Base { size: object_size(4) });
        assert!(og.block_offset_stats().oracle_unknown >= 1);

        // A double free: "which free block covers p" is ill-defined.
        let mut og = OldGen::new(64 * 1024);
        og.set_block_offset_table_enabled(true);
        let objs = carve_buffer(&mut og, 8, 2);
        let base = og.base_ptr() as usize;
        // SAFETY: the modelled defect — a span freed, then a piece of it again.
        unsafe {
            og.free((base + objs[2]) as *mut u8, object_size(2) * 4);
            og.free((base + objs[3]) as *mut u8, 24);
        }
        assert_eq!(og.object_at(base + objs[7]), ObjectAt::Unknown);

        // A generation without the table answers nothing.
        let mut og = OldGen::new(64 * 1024);
        og.set_block_offset_table_enabled(false);
        let objs = carve_buffer(&mut og, 4, 1);
        assert_eq!(og.object_at(og.base_ptr() as usize + objs[1]), ObjectAt::Unknown);
    }

    /// The long stride is a promotion buffer's (every card anchored at the
    /// buffer's start). The oracle pays it once: the cards it passes are
    /// re-anchored exactly, so asking again costs one card's objects. A
    /// rebuild from the kept objects makes every card exact at once.
    #[test]
    fn the_oracle_compresses_a_buffer_once_and_a_rebuild_makes_every_card_exact() {
        let mut og = OldGen::new(256 * 1024);
        og.set_block_offset_table_enabled(true);
        let objs = carve_buffer(&mut og, 400, 1);
        let base = og.base_ptr() as usize;
        let per_card = BOT_CARD / object_size(1) + 2;
        let last = base + objs[399];
        let s0 = og.block_offset_stats().oracle_strides;
        assert!(matches!(og.object_at(last), ObjectAt::Base { .. }));
        let first_cost = og.block_offset_stats().oracle_strides - s0;
        assert!(first_cost >= 390, "from the buffer's start: {first_cost}");
        let s1 = og.block_offset_stats().oracle_strides;
        assert!(matches!(og.object_at(last), ObjectAt::Base { .. }));
        let second_cost = (og.block_offset_stats().oracle_strides - s1) as usize;
        assert!(second_cost <= per_card, "compressed: {second_cost} > {per_card}");
        assert!(og.block_offset_stats().oracle_compressed > 0);

        // Free every other object, then rebuild from the kept ones: every
        // card is trusted and exact, and the answers match the walk.
        let mut kept = Vec::new();
        for (i, &o) in objs.iter().enumerate() {
            if i % 2 == 0 {
                // SAFETY: a live object, freed once.
                unsafe { og.free((base + o) as *mut u8, object_size(1)) };
            } else {
                kept.push((o, object_size(1)));
            }
        }
        og.rebuild_block_offsets_from_live(&kept);
        let st = og.block_offset_stats();
        assert_eq!(st.rebuilds, 1);
        assert!(st.valid_cards >= og.capacity() / BOT_CARD, "{st:?}");
        let s2 = st.oracle_strides;
        assert_eq!(
            og.object_at(base + objs[399] + 8),
            ObjectAt::Interior { base: base + objs[399], size: object_size(1) },
        );
        assert!((og.block_offset_stats().oracle_strides - s2) as usize <= per_card);
        let mut rng = Rng(7);
        assert_oracle_matches_walk(&og, &mut rng, 500, "after the rebuild");
        assert_eq!(og.block_offset_stats().rederived_objects, 0, "nothing was re-derived");
    }
}
