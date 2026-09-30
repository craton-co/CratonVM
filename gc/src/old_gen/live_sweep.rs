// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gen r5w5/old9 (2026-09-27) — the O(live) in-place sweep.
//!
//! `docs/internal/gc/gengc-r5w3-oldgen7-proposal-bot-object-base-oracle-for-an-o-live-sweep-DONE-20260928.md`,
//! step 4, and the O(live) half of
//! `gengc-r4w4-oldgen4-proposal-parallel-compaction-and-an-o-live-sweep`.
//!
//! # What was O(allocated)
//!
//! Every stop-the-world old-gen collection began with
//! [`OldGen::walk_objects_with_gaps`] — a header-by-header stride over every
//! ALLOCATED object, dead ones included, into a `Vec` — because that grid was
//! the mark's admission oracle ("is this address an object base?") and the
//! sweep's list of candidates. Then [`OldGen::close_live_set`] passed over the
//! whole grid again, and the free loop a third time. On a large old generation
//! whose live set is small, all three are proportional to what DIED.
//!
//! # What replaces it (opt-in, `CRATONVM_GC_OLD_LIVE_SWEEP`)
//!
//! * the admission oracle is [`OldGen::object_at`] — the block-offset table,
//!   a bounded stride from a card's anchor (`bot.rs`);
//! * the collector records every object it marks (the mark's own
//!   bookkeeping, `gen_heap_live_sweep.rs`), so the KEPT set is known without
//!   a walk;
//! * [`OldGen::close_live_set_by_oracle`] closes that set over references
//!   from kept objects (the GCAUD-8 guard `close_live_set` provides), visiting
//!   only kept objects;
//! * [`OldGen::sweep_dead_runs_around`] frees, in each allocated region, the
//!   spans BETWEEN consecutive kept objects — each dead run as one block,
//!   without decoding a single dead header — and re-anchors the table from
//!   the kept objects so the next collection's queries are exact.
//!
//! Cost: O(kept objects + their references + free blocks), plus the young
//! generation's cross-references (unchanged). The per-dead-object
//! instruments of the walked sweep (the reclamation ring, the A2 breadcrumb,
//! the overlay-owner reporter) have no dead object to report; the collector
//! does not take this path while any of them is switched on.

use super::bot::{free_list_is_disjoint, ObjectAt};
use super::{FreeBlock, OldGen};
use crate::heap::{ObjectHeader, GC_FLAG_MARKED, HEADER_SIZE};

/// What a completed [`OldGen::sweep_dead_runs_around`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LiveSweepOutcome {
    /// Objects kept, and their bytes (`used()` afterwards).
    pub kept_objects: usize,
    pub kept_bytes: usize,
    /// Dead runs freed (one free block each, before coalescing), and bytes.
    pub dead_runs: usize,
    pub freed_bytes: usize,
}

/// Why [`OldGen::sweep_dead_runs_around`] refused. On every refusal NOTHING
/// changed: the free list, `used()`, the block-offset table and every header
/// are as the caller handed them over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveSweepRefusal {
    /// The kept list is not ascending, disjoint, 8-aligned and inside the
    /// storage, or an entry is smaller than a header.
    BadLiveGrid,
    /// The free list overlaps itself (a double free): its complement is not
    /// a well-defined set of allocated regions.
    OverlappingFreeList,
    /// A kept object does not lie wholly inside one allocated region.
    LiveOutsideAllocated,
    /// The allocated regions do not add up to `used()`, or a dead run is not
    /// a legal free block (shorter than a header, unaligned). Either says the
    /// allocated space holds bytes that are neither a kept object nor a
    /// whole dead object — the walked sweep's GCAUD-9 test, answered here
    /// without the walk.
    AccountingMismatch,
}

impl LiveSweepRefusal {
    /// A short name for a log line.
    pub fn label(self) -> &'static str {
        match self {
            Self::BadLiveGrid => "the kept list is not an ascending grid of objects",
            Self::OverlappingFreeList => "the free list overlaps itself",
            Self::LiveOutsideAllocated => "a kept object lies outside the allocated regions",
            Self::AccountingMismatch => "the allocated regions do not add up",
        }
    }
}

/// What [`OldGen::close_live_set_by_oracle`] found.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LiveClosure {
    /// Unmarked objects a kept object references, marked and appended to the
    /// kept list (the walked sweep's `close_live_set` "rescues").
    pub promoted: usize,
    /// References from kept objects to an in-generation address that is not
    /// an object base (inside an object, or in a free block): the walked
    /// sweep's `escaped`. Reported, not repaired — an earlier reclamation
    /// already freed what they name.
    pub escaped: usize,
    /// References the oracle could not answer ([`ObjectAt::Unknown`]). Any
    /// non-zero value means the caller must not sweep from this marked set.
    pub unknown: usize,
}

impl OldGen {
    /// Close the kept set over references from kept objects, using the
    /// block-offset oracle as the admission proof: every unmarked object a
    /// kept object references is MARKED and appended to `live` (and traced in
    /// turn). `live` holds absolute `(base, size)` pairs of MARKED objects,
    /// in any order; each is scanned once, capped by its `size` exactly as
    /// the walked closure caps by the walked size.
    ///
    /// The walked sweep's `close_live_set` is this with the grid as oracle and
    /// a linear pass over every allocated object; here only kept objects are
    /// visited. Must run under the old-gen lock, before any free.
    pub fn close_live_set_by_oracle(&self, live: &mut Vec<(usize, usize)>) -> LiveClosure {
        let data = self.extent();
        let mut out = LiveClosure::default();
        let mut i = 0usize;
        while i < live.len() {
            let (obj, size) = live[i];
            i += 1;
            Self::for_each_old_gen_ref(obj as *mut u8, size, data, |r| match self.object_at(r) {
                ObjectAt::Base { size: rs } => {
                    // SAFETY: the oracle resolved `r` to an object base of this
                    // generation, under the lock the caller holds; nothing has
                    // been freed or moved.
                    let h = unsafe { &*(r as *const ObjectHeader) };
                    if h.gc_flags() & GC_FLAG_MARKED == 0 {
                        h.add_gc_flags(GC_FLAG_MARKED);
                        live.push((r, rs));
                        out.promoted += 1;
                    }
                }
                ObjectAt::Interior { .. } | ObjectAt::Free => out.escaped += 1,
                ObjectAt::Unknown => out.unknown += 1,
            });
        }
        out
    }

    /// Free every allocated byte that is not inside a kept object, one block
    /// per dead RUN, and re-anchor the block-offset table from the kept
    /// objects.
    ///
    /// `live`: absolute `(base, size)` of every object this collection keeps,
    /// ascending and disjoint (the collector's marked set after
    /// [`Self::close_live_set_by_oracle`]). The allocated regions are the
    /// complement of the (disjoint) free list; inside each, the dead runs are
    /// the spans in front of, between and behind the kept objects. Nothing
    /// between two kept objects is decoded: a run of dead objects is one
    /// `free`, which is what the walked sweep's batched free loop produces too
    /// (`freeing_a_contiguous_dead_run_as_one_block_equals_freeing_each_then_coalescing`).
    ///
    /// Validated before anything changes (see [`LiveSweepRefusal`]); the
    /// caller coalesces afterwards exactly as after the walked sweep. Mark
    /// bits are the caller's to clear.
    pub fn sweep_dead_runs_around(
        &mut self,
        live: &[(usize, usize)],
    ) -> Result<LiveSweepOutcome, LiveSweepRefusal> {
        let base = self.data.as_ptr() as usize;
        let len = self.data.len();
        let mut kept: Vec<(usize, usize)> = Vec::with_capacity(live.len());
        let mut prev_end = 0usize;
        for &(addr, size) in live {
            let Some(o) = addr.checked_sub(base) else {
                return Err(LiveSweepRefusal::BadLiveGrid);
            };
            if o % 8 != 0
                || size < HEADER_SIZE
                || size % 8 != 0
                || o < prev_end
                || o.checked_add(size).is_none_or(|e| e > len)
            {
                return Err(LiveSweepRefusal::BadLiveGrid);
            }
            prev_end = o + size;
            kept.push((o, size));
        }
        let sorted: Vec<FreeBlock> = self.with_sorted_free_blocks(|s| s.to_vec());
        if !free_list_is_disjoint(&sorted, len) {
            return Err(LiveSweepRefusal::OverlappingFreeList);
        }
        let mut dead: Vec<(usize, usize)> = Vec::new();
        let mut li = 0usize;
        let mut cursor = 0usize;
        let mut allocated = 0usize;
        for b in &sorted {
            if b.offset > cursor {
                allocated += b.offset - cursor;
                dead_runs_in_region(&kept, &mut li, cursor, b.offset, &mut dead)?;
            }
            cursor = b.offset + b.size;
        }
        if cursor < len {
            allocated += len - cursor;
            dead_runs_in_region(&kept, &mut li, cursor, len, &mut dead)?;
        }
        if li != kept.len() {
            return Err(LiveSweepRefusal::LiveOutsideAllocated);
        }
        if allocated != self.used_bytes
            || dead
                .iter()
                .any(|&(o, s)| s < HEADER_SIZE || s % 8 != 0 || o % 8 != 0)
        {
            return Err(LiveSweepRefusal::AccountingMismatch);
        }

        // Committed from here on: every refusal is above.
        let mut freed = 0usize;
        for &(o, s) in &dead {
            // SAFETY: `[o, o + s)` lies inside an allocated region of this
            // generation (the complement of its disjoint free list) and holds
            // no kept object; every byte of it belongs to objects the
            // collection proved unreachable (or, before any object, to
            // nothing), so it is exactly a union of allocated extents, which
            // `free` returns as one block. `s` is 8-aligned and at least a
            // header, so `free`'s rounding does not widen it.
            unsafe { self.free((base + o) as *mut u8, s) };
            freed += s;
        }
        self.rebuild_block_offsets_from_live(&kept);
        let kept_bytes: usize = kept.iter().map(|&(_, s)| s).sum();
        debug_assert_eq!(
            self.used_bytes, kept_bytes,
            "live sweep: after freeing every dead run the kept objects are all that is allocated",
        );
        let outcome = LiveSweepOutcome {
            kept_objects: kept.len(),
            kept_bytes,
            dead_runs: dead.len(),
            freed_bytes: freed,
        };
        self.sizing.live_sweeps = self.sizing.live_sweeps.wrapping_add(1);
        self.sizing.live_sweep_dead_runs = self
            .sizing
            .live_sweep_dead_runs
            .wrapping_add(dead.len() as u64);
        self.sizing.live_sweep_freed_bytes = self
            .sizing
            .live_sweep_freed_bytes
            .wrapping_add(freed as u64);
        self.sizing.live_sweep_kept_last = kept.len() as u64;
        Ok(outcome)
    }

    /// Count a live-sweep attempt the collector abandoned to the walked path;
    /// returns the count before this one (for rate-limited logging).
    pub fn note_live_sweep_fallback(&mut self) -> u64 {
        let n = self.sizing.live_sweep_fallbacks;
        self.sizing.live_sweep_fallbacks = n.wrapping_add(1);
        n
    }

    /// DEBUG ONLY — make a WALK GAP on purpose
    /// (`CRATONVM_DBG_OLD_PLANT_WALK_BREAK`, see
    /// `GenerationalHeap::after_old_gen_collection`): allocate one small block
    /// nobody references and give it a header no walk can size (an invalid
    /// kind tag), so every later walk of its allocated region stops there and
    /// reports the rest of the region as a gap — the heap anomaly
    /// (`SCAN_REGION_BREAK_HITS` / `WALK_DESYNC_HITS`) the walk-gap
    /// recoveries exist for, which nothing else can produce on demand
    /// (`gengc-r4w5-oldcompact5-concurrent-remark-refuses-its-sweep-over-a-walk-gap`).
    ///
    /// Harmless to live data by construction: the block is fresh, zeroed and
    /// unreferenced, so no object and no reference is behind its header; the
    /// LIVE objects the gap hides are never freed (the walked sweep's gap
    /// recovery keeps gap bytes, the live sweep abandons to it whenever a
    /// query strides across the break, the compactors refuse a walk that does
    /// not cover `used()`). No WALKED collection ever frees the block (no walk can size
    /// it); the O(live) sweep frees it with its dead run — correctly, it is
    /// unreferenced — unless a query strides across it first (then that
    /// collection falls back to the walked one). Planted at most once per
    /// generation either way. Returns the block's address, or `None` (already
    /// planted, or no room).
    pub fn plant_walk_break(&mut self) -> Option<usize> {
        if self.sizing.walk_breaks_planted > 0 {
            return None;
        }
        let p = self.alloc(64, 8)?;
        // SAFETY: `p` is a fresh, zeroed, 8-aligned block of 64 bytes owned
        // by nobody; the kind-tag byte lies inside its first header.
        unsafe { std::ptr::write(p.add(cratonvm_types::KIND_TAGS_BYTE_OFFSET), 0xFFu8) };
        self.sizing.walk_breaks_planted = 1;
        Some(p as usize)
    }
}

/// The dead runs of the allocated region `[start, end)` (offsets) around the
/// kept objects `kept[*li..]` that start inside it; advances `*li` past them.
fn dead_runs_in_region(
    kept: &[(usize, usize)],
    li: &mut usize,
    start: usize,
    end: usize,
    dead: &mut Vec<(usize, usize)>,
) -> Result<(), LiveSweepRefusal> {
    let mut pos = start;
    while *li < kept.len() && kept[*li].0 < end {
        let (o, s) = kept[*li];
        if o < start || o + s > end {
            return Err(LiveSweepRefusal::LiveOutsideAllocated);
        }
        if o > pos {
            dead.push((pos, o - pos));
        }
        pos = o + s;
        *li += 1;
    }
    if end > pos {
        dead.push((pos, end - pos));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::{ArrayElementType, ObjectKind, SLOT_SIZE};
    use cratonvm_types::{ClassId, ObjectRef, Value};

    /// Deterministic xorshift, so two generations built from the same seed
    /// are laid out identically.
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

    fn obj_size(slots: usize) -> usize {
        HEADER_SIZE + SLOT_SIZE * slots
    }

    /// A legacy object of `slots` slots on a block of its own (offset).
    fn own(og: &mut OldGen, slots: u32) -> Option<usize> {
        let p = og.alloc(obj_size(slots as usize), 8)?;
        // SAFETY: a fresh, zeroed block sized for the header and its slots.
        unsafe { (*(p as *mut ObjectHeader)).set_num_slots(slots) };
        Some(p as usize - og.base_ptr() as usize)
    }

    /// `n` legacy objects carved end to end from one promotion-buffer-shaped
    /// block, consumed to the byte (offsets).
    fn carve(og: &mut OldGen, n: usize, slots: u32) -> Option<Vec<usize>> {
        let size = obj_size(slots as usize);
        let p = og.alloc_unzeroed_buffer(size * n, 8)?;
        let base = og.base_ptr() as usize;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            // SAFETY: `[p + i*size, p + (i+1)*size)` lies inside the buffer.
            unsafe {
                let q = p.add(i * size);
                std::ptr::write_bytes(q, 0, size);
                (*(q as *mut ObjectHeader)).set_num_slots(slots);
            }
            out.push(p as usize - base + i * size);
        }
        Some(out)
    }

    /// An `int[len]` on a block of its own (offset).
    fn int_array(og: &mut OldGen, len: u32) -> Option<usize> {
        let size = (crate::heap::ARRAY_DATA_OFFSET + len as usize * 4 + 7) & !7;
        let p = og.alloc(size, 8)?;
        // SAFETY: a fresh, zeroed block sized for the array.
        unsafe {
            std::ptr::write(
                p as *mut ObjectHeader,
                ObjectHeader::new(ClassId::new(1), ObjectKind::Array, ArrayElementType::Int, len, len),
            )
        };
        Some(p as usize - og.base_ptr() as usize)
    }

    fn set_ref(og: &OldGen, holder: usize, slot: usize, target: usize) {
        let base = og.base_ptr() as usize;
        // SAFETY: test holders are legacy objects with more than `slot` slots;
        // the target is a non-null, 8-aligned object base.
        unsafe {
            std::ptr::write(
                (base + holder + HEADER_SIZE + slot * SLOT_SIZE) as *mut Value,
                Value::Object(Some(ObjectRef::from_raw((base + target) as *mut u8))),
            );
        }
    }

    /// A fragmented generation from `seed`: objects on their own blocks,
    /// promotion buffers, big arrays, and frees at random. Returns the
    /// offsets of the objects still allocated, ascending.
    fn build(seed: u64) -> (OldGen, Vec<usize>) {
        let mut rng = Rng(0x243F_6A88_85A3_08D3 ^ (seed * 0x9E37_79B9));
        let mut og = OldGen::new(8 * 1024 * 1024);
        og.set_block_offset_table_enabled(true);
        let mut objs: Vec<usize> = Vec::new();
        for _ in 0..600 {
            match rng.below(8) {
                0..=3 => objs.extend(own(&mut og, rng.below(6) as u32)),
                4 | 5 => {
                    let n = 1 + rng.below(60);
                    if let Some(v) = carve(&mut og, n, rng.below(4) as u32) {
                        objs.extend(v);
                    }
                }
                6 => {
                    let len = if rng.below(10) == 0 {
                        40_000
                    } else {
                        rng.below(500) as u32
                    };
                    objs.extend(int_array(&mut og, len));
                }
                _ => {
                    if !objs.is_empty() {
                        let o = objs.swap_remove(rng.below(objs.len()));
                        let p = (og.base_ptr() as usize + o) as *mut u8;
                        // SAFETY: an object of this generation, freed once;
                        // its size is read from its own intact header.
                        let size = og
                            .walk_objects()
                            .into_iter()
                            .find(|&(q, _)| q == p)
                            .map(|(_, s)| s)
                            .expect("a walked object");
                        unsafe { og.free(p, size) };
                    }
                }
            }
        }
        objs.sort_unstable();
        (og, objs)
    }

    /// Sorted free blocks as `(offset, size)`.
    fn free_view(og: &OldGen) -> Vec<(usize, usize)> {
        og.with_sorted_free_blocks(|s| s.iter().map(|b| (b.offset, b.size)).collect())
    }

    /// THE equivalence: over fragmented generations, keeping a random subset
    /// and sweeping with the live sweep leaves EXACTLY the generation the
    /// walked sweep (free every dead walked object, then coalesce) leaves —
    /// same `used()`, same free list, same object grid — and the table the
    /// rebuild leaves answers every query as the walk does.
    #[test]
    fn a_live_sweep_leaves_exactly_what_the_walked_sweep_leaves() {
        for seed in 1..=8u64 {
            let (mut walked_og, objs) = build(seed);
            let (mut live_og, objs2) = build(seed);
            assert_eq!(objs, objs2, "seed {seed}: identical layouts");
            let mut rng = Rng(seed);
            let keep: Vec<bool> = objs.iter().map(|_| rng.below(4) == 0).collect();

            // The walked sweep, as `old_gen_gc_inner`'s free loop does it.
            let wbase = walked_og.base_ptr() as usize;
            for (p, size) in walked_og.walk_objects() {
                let o = p as usize - wbase;
                let i = objs.binary_search(&o).expect("every walked object is one of ours");
                if !keep[i] {
                    // SAFETY: a walked object, freed once.
                    unsafe { walked_og.free(p, size) };
                }
            }
            walked_og.coalesce_free_blocks();

            // The live sweep.
            let lbase = live_og.base_ptr() as usize;
            let sizes: std::collections::HashMap<usize, usize> = live_og
                .walk_objects()
                .into_iter()
                .map(|(p, s)| (p as usize - lbase, s))
                .collect();
            let live: Vec<(usize, usize)> = objs
                .iter()
                .zip(&keep)
                .filter(|&(_, &k)| k)
                .map(|(&o, _)| (lbase + o, sizes[&o]))
                .collect();
            let out = live_og.sweep_dead_runs_around(&live).expect("a sane heap sweeps");
            live_og.coalesce_free_blocks();

            assert_eq!(out.kept_objects, live.len(), "seed {seed}");
            assert_eq!(live_og.used(), walked_og.used(), "seed {seed}: used");
            assert_eq!(free_view(&live_og), free_view(&walked_og), "seed {seed}: free list");
            let grid = |og: &OldGen| -> Vec<(usize, usize)> {
                let b = og.base_ptr() as usize;
                og.walk_objects().into_iter().map(|(p, s)| (p as usize - b, s)).collect()
            };
            assert_eq!(grid(&live_og), grid(&walked_og), "seed {seed}: object grid");
            // The rebuilt table: every kept object and a byte inside it.
            for &(p, s) in &live {
                assert_eq!(live_og.object_at(p), ObjectAt::Base { size: s }, "seed {seed}");
                assert_eq!(
                    live_og.object_at(p + s - 1),
                    ObjectAt::Interior { base: p, size: s },
                    "seed {seed}"
                );
            }
            let st = live_og.block_offset_stats();
            assert_eq!(st.rebuilds, 1, "seed {seed}: {st:?}");
            assert_eq!(st.rederived_objects, 0, "seed {seed}: the rebuild left nothing to re-derive");
            assert_eq!(st.oracle_unknown, 0, "seed {seed}: {st:?}");
            assert_eq!(live_og.sizing_stats().live_sweeps, 1);
        }
    }

    /// Every refusal changes nothing.
    #[test]
    fn a_live_sweep_refuses_an_inconsistent_grid_and_changes_nothing() {
        let (mut og, objs) = build(3);
        let base = og.base_ptr() as usize;
        let sizes: std::collections::HashMap<usize, usize> = og
            .walk_objects()
            .into_iter()
            .map(|(p, s)| (p as usize - base, s))
            .collect();
        let used = og.used();
        let fl = free_view(&og);
        let a = (base + objs[1], sizes[&objs[1]]);
        let b = (base + objs[5], sizes[&objs[5]]);
        // Not ascending.
        assert_eq!(og.sweep_dead_runs_around(&[b, a]), Err(LiveSweepRefusal::BadLiveGrid));
        // Unaligned, and below the storage.
        assert_eq!(og.sweep_dead_runs_around(&[(a.0 + 4, a.1)]), Err(LiveSweepRefusal::BadLiveGrid));
        assert_eq!(og.sweep_dead_runs_around(&[(8, 32)]), Err(LiveSweepRefusal::BadLiveGrid));
        // A "kept object" inside a free block.
        let (free_off, free_size) = *fl.iter().find(|&&(_, s)| s >= 64).expect("a free block");
        assert!(free_size >= 64);
        assert_eq!(
            og.sweep_dead_runs_around(&[(base + free_off, 32)]),
            Err(LiveSweepRefusal::LiveOutsideAllocated),
        );
        // A kept extent that runs past its region's end.
        let (last_off, last_size) = og
            .walk_objects()
            .into_iter()
            .map(|(p, s)| (p as usize - base, s))
            .filter(|&(o, s)| fl.iter().any(|&(f, _)| f == o + s))
            .last()
            .expect("an object followed by a free block");
        assert_eq!(
            og.sweep_dead_runs_around(&[(base + last_off, last_size + 8)]),
            Err(LiveSweepRefusal::LiveOutsideAllocated),
        );
        assert_eq!(og.used(), used);
        assert_eq!(free_view(&og), fl);
        assert_eq!(og.sizing_stats().live_sweeps, 0);
    }

    /// The debug plant makes exactly one walk gap: the walk stops at the
    /// planted header, the gap accounts for the rest of the region, the
    /// oracle refuses behind it, and a second plant is refused.
    #[test]
    fn a_planted_walk_break_is_one_gap_and_is_planted_once() {
        let mut og = OldGen::new(64 * 1024);
        og.set_block_offset_table_enabled(true);
        let before = own(&mut og, 2).unwrap();
        let at = og.plant_walk_break().expect("room for the plant");
        let after = own(&mut og, 2).unwrap();
        let base = og.base_ptr() as usize;
        assert_eq!(at - base, before + obj_size(2), "best fit: right behind `before`");
        let (walked, gaps) = og.walk_objects_with_gaps();
        let walked: Vec<usize> = walked.iter().map(|&(p, _)| p as usize - base).collect();
        assert_eq!(walked, vec![before], "the walk stops at the planted header");
        assert_eq!(gaps, vec![(at - base, after + obj_size(2))], "one gap to the region's end");
        assert_eq!(og.object_at(base + after), ObjectAt::Unknown, "behind the break");
        assert!(matches!(og.object_at(base + before), ObjectAt::Base { .. }));
        assert_eq!(og.plant_walk_break(), None, "once per generation");
        assert_eq!(og.sizing_stats().walk_breaks_planted, 1);
    }

    /// The oracle closure marks and appends what a kept object references,
    /// transitively, reports a reference into a free block as an escape, and
    /// visits nothing else.
    #[test]
    fn the_oracle_closure_promotes_what_kept_objects_reference() {
        let mut og = OldGen::new(64 * 1024);
        og.set_block_offset_table_enabled(true);
        let a = own(&mut og, 2).unwrap();
        let b = own(&mut og, 2).unwrap();
        let c = own(&mut og, 2).unwrap();
        let dead = own(&mut og, 2).unwrap();
        let gone = own(&mut og, 1).unwrap();
        let base = og.base_ptr() as usize;
        set_ref(&og, a, 0, b);
        set_ref(&og, b, 1, c);
        set_ref(&og, c, 0, a);
        set_ref(&og, dead, 0, c);
        set_ref(&og, c, 1, gone);
        // `gone` is freed while `c` still names it: an escape.
        // SAFETY: an object of this generation, freed once.
        unsafe { og.free((base + gone) as *mut u8, obj_size(1)) };
        // SAFETY: a live header.
        unsafe { &*((base + a) as *const ObjectHeader) }.add_gc_flags(GC_FLAG_MARKED);
        let mut live = vec![(base + a, obj_size(2))];
        let out = og.close_live_set_by_oracle(&mut live);
        assert_eq!(out, LiveClosure { promoted: 2, escaped: 1, unknown: 0 });
        let mut got: Vec<usize> = live.iter().map(|&(p, _)| p - base).collect();
        got.sort_unstable();
        assert_eq!(got, vec![a, b, c], "`dead` references c but nothing kept references it");
        // SAFETY: a header of this generation.
        let dead_marked = unsafe { &*((base + dead) as *const ObjectHeader) }.gc_flags() & GC_FLAG_MARKED;
        assert_eq!(dead_marked, 0);
    }
}
