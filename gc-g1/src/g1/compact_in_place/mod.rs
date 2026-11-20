// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! G1's IN-PLACE full compaction: HotSpot's `G1FullCollector` shape (mark,
//! plan, adjust, compact) over the movable Old regions, inside one pause, with
//! no Free region needed as to-space.
//!
//! # Why
//!
//! Every other compaction this collector has is a COPY into Free regions: the
//! full-compaction pause (`arm_full_compaction`) is a mixed pause whose old
//! half is bounded by the Free pool (`full_compaction_candidates`' `room`).
//! On a heap whose live set leaves a few regions free and whose Old regions
//! hold garbage beside live data, that copy cannot run: measured on
//! `Zs3Nd3UpcallMints proxy 4096 -XX:+UseG1GC -Xmx64m` (the G1 near-full
//! proxy-define page), the Free pool sat at 0-3 regions, every compaction pause
//! copied the same 52 KB back and forth, and the run took 7507 pauses for
//! 20000 iterations with 853 refused allocations. HotSpot meets that heap
//! with 386 `Pause Full (G1 Compaction Pause)` of 62M->59M each, and refuses
//! none: its full collection slides every live object down in place.
//!
//! # What it does
//!
//! Called from `collect_garbage` right after an armed full-compaction pause
//! when the Free pool is still starved (`in_place_compaction_starved`), so the
//! young regions are already evacuated, kept as Old, or left out of that
//! pause's collection set (a JIT-pinned or unretired-TLAB Eden region, which
//! is then an ordinary source here, walked as Phase 4 walks it). With the
//! regions WRITE guard held for the whole pause:
//!
//! 1. **Parse.** The compaction set `C` is every Old (and Survivor) region
//!    that is not JNI-pinned, not JIT-pinned and holds no frozen peer's TLAB
//!    span; each is walked from its base to its cursor (the Phase-4 walk's
//!    rules: gap fillers skipped, a size that leaves the cursor refuses the
//!    region). A compacted Survivor region is Old afterwards. A JIT-pinned
//!    Old, Survivor or Eden region is HELD in `C`: a conservative JIT word
//!    inside it, or a root an evacuation pause would pin for, keeps the object
//!    containing it (and the one it ends) where it is; its other live objects
//!    pack down inside the region (`CRATONVM_G1_IN_PLACE_HELD_OBJECTS_MOVE`;
//!    with it off, none of its objects moves), objects from above may slide
//!    into its dead space, the rest of that space becomes fillers, and it is
//!    Old afterwards.
//! 2. **Mark.** An object of `C` is live when the roots, the pending
//!    finalizers, the overlay rows, or any object OUTSIDE `C` (dead or alive,
//!    the rule an evacuation pause applies to its remembered-set sources)
//!    reach it, transitively through `C`. Every non-`C` region is walked the
//!    way Phase 4 walks it. A reference into `C` that is not an object start
//!    found in step 1 ABORTS the compaction before anything moved.
//! 3. **Plan.** In region order, every live object is given the next address
//!    of the sliding destination (which never passes its source), and a
//!    forward `old -> new`, identity included, so every live `C` object is a
//!    key of the pointer map exactly as every survivor of an evacuation is.
//! 4. **Adjust.** The roots slice, then every non-`C` region through the
//!    Phase-4 walk (`update_references_in_regions`, which also rebuilds their
//!    outgoing remembered-set edges into the cleared `C` rsets).
//! 5. **Compact.** `memmove` in ascending source order; each `C` region takes
//!    its new fill as cursor (or is reset to Free), with no mark data, clean
//!    cards and a fresh recycle stamp.
//! 6. **Adjust `C`.** The compacted objects' own slots, through the same
//!    Phase-4 region walk, which re-derives their outgoing edges and cards.
//!
//! The pause's pointer map then goes through every remap the evacuation
//! pauses run (monitors, the referent skip set, the overlay snapshot, the
//! string dedup table, the gray set) and back to the VM with the result.
//!
//! Every phase but the plan's arithmetic and the region bookkeeping runs on
//! the evacuation pool (`CRATONVM_G1_FULL_COMPACTION_IN_PLACE_PARALLEL`,
//! default ON): the parse and the adjust walks claim regions, the mark claims
//! a bit per object and shares work through one queue, and the plan splits the
//! set into contiguous chunks that each slide into their own regions
//! ([`plan_chunks`]; one chunk under 128 regions), so the moves of different
//! chunks never touch the same bytes.
//!
//! `CRATONVM_G1_FULL_COMPACTION_IN_PLACE` (default ON, `=0` kills).

use super::*;

/// In-place compactions run, and the bytes they reclaimed (census).
static IN_PLACE_COMPACTIONS: AtomicU64 = AtomicU64::new(0);
/// g1sz: wall microseconds per phase of the completed and declined passes,
/// summed: parse, mark, plan, map, adjust outside, copy, adjust inside, rest.
static IN_PLACE_PHASE_US: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

/// g1sz: [`IN_PLACE_PHASE_US`] for the census line.
pub fn g1_in_place_phase_us() -> [u64; 8] {
    std::array::from_fn(|i| IN_PLACE_PHASE_US[i].load(Ordering::Relaxed))
}

/// g1sz: charge the time since `lap` to phase `i` and restart `lap`.
fn in_place_lap(lap: &mut std::time::Instant, i: usize) {
    let now = std::time::Instant::now();
    IN_PLACE_PHASE_US[i].fetch_add(now.duration_since(*lap).as_micros() as u64, Ordering::Relaxed);
    *lap = now;
}
static IN_PLACE_RECLAIMED_BYTES: AtomicU64 = AtomicU64::new(0);
static IN_PLACE_MOVED_BYTES: AtomicU64 = AtomicU64::new(0);
static IN_PLACE_FREED_REGIONS: AtomicU64 = AtomicU64::new(0);
/// Compactions declined (census), and per reason ([`DECLINE_REASONS`]).
static IN_PLACE_DECLINED: AtomicU64 = AtomicU64::new(0);
static IN_PLACE_LAST_DECLINE: std::sync::Mutex<&'static str> = std::sync::Mutex::new("");
const DECLINE_REASONS: [&str; 10] = [
    "pool-not-starved",
    "mark-cycle-open",
    "no-regions",
    "nothing-to-slide",
    "compaction-region-unparsable",
    "source-region-unparsable",
    "interior-reference-into-compaction-set",
    "plan-overflow",
    "plan-passed-source",
    "less-than-a-region-to-reclaim",
];
static IN_PLACE_DECLINED_BY: [AtomicU64; 10] = [const { AtomicU64::new(0) }; 10];
/// Regions a pass would have compacted but left out: JNI-pinned, JIT-pinned
/// (a conservative root names them; only under
/// `CRATONVM_G1_IN_PLACE_HOLD_PINNED=0`), holding a frozen peer's TLAB span;
/// and the JIT-pinned ones it held instead (census, summed over passes).
static IN_PLACE_EXCLUDED: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// `CRATONVM_G1_DBG_PINS=1`: the young regions the last pass left out, and
/// why (` idx:type/KB:reason` each), for its `[g1][IN-PLACE]` line.
static IN_PLACE_DBG_LEFT: parking_lot::Mutex<String> = parking_lot::Mutex::new(String::new());

/// `CRATONVM_G1_DBG_IN_PLACE_RETAIN=1`: when a pass declines on a heap with
/// at most two Free regions, what keeps its live set (diagnostic; at most 60
/// reports per process).
fn dbg_retain_wanted(regions: &[G1Region]) -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    static N: AtomicU64 = AtomicU64::new(0);
    *ON.get_or_init(|| false)
        && regions
            .iter()
            .filter(|r| r.region_type() == RegionType::Free)
            .count()
            <= 2
        && N.fetch_add(1, Ordering::Relaxed) < 60
}

/// Take what [`IN_PLACE_DBG_LEFT`] holds.
pub(super) fn take_in_place_dbg_left() -> String {
    std::mem::take(&mut *IN_PLACE_DBG_LEFT.lock())
}

/// `pinned=N jit_pinned=N tlab_span=N held=N` -- the regions passes left out,
/// and the JIT-pinned ones they held.
pub fn g1_in_place_compaction_exclusions() -> String {
    let n = |i: usize| IN_PLACE_EXCLUDED[i].load(Ordering::Relaxed);
    format!(
        "pinned={} jit_pinned={} tlab_span={} held={} lifted={} lifted_objects={}",
        n(0),
        n(1),
        n(2),
        n(3),
        IN_PLACE_LIFTED_REGIONS.load(Ordering::Relaxed),
        IN_PLACE_LIFTED_OBJECTS.load(Ordering::Relaxed)
    )
}

/// r23 w6 (`CRATONVM_G1_STARVED_FULL`): starved fulls run, and the regions
/// their in-place pass freed (census).
static STARVED_FULLS: AtomicU64 = AtomicU64::new(0);
static STARVED_FULL_FREED_REGIONS: AtomicU64 = AtomicU64::new(0);

/// Count one starved full (`G1Collector::note_starved_full_outcome`).
pub(super) fn note_starved_full(freed_regions: usize) {
    STARVED_FULLS.fetch_add(1, Ordering::Relaxed);
    STARVED_FULL_FREED_REGIONS.fetch_add(freed_regions as u64, Ordering::Relaxed);
}

/// `(starved fulls, regions they freed)` -- part of the
/// `[GC] g1 in-place-compaction:` census line.
pub fn g1_starved_full_census() -> (u64, u64) {
    (
        STARVED_FULLS.load(Ordering::Relaxed),
        STARVED_FULL_FREED_REGIONS.load(Ordering::Relaxed),
    )
}



/// The G1 near-full proxy row
/// (`g1-near-full-proxy-row-refuses-a-rare-allocation-when-a-pinned-eden-region-fills-the-heap-FIXED-20261026`):
/// `CRATONVM_G1_IN_PLACE_HELD_OBJECTS_MOVE` (default ON, `=0` kills; only with
/// the two switches above): in a held region only the objects a root or a
/// conservative JIT word names stay put, and its other live objects slide.
/// Holding the whole region left the frames' Eden regions mostly dead space
/// cut into holes too small for a 64 KiB chunk, so a heap whose live set
/// fitted in 60 of 64 regions freed none. Read once per pass.
fn g1_in_place_held_objects_move_enabled() -> bool {
    true
}

/// t1 W13 (`docs/feature-designs/jit-ir-optimizations.md`):
/// `CRATONVM_G1_IN_PLACE_PINS_FIX_ONLY_MARKED` (default ON since gkr, 2026-10-28,
/// `=0` kills; WARNING gkr-fix-only-marked-flip-20261028; only with
/// [`g1_in_place_held_objects_move_enabled`]). A conservative JIT word no
/// longer seeds the mark: an object it names is fixed only when the roots
/// marked it, so a stale word stops keeping a dead object -- and all it
/// reaches -- alive and in place. A held region left with no fixed object
/// and no root word in it is released: the pass declines before it changes
/// anything and runs once more with that region movable, so its live objects
/// pack down with the rest. After JIT churn, stale words held 3-5 Survivor
/// regions scattered over a 96 MiB heap that HotSpot compacts into one run.
fn g1_in_place_pins_fix_only_marked() -> bool {
    // WARNING gkr-fix-only-marked-flip-20261028: default ON, `=0` restores the seed.
    true
}

/// The first pass's decline under [`g1_in_place_pins_fix_only_marked`]: held
/// regions with nothing fixed were found; the caller retries with them movable.
const HELD_REGIONS_RELEASED: &str = "held-regions-released";

/// The G1 near-full proxy row: `CRATONVM_G1_IN_PLACE_LIFT_INTO_HELD_HOLES`
/// (default ON, `=0` kills; only with the two switches above): movable
/// regions, cheapest first, are emptied into the held regions' holes, which
/// may lie above them. Read once per pass.
fn g1_in_place_lift_enabled() -> bool {
    true
}

/// Regions the lift emptied into held regions' holes, and single objects it
/// lifted (census, summed).
static IN_PLACE_LIFTED_REGIONS: AtomicU64 = AtomicU64::new(0);
static IN_PLACE_LIFTED_OBJECTS: AtomicU64 = AtomicU64::new(0);

/// A held region's free space after the mark, ascending: each run of dead
/// parsed objects, and the tail from its last object to the region's end.
/// Pure over the parse and its live bits.
fn held_region_holes(r: &RegionObjs, region_size: usize) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut end = 0usize;
    for j in 0..r.offs.len() {
        let (o, n) = (r.offs[j] as usize, r.sizes[j] as usize);
        if !r.is_live(j) {
            match out.last_mut() {
                Some(last) if last.1 == o => last.1 = o + n,
                _ => out.push((o, o + n)),
            }
        }
        end = o + n;
    }
    if end < region_size {
        match out.last_mut() {
            Some(last) if last.1 == end => last.1 = region_size,
            _ => out.push((end, region_size)),
        }
    }
    out
}

/// `holes` (ascending, disjoint) with each one that strictly contains a
/// word offset in `pins` split there: an object placed in the result never
/// covers a pin word, it ends at one or starts at one. Pure; sorts `pins`.
fn split_holes_at(holes: Vec<(usize, usize)>, pins: &mut [usize]) -> Vec<(usize, usize)> {
    if pins.is_empty() {
        return holes;
    }
    pins.sort_unstable();
    let mut out = Vec::with_capacity(holes.len() + pins.len());
    for (mut s, e) in holes {
        let first = pins.partition_point(|&p| p <= s);
        for &p in pins[first..].iter().take_while(|&&p| p < e) {
            // Word offsets are 8-aligned; a hole boundary is too.
            let p = p & !7;
            if p > s {
                out.push((s, p));
                s = p;
            }
        }
        out.push((s, e));
    }
    out
}







/// The object table of a movable region from its granule marks (one bit per
/// 8 bytes from the region base, set at each live object's start): offsets
/// and sizes in address order. `Err` when a mark is not a sound object start:
/// it lies inside the previous live object, or its header's size is under a
/// header or runs past `cursor` -- a reference into the region that names no
/// object, which a parse would have refused as an interior reference.
///
/// # Safety
///
/// Every set bit must name an address below `base + cursor` of a region the
/// caller's write guard holds still.
unsafe fn live_table_from_marks(
    base: usize,
    cursor: usize,
    marks: &[AtomicU64],
) -> Result<(Vec<u32>, Vec<u32>), ()> {
    let mut offs = Vec::new();
    let mut sizes = Vec::new();
    let mut end = 0usize;
    for (w, word) in marks.iter().enumerate() {
        let mut bits = word.load(Ordering::Acquire);
        while bits != 0 {
            let b = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let off = (w * 64 + b) * 8;
            // SAFETY: the caller's contract: a marked offset is below cursor.
            let header = unsafe { &*((base + off) as *const ObjectHeader) };
            let size = object_total_size(header);
            if off < end || size < HEADER_SIZE || off + size > cursor {
                return Err(());
            }
            offs.push(off as u32);
            sizes.push(size as u32);
            end = off + size;
        }
    }
    Ok((offs, sizes))
}

/// `CRATONVM_G1_FULL_COMPACTION_IN_PLACE` (default ON, `=0` kills). Read once
/// per armed full compaction.
pub(super) fn g1_full_compaction_in_place_enabled() -> bool {
    true
}

/// `(compactions, reclaimed bytes, moved bytes, freed regions, declined, last
/// decline reason)` -- the `[GC] g1 in-place-compaction:` census line.
pub fn g1_in_place_compaction_census() -> (u64, u64, u64, u64, u64, &'static str) {
    (
        IN_PLACE_COMPACTIONS.load(Ordering::Relaxed),
        IN_PLACE_RECLAIMED_BYTES.load(Ordering::Relaxed),
        IN_PLACE_MOVED_BYTES.load(Ordering::Relaxed),
        IN_PLACE_FREED_REGIONS.load(Ordering::Relaxed),
        IN_PLACE_DECLINED.load(Ordering::Relaxed),
        *IN_PLACE_LAST_DECLINE.lock().unwrap_or_else(|e| e.into_inner()),
    )
}

/// `reason=count` for every decline reason seen (census).
pub fn g1_in_place_compaction_declines() -> String {
    DECLINE_REASONS
        .iter()
        .zip(IN_PLACE_DECLINED_BY.iter())
        .filter(|(_, c)| c.load(Ordering::Relaxed) != 0)
        .map(|(r, c)| format!("{r}={}", c.load(Ordering::Relaxed)))
        .collect::<Vec<_>>()
        .join(",")
}

fn note_declined(why: &'static str) {
    if let Some(i) = DECLINE_REASONS.iter().position(|r| *r == why) {
        IN_PLACE_DECLINED_BY[i].fetch_add(1, Ordering::Relaxed);
    }
    IN_PLACE_DECLINED.fetch_add(1, Ordering::Relaxed);
    *IN_PLACE_LAST_DECLINE.lock().unwrap_or_else(|e| e.into_inner()) = why;
}

/// Is the Free pool starved enough, after an armed full-compaction pause, for
/// the in-place pass to be worth its whole-heap walk? Under an eighth of the
/// heap's regions (and fewer than eight), which is where the copying
/// compaction's `room` stops being able to take a region per pause. Pure.
pub(super) fn in_place_compaction_starved(free_regions: usize, total_regions: usize) -> bool {
    free_regions < (total_regions / 8).max(8).min(total_regions)
}

/// `fill_free` (`G1Collector::arm_fragmented_humongous_slide`): the Free
/// regions below the returned index are slide destinations. It is the
/// highest region the slide can move (Old or Survivor with data, unpinned);
/// every region below it is inside the committed prefix, which is a range.
/// `0` when there is none. Pure.
fn free_destination_ceiling(regions: &[G1Region], jit_pinned: &RegionSet) -> usize {
    regions
        .iter()
        .enumerate()
        .filter(|(i, r)| {
            r.cursor() != 0
                && matches!(r.region_type(), RegionType::Old | RegionType::Survivor)
                && !r.pinned
                && r.pin_total() == 0
                && !jit_pinned.contains(i)
        })
        .map(|(i, _)| i)
        .last()
        .unwrap_or(0)
}

/// Would a planned slide leave Free exactly the regions that were Free
/// (`was_free`, one per compaction region; all false without `fill_free`,
/// where this is "every region keeps data")? Then it reclaims nothing and the
/// pass declines before anything moves. Pure.
fn slide_frees_nothing(fills: &[usize], was_free: &[bool]) -> bool {
    fills.iter().zip(was_free).all(|(&f, &w)| (f == 0) == w)
}

/// One compaction region's parse: its base, every object start (offset)
/// and size in address order, and a live bit per object, claimed by the mark.
struct RegionObjs {
    base: usize,
    offs: Vec<u32>,
    sizes: Vec<u32>,
    live: Vec<AtomicU64>,
    /// g1ep: one bit per 8-byte unit of the region, set at each object start,
    /// and `ranks[w]` = the starts in `starts[..w]`, so [`Self::index_of`] is
    /// a bit test and a popcount instead of a binary search over `offs`.
    /// Empty when an offset is not 8-aligned (the binary search answers then).
    starts: Vec<u64>,
    ranks: Vec<u32>,
}

impl RegionObjs {
    fn new(base: usize, offs: Vec<u32>, sizes: Vec<u32>) -> Self {
        let words = offs.len().div_ceil(64);
        let (starts, ranks) = Self::start_index(&offs);
        Self {
            base,
            offs,
            sizes,
            live: (0..words).map(|_| AtomicU64::new(0)).collect(),
            starts,
            ranks,
        }
    }

    /// g1ep7: a table of live objects only (`live_table_from_marks`): every
    /// object is live.
    fn all_live(base: usize, offs: Vec<u32>, sizes: Vec<u32>) -> Self {
        let r = Self::new(base, offs, sizes);
        let n = r.offs.len();
        for (w, word) in r.live.iter().enumerate() {
            let bits = (n - w * 64).min(64);
            word.store(if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 }, Ordering::Relaxed);
        }
        r
    }

    /// The start bitmap and its per-word ranks for ascending `offs`, or two
    /// empty vectors when an offset is not 8-aligned.
    fn start_index(offs: &[u32]) -> (Vec<u64>, Vec<u32>) {
        let Some(&last) = offs.last() else {
            return (Vec::new(), Vec::new());
        };
        if offs.iter().any(|&o| o % 8 != 0) {
            return (Vec::new(), Vec::new());
        }
        let words = (last as usize / 8) / 64 + 1;
        let mut starts = vec![0u64; words];
        for &o in offs {
            let unit = o as usize / 8;
            starts[unit / 64] |= 1u64 << (unit % 64);
        }
        let mut ranks = Vec::with_capacity(words);
        let mut n = 0u32;
        for &w in &starts {
            ranks.push(n);
            n += w.count_ones();
        }
        (starts, ranks)
    }

    /// The index in `offs` of the object starting at region offset `off`, or
    /// `None` when no object starts there. g1ep: the compaction's mark
    /// resolves every reference into the set through here; the binary search
    /// it replaces (`offs.binary_search`) was 4.9% of `G1epKeepRounds` CPU.
    #[inline]
    fn index_of(&self, off: u32) -> Option<usize> {
        if self.starts.is_empty() {
            return self.offs.binary_search(&off).ok();
        }
        if off % 8 != 0 {
            return None;
        }
        let unit = off as usize / 8;
        let word = *self.starts.get(unit / 64)?;
        let bit = 1u64 << (unit % 64);
        if word & bit == 0 {
            return None;
        }
        Some(self.ranks[unit / 64] as usize + (word & (bit - 1)).count_ones() as usize)
    }

    /// Mark object `j` live; `true` for the one caller that set the bit.
    #[inline]
    fn claim(&self, j: usize) -> bool {
        let bit = 1u64 << (j % 64);
        self.live[j / 64].fetch_or(bit, Ordering::AcqRel) & bit == 0
    }

    #[inline]
    fn is_live(&self, j: usize) -> bool {
        self.live[j / 64].load(Ordering::Acquire) & (1u64 << (j % 64)) != 0
    }

    /// The live objects' indices, ascending: the set bits of `live`. g1ep3:
    /// the plan's walks over a region asked [`Self::is_live`] of every parsed
    /// object, dead ones included, and a starved full's regions are mostly
    /// dead (`G1epKeepRounds 10 -Xmx1g` frees about 620 of them per pass).
    fn live_indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.live.iter().enumerate().flat_map(|(w, word)| {
            let mut bits = word.load(Ordering::Acquire);
            std::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let b = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                Some(w * 64 + b)
            })
        })
    }
}

/// The plan's chunks: contiguous `[lo, hi)` index ranges of the compaction
/// set, each slid into its own first regions. One chunk for a set under 128
/// regions (a tight heap must not lose a region per chunk tail); otherwise up
/// to one per worker, at least 64 regions each. Pure.
fn plan_chunks(len: usize, workers: usize) -> Vec<(usize, usize)> {
    let n = if len < 128 { 1 } else { workers.min(len / 64).max(1) };
    let per = len.div_ceil(n);
    (0..n)
        .map(|c| (c * per, ((c + 1) * per).min(len)))
        .filter(|&(lo, hi)| lo < hi)
        .collect()
}



/// Visit every reference slot's raw value of one object, under the same
/// bounds the Phase-4 rewrite (`update_object_refs`) applies.
fn for_each_ref_raw(
    collector: &G1Collector,
    view: RegionView<'_>,
    obj_ptr: *mut u8,
    header: &ObjectHeader,
    mut f: impl FnMut(usize),
) {
    if header.kind() == ObjectKind::Array {
        if header.element_type() == ArrayElementType::Reference {
            let data_start = unsafe { obj_ptr.add(ARRAY_DATA_OFFSET) };
            let walkable = collector.holder_walkable_slots_view(
                view,
                obj_ptr,
                header.array_length() as usize,
                8,
                HolderBound::Cursor,
            );
            for i in 0..walkable {
                // SAFETY: `holder_walkable_slots_view` bounds the walk inside
                // the holder's allocated span.
                let raw = unsafe { std::ptr::read(data_start.add(i * 8) as *const u64) } as usize;
                if raw != 0 {
                    f(raw);
                }
            }
        }
    } else {
        let walkable = collector.holder_walkable_slots_view(
            view,
            obj_ptr,
            header.num_slots() as usize,
            SLOT_SIZE,
            HolderBound::Cursor,
        );
        for_each_flat_object_reference_capped(obj_ptr, header, 0, walkable, |_, raw, _| {
            if raw != 0 {
                f(raw);
            }
        });
    }
}

/// g1sz: how far ahead of [`walk_region`]'s cursor it prefetches.
const WALK_PREFETCH_AHEAD: usize = 2048;

/// Walk region `i`'s objects from its base to its cursor by the Phase-4
/// walk's parse rules (`fixup_walk_region`): a frozen peer's TLAB span and a
/// gap filler are skipped, a humongous filler ends the region, and a size
/// that leaves the cursor is a parse failure (`false`). `f` sees every other
/// object.
pub(super) fn walk_region(
    regions: &[G1Region],
    i: usize,
    jit_skips: &[(usize, usize)],
    mut f: impl FnMut(*mut u8, &ObjectHeader, usize),
) -> bool {
    let cursor = regions[i].cursor();
    let base = regions[i].data.addr() as *mut u8;
    let mut offset = 0usize;
    while offset < cursor {
        let obj_ptr = unsafe { base.add(offset) };
        // g1sz: each header's size names the next header, so this walk is a
        // chain of dependent loads; start the line `WALK_PREFETCH_AHEAD`
        // bytes on in address order (a hint: it cannot fault).
        #[cfg(target_arch = "x86_64")]
        // SAFETY: a prefetch is a hint and cannot fault.
        unsafe {
            core::arch::x86_64::_mm_prefetch(
                obj_ptr.wrapping_add(WALK_PREFETCH_AHEAD) as *const i8,
                core::arch::x86_64::_MM_HINT_T0,
            );
        }
        if let Some(skip) = jit_tlab_skip_span_len(jit_skips, obj_ptr as usize) {
            offset += skip;
            continue;
        }
        if let Some(gap) = gap_filler_len(obj_ptr) {
            offset += gap;
            continue;
        }
        // SAFETY: below the cursor of a region the write guard holds still.
        let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
        if is_humongous_filler(header) {
            break;
        }
        let size = object_total_size(header);
        if size < HEADER_SIZE || offset + size > cursor {
            return false;
        }
        f(obj_ptr, header, size);
        offset += size;
    }
    true
}

impl G1Collector {
    /// See the module doc. `Err` names why the pass declined; nothing has
    /// been changed then.
    pub(super) fn full_compaction_in_place(
        &self,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> Result<GcResult, &'static str> {
        self.full_compaction_in_place_with(roots, monitors, false)
    }

    /// [`Self::full_compaction_in_place`]; with `fill_free`
    /// (`G1Collector::arm_fragmented_humongous_slide`), the Free regions
    /// below the top movable region join the compaction set as empty
    /// regions, so the slide packs into them and the regions it leaves Free
    /// are the top ones: one run, as HotSpot's full collection leaves.
    pub(super) fn full_compaction_in_place_with(
        &self,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
        fill_free: bool,
    ) -> Result<GcResult, &'static str> {
        let mut release = RegionSet::new();
        match self.full_compaction_in_place_releasing(
            roots,
            monitors,
            fill_free,
            &RegionSet::new(),
            &mut release,
        ) {
            Err(HELD_REGIONS_RELEASED) => {
                let mut unused = RegionSet::new();
                self.full_compaction_in_place_releasing(
                    roots, monitors, fill_free, &release, &mut unused,
                )
            }
            other => other,
        }
    }

    /// [`Self::full_compaction_in_place_with`] with the held regions in
    /// `released` made movable. Under [`g1_in_place_pins_fix_only_marked`] a
    /// first pass (empty `released`) that finds held regions with nothing
    /// fixed fills `release_out` and declines with [`HELD_REGIONS_RELEASED`]
    /// before it has changed anything.
    fn full_compaction_in_place_releasing(
        &self,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
        fill_free: bool,
        released: &RegionSet,
        release_out: &mut RegionSet,
    ) -> Result<GcResult, &'static str> {
        let start = std::time::Instant::now();
        let mut regions = self.regions.write();
        if self.satb_queue.is_active() {
            return Err("mark-cycle-open");
        }
        let region_size = self.config.region_size;
        if region_size == 0 {
            return Err("no-regions");
        }
        let pins_fix_only_marked = g1_in_place_pins_fix_only_marked();
        let mut jit_pinned = self.pinned_region_set_including_non_object_roots(&regions, roots);
        if !released.is_empty() {
            let mut kept = RegionSet::new();
            for i in jit_pinned.iter().filter(|i| !released.contains(i)) {
                kept.insert(i);
            }
            jit_pinned = kept;
        }
        let jit_skips = self.jit_tlab_skip_spans();

        // 1. The compaction set, and its objects in address order.
        let mut cset = RegionSet::new();
        let mut order: Vec<usize> = Vec::new();
        // Survivor regions too (`CRATONVM_G1_IN_PLACE_SURVIVORS`, default
        // ON): HotSpot's full collection compacts every region, young ones
        // into Old. On `Zs3Nd3UpcallMints proxy 4096 -Xmx64m` every ladder
        // that threw ended with 60 packed Old regions, 1 Free and 1-3
        // Survivor regions of up to 2 MB this pass left alone -- and kept
        // their dead objects' targets alive as holders.
        let survivors = true;
        // JIT-pinned regions are HELD (`CRATONVM_G1_IN_PLACE_HOLD_PINNED`,
        // default ON): parsed and marked like the rest, only their named
        // objects keep their place (all of them under
        // `CRATONVM_G1_IN_PLACE_HELD_OBJECTS_MOVE=0`), their dead space takes
        // objects sliding down from above
        // (`CRATONVM_G1_IN_PLACE_FILL_HELD_HOLES`) and what stays dead becomes
        // fillers. A conservative word cannot be rewritten, and a compiled loop
        // may hold a derived pointer into an object the word does not start,
        // so the object such a word touches does not move; what changes is that its DEAD
        // objects no longer hold Old objects alive (every object of an
        // excluded region is a holder, dead or alive) and their space is used. `Zs3Nd3UpcallMints proxy 4096 -Xmx64m`
        // under load: runs that wedged at 1 Free region and threw on every
        // later iteration excluded 4603 and 14335 JIT-pinned regions over
        // their passes, against 229-594 in the runs that did not.
        let hold_pinned = true;
        let mut held: Vec<bool> = Vec::new();
        let mut excluded = [0u64; 4];
        // `CRATONVM_G1_DBG_PINS=1`: each young region this pass leaves out,
        // and why (the caller prints it with the pass's outcome).
        let dbg = cratonvm_gc_core::gc_flags().g1_dbg_pins;
        let mut left_young = String::new();
        let fill_free_below = if fill_free {
            free_destination_ceiling(regions.as_slice(), &jit_pinned)
        } else {
            0
        };
        let mut was_free: Vec<bool> = Vec::new();
        let mut note_left = |i: usize, r: &G1Region, why: &str| {
            if dbg && matches!(r.region_type(), RegionType::Eden | RegionType::Survivor) {
                left_young.push_str(&format!(
                    " {i}:{:?}/{}K:{why}",
                    r.region_type(),
                    r.cursor() >> 10
                ));
            }
        };
        for (i, r) in regions.iter().enumerate() {
            let ty = r.region_type();
            if i < fill_free_below
                && ty == RegionType::Free
                && r.cursor() == 0
                && !r.pinned
                && !jit_pinned.contains(&i)
            {
                cset.insert(i);
                order.push(i);
                held.push(false);
                was_free.push(true);
                continue;
            }
            let movable_ty = ty == RegionType::Old || (survivors && ty == RegionType::Survivor);
            let jp = jit_pinned.contains(&i);
            let holdable_ty =
                matches!(ty, RegionType::Old | RegionType::Survivor | RegionType::Eden);
            if r.cursor() == 0 || !(movable_ty || (jp && hold_pinned && holdable_ty)) {
                if r.cursor() != 0 {
                    note_left(i, r, "not-pinned-young");
                }
                continue;
            }
            if r.pinned || r.pin_total() != 0 {
                excluded[0] += 1;
                note_left(i, r, "pinned");
                continue;
            }
            if jp && !hold_pinned {
                excluded[1] += 1;
                note_left(i, r, "jit-pinned");
                continue;
            }
            let base = r.data.addr();
            let end = base + r.cursor();
            if jit_skips.iter().any(|&(s, e)| s < end && e > base) {
                excluded[2] += 1;
                note_left(i, r, "tlab-span");
                continue;
            }
            if jp {
                excluded[3] += 1;
            }
            cset.insert(i);
            order.push(i);
            held.push(jp);
            was_free.push(false);
        }
        for (cell, n) in IN_PLACE_EXCLUDED.iter().zip(excluded) {
            cell.fetch_add(n, Ordering::Relaxed);
        }
        drop(note_left);
        if dbg {
            let mut held_set = RegionSet::new();
            for (k, &i) in order.iter().enumerate() {
                if held[k] {
                    held_set.insert(i);
                }
            }
            left_young.push_str(&format!(
                " held_pins={}",
                self.dbg_held_region_pin_sources(roots, &held_set)
            ));
            *IN_PLACE_DBG_LEFT.lock() = left_young;
        }
        if order.len() < 2 {
            return Err("nothing-to-slide");
        }
        // The pool width this pass may use (the driver is worker 0).
        let helpers = {
            self.parallel_worker_count()
                .saturating_sub(1)
                .min(self.evac_pool().helpers())
        };
        let workers = helpers + 1;
        let regions_len = regions.len();
        let base = RegionsBase(regions.as_mut_ptr());
        // `pos[region]`: the region's index in `order`, or `NOT_C`.
        const NOT_C: u32 = u32::MAX;
        let mut pos = vec![NOT_C; regions_len];
        for (k, &i) in order.iter().enumerate() {
            pos[i] = k as u32;
        }

        let mut lap = std::time::Instant::now();
        // g1ep7: movable regions are marked by granule and tabled afterwards
        // (`g1_in_place_mark_bitmap_enabled`).
        let bitmap_mark = true;
        let by_granule: Vec<bool> = (0..order.len()).map(|k| bitmap_mark && !held[k]).collect();
        let cursors: Vec<usize> = order.iter().map(|&i| regions[i].cursor()).collect();
        let granules: Vec<Vec<AtomicU64>> = (0..order.len())
            .map(|k| {
                if by_granule[k] {
                    (0..(cursors[k] / 8).div_ceil(64)).map(|_| AtomicU64::new(0)).collect()
                } else {
                    Vec::new()
                }
            })
            .collect();
        // 1. Parse every compaction region, in parallel: its object starts
        // and sizes, base-relative.
        let parsed: Vec<Mutex<Option<Option<RegionObjs>>>> =
            (0..order.len()).map(|_| Mutex::new(None)).collect();
        {
            let next = AtomicUsize::new(0);
            let body = |_w: usize| {
                let table: RegionsBase = base;
                // SAFETY: the caller's write guard keeps the table alive and
                // unreallocated; every access here is a shared read.
                let view: &[G1Region] =
                    unsafe { std::slice::from_raw_parts(table.0 as *const G1Region, regions_len) };
                loop {
                    let k = next.fetch_add(1, Ordering::Relaxed);
                    if k >= order.len() {
                        break;
                    }
                    let i = order[k];
                    let rbase = view[i].data.addr();
                    if by_granule[k] {
                        // Tabled after the mark (`live_table_from_marks`).
                        *parsed[k].lock() =
                            Some(Some(RegionObjs::new(rbase, Vec::new(), Vec::new())));
                        continue;
                    }
                    // g1ep4: room for the most objects the region can hold
                    // (one per header's worth of its used bytes), so the
                    // tables never grow by copy. Past the allocator's 64 KiB
                    // arena cap a table is its own mapping, and its unused
                    // tail is never touched, so never faulted in; grown from
                    // empty, every doubling past the cap was a fresh mapping
                    // filled by copy (135 k page faults over the nine full
                    // collections of `G1epKeepRounds 10 -Xmx1g`).
                    let most = view[i].cursor() / HEADER_SIZE + 1;
                    let mut offs = Vec::with_capacity(most);
                    let mut sizes = Vec::with_capacity(most);
                    let ok = walk_region(view, i, &[], |p, _, size| {
                        offs.push((p as usize - rbase) as u32);
                        sizes.push(size as u32);
                    });
                    *parsed[k].lock() = Some(ok.then(|| RegionObjs::new(rbase, offs, sizes)));
                }
            };
            self.evac_pool().scope(helpers, &body, || body(helpers));
        }
        let mut objs: Vec<RegionObjs> = Vec::with_capacity(order.len());
        for (k, slot) in parsed.into_iter().enumerate() {
            match slot.into_inner() {
                Some(Some(r)) => objs.push(r),
                _ => {
                    if dbg {
                        let i = order[k];
                        IN_PLACE_DBG_LEFT.lock().push_str(&format!(
                            " unparsable={i}:{:?}/{}K:held={}",
                            regions[i].region_type(),
                            regions[i].cursor() >> 10,
                            held[k]
                        ));
                    }
                    return Err("compaction-region-unparsable");
                }
            }
        }
        let find_in = |objs: &[RegionObjs], addr: usize| -> Result<Option<(usize, usize)>, ()> {
            let Some(ridx) = self.lookup_region_for_addr(addr) else {
                return Ok(None);
            };
            let Some(&k) = pos.get(ridx) else {
                return Ok(None);
            };
            if k == NOT_C {
                return Ok(None);
            }
            let k = k as usize;
            let r = &objs[k];
            let off = addr.wrapping_sub(r.base);
            if off >= region_size {
                return Err(());
            }
            match r.index_of(off as u32) {
                Some(j) => Ok(Some((k, j))),
                None => Err(()),
            }
        };
        // A word into a HELD region names the object containing it, and --
        // when it starts one and `ends` says it may be a conservative JIT word
        // -- the object it ends too (a compiled loop's end pointer). Empty for
        // any other address.
        let held_named_in = |objs: &[RegionObjs], addr: usize, ends: bool, out: &mut Vec<(usize, usize)>| {
            let Some(ridx) = self.lookup_region_for_addr(addr) else {
                return;
            };
            let Some(&k) = pos.get(ridx) else {
                return;
            };
            if k == NOT_C || !held[k as usize] {
                return;
            }
            let k = k as usize;
            let r = &objs[k];
            let off = addr.wrapping_sub(r.base);
            if off >= region_size {
                return;
            }
            let off = off as u32;
            match r.offs.binary_search(&off) {
                Ok(j) => {
                    out.push((k, j));
                    if ends && j > 0 {
                        out.push((k, j - 1));
                    }
                }
                Err(0) => {}
                Err(p) => {
                    let j = p - 1;
                    if off <= r.offs[j] + r.sizes[j] {
                        out.push((k, j));
                    }
                }
            }
        };
        // g1ep6: the words that may be end pointers -- the conservative JIT
        // words, every one of which is published as a pin (the initiator's
        // scan section, the frozen peers' words). Every other root is precise:
        // a reference to the object it starts, never to the one below it. A
        // shadow-stack root that started the object right after round 0's
        // `Node[64]` ring on `G1epKeepRounds 10 -Xmx1g` kept all 652 MB of it.
        let pin_words: Vec<usize> = {
            let mut v = cratonvm_gc_core::gc_quiescence::pinned_jit_roots_snapshot();
            v.sort_unstable();
            v.dedup();
            v
        };
        let may_end = |addr: usize| pin_words.binary_search(&addr).is_ok();
        let held_named =
            |addr: usize, ends: bool, out: &mut Vec<(usize, usize)>| held_named_in(&objs, addr, ends, out);

        in_place_lap(&mut lap, 0);
        // 2. Mark, in parallel: seeds from the roots, the pending finalizers
        // and the overlay rows (driver), then every object of every non-C
        // region as a holder (workers claim regions), then the closure inside
        // C. A bit per parsed object, claimed with `fetch_or`, so each live
        // object is pushed once. Work is shared through `shared` when a
        // worker's own stack is deep and the shared queue is empty.
        // g1ep7: a work item is an object's address (a held region's objects
        // are claimed by index, a movable region's by granule).
        let interior = AtomicUsize::new(0);
        let shared: Mutex<Vec<usize>> = Mutex::new(Vec::new());
        let shared_len = AtomicUsize::new(0);
        let idle = AtomicUsize::new(0);
        let source_unparsable = AtomicBool::new(false);
        let claim_granule = |k: usize, addr: usize| -> Result<bool, ()> {
            let off = addr.wrapping_sub(objs[k].base);
            if off >= cursors[k] || off % 8 != 0 {
                return Err(());
            }
            let g = off / 8;
            let bit = 1u64 << (g % 64);
            Ok(granules[k][g / 64].fetch_or(bit, Ordering::AcqRel) & bit == 0)
        };
        let mark_into = |addr: usize, local: &mut Vec<usize>| {
            let claimed = match pos.get(self.lookup_region_for_addr(addr).unwrap_or(usize::MAX)) {
                Some(&k) if k != NOT_C && by_granule[k as usize] => claim_granule(k as usize, addr),
                _ => match find_in(&objs, addr) {
                    Ok(Some((k, j))) => Ok(objs[k].claim(j)),
                    Ok(None) => Ok(false),
                    Err(()) => Err(()),
                },
            };
            match claimed {
                Ok(true) => {
                    // g1sz: its header is the drain's next miss when popped.
                    #[cfg(target_arch = "x86_64")]
                    // SAFETY: a prefetch is a hint and cannot fault.
                    unsafe {
                        core::arch::x86_64::_mm_prefetch(
                            addr as *const i8,
                            core::arch::x86_64::_MM_HINT_T0,
                        );
                    }
                    local.push(addr);
                }
                Ok(false) => {}
                Err(()) => {
                    interior.fetch_add(1, Ordering::Relaxed);
                }
            }
        };
        {
            let mut seeds: Vec<usize> = Vec::new();
            let mut named: Vec<(usize, usize)> = Vec::new();
            let mut claim_named = |named: &mut Vec<(usize, usize)>, seeds: &mut Vec<usize>| {
                for (k, j) in named.drain(..) {
                    if objs[k].claim(j) {
                        seeds.push(objs[k].base + objs[k].offs[j] as usize);
                    }
                }
            };
            for r in roots.iter() {
                let addr = r.as_ptr() as usize;
                held_named(addr, may_end(addr), &mut named);
                if named.is_empty() {
                    mark_into(addr, &mut seeds);
                } else {
                    claim_named(&mut named, &mut seeds);
                }
            }
            // The conservative JIT words the pin set was built from: each
            // keeps what it names in a held region (not under
            // `g1_in_place_pins_fix_only_marked`: only the roots mark).
            if hold_pinned && !pins_fix_only_marked && jit_frames_live_for_pins() {
                for addr in cratonvm_gc_core::gc_quiescence::pinned_jit_roots_snapshot() {
                    held_named(addr, true, &mut named);
                    claim_named(&mut named, &mut seeds);
                }
            }
            for &a in self.pending_finalizer_roots.lock().iter() {
                mark_into(a, &mut seeds);
            }
            if cratonvm_gc_core::external_roots::has_providers() {
                let mut rows = Vec::new();
                cratonvm_gc_core::external_roots::scan_external_roots(&mut rows);
                for r in rows {
                    mark_into(r.as_ptr() as usize, &mut seeds);
                }
            }
            shared_len.store(seeds.len(), Ordering::Release);
            *shared.lock() = seeds;
        }
        let sources: Vec<usize> = (0..regions_len)
            .filter(|&i| pos[i] == NOT_C && regions[i].region_type() != RegionType::Free)
            .collect();
        {
            let next = AtomicUsize::new(0);
            const SHARE_AT: usize = 64;
            let body = |_w: usize| {
                let table: RegionsBase = base;
                // SAFETY: as in the parse above.
                let view: &[G1Region] =
                    unsafe { std::slice::from_raw_parts(table.0 as *const G1Region, regions_len) };
                let rview = RegionView::Raw(table, regions_len);
                let mut local: Vec<usize> = Vec::new();
                let mut drain = |local: &mut Vec<usize>| {
                    while let Some(addr) = local.pop() {
                        let p = addr as *mut u8;
                        // SAFETY: a claimed object start of a compaction region,
                        // unmoved (a granule claim is checked against the
                        // region's cursor; `live_table_from_marks` refuses the
                        // pass if it names no object).
                        let h = unsafe { &*(p as *const ObjectHeader) };
                        for_each_ref_raw(self, rview, p, h, |raw| mark_into(raw, local));
                        // r23 w6 (`R23W6KeepLong`): with a peer idle, half a
                        // stack of two or more is given away too. A chain-shaped
                        // graph keeps every stack one or two deep, so the
                        // depth rule alone never shared: one root array of 64
                        // chain heads was marked by one worker (480-630 ms of a
                        // 614-788 ms pass, 400 MB live).
                        let give_n = if shared_len.load(Ordering::Relaxed) != 0 {
                            0
                        } else if local.len() >= 2 * SHARE_AT {
                            SHARE_AT
                        } else if local.len() >= 2 && idle.load(Ordering::Relaxed) != 0 {
                            local.len() / 2
                        } else {
                            0
                        };
                        if give_n != 0 {
                            let give: Vec<usize> = local.drain(..give_n).collect();
                            let mut q = shared.lock();
                            q.extend(give);
                            shared_len.store(q.len(), Ordering::Release);
                        }
                    }
                };
                // The non-C holders.
                loop {
                    let s = next.fetch_add(1, Ordering::Relaxed);
                    if s >= sources.len() {
                        break;
                    }
                    let parsed = walk_region(view, sources[s], &jit_skips, |p, h, _| {
                        for_each_ref_raw(self, rview, p, h, |raw| mark_into(raw, &mut local));
                    });
                    if !parsed {
                        source_unparsable.store(true, Ordering::Relaxed);
                    }
                    drain(&mut local);
                }
                // The closure, with the shared queue as the balancer.
                loop {
                    drain(&mut local);
                    let took = {
                        let mut q = shared.lock();
                        let n = q.len().min(SHARE_AT);
                        let at = q.len() - n;
                        local.extend(q.drain(at..));
                        shared_len.store(q.len(), Ordering::Release);
                        n
                    };
                    if took > 0 {
                        continue;
                    }
                    // Nothing here and nothing shared: idle until either work
                    // appears or every worker is idle.
                    idle.fetch_add(1, Ordering::AcqRel);
                    loop {
                        if shared_len.load(Ordering::Acquire) > 0 {
                            idle.fetch_sub(1, Ordering::AcqRel);
                            break;
                        }
                        if idle.load(Ordering::Acquire) == workers {
                            return;
                        }
                        std::hint::spin_loop();
                        std::thread::yield_now();
                    }
                }
            };
            self.evac_pool().scope(helpers, &body, || body(helpers));
        }
        if source_unparsable.load(Ordering::Relaxed) {
            return Err("source-region-unparsable");
        }
        if interior.load(Ordering::Relaxed) != 0 {
            return Err("interior-reference-into-compaction-set");
        }
        // g1ep7: the movable regions' tables, from their granule marks, in
        // parallel; live objects only.
        let objs: Vec<RegionObjs> = if by_granule.iter().any(|&g| g) {
            let tabled: Vec<Mutex<Option<Result<RegionObjs, ()>>>> =
                (0..order.len()).map(|_| Mutex::new(None)).collect();
            let next = AtomicUsize::new(0);
            let body = |_w: usize| loop {
                let k = next.fetch_add(1, Ordering::Relaxed);
                if k >= order.len() {
                    break;
                }
                if !by_granule[k] {
                    continue;
                }
                let rbase = objs[k].base;
                // SAFETY: a granule claim is below the region's cursor, and the
                // write guard holds the region still.
                let t = unsafe { live_table_from_marks(rbase, cursors[k], &granules[k]) };
                *tabled[k].lock() = Some(t.map(|(offs, sizes)| RegionObjs::all_live(rbase, offs, sizes)));
            };
            self.evac_pool().scope(helpers, &body, || body(helpers));
            let mut rebuilt = Vec::with_capacity(order.len());
            for (k, (old, slot)) in objs.into_iter().zip(tabled).enumerate() {
                match slot.into_inner() {
                    Some(Ok(r)) => rebuilt.push(r),
                    Some(Err(())) => return Err("interior-reference-into-compaction-set"),
                    None => {
                        debug_assert!(!by_granule[k]);
                        rebuilt.push(old);
                    }
                }
            }
            rebuilt
        } else {
            objs
        };
        drop(granules);
        let find = |addr: usize| find_in(&objs, addr);
        let held_named =
            |addr: usize, ends: bool, out: &mut Vec<(usize, usize)>| held_named_in(&objs, addr, ends, out);

        // A held region's FIXED objects: the ones a root or a conservative JIT
        // word names (`held_named`, the mark's own seeding rule). Under
        // `CRATONVM_G1_IN_PLACE_HELD_OBJECTS_MOVE` (default ON) only these keep
        // their address; every other live object of a held region slides like
        // a movable region's. Off, every live object of a held region is fixed.
        let held_objects_move = hold_pinned
            && g1_in_place_held_objects_move_enabled();
        let fixed: Vec<Vec<bool>> = objs
            .iter()
            .enumerate()
            .map(|(k, r)| {
                if !held[k] {
                    Vec::new()
                } else if held_objects_move {
                    vec![false; r.offs.len()]
                } else {
                    (0..r.offs.len()).map(|j| r.is_live(j)).collect()
                }
            })
            .collect();
        let mut fixed = fixed;
        // Which roots hold their object in place: under
        // `CRATONVM_G1_IN_PLACE_HELD_OBJECTS_MOVE`, only a root an evacuation
        // pause would pin its region for (`pinned_region_set_with_pin_screen`:
        // not a stamped, followable object start). Every other root is
        // rewritten through the pointer map like any root into a movable
        // region. On the near-full proxy row a held Old region carried 4000-8000
        // such roots, which fixed most of its objects.
        let root_holds: Vec<bool> = roots
            .iter()
            .map(|r| {
                let addr = r.as_ptr() as usize;
                if !held_objects_move {
                    return true;
                }
                let in_held = self
                    .lookup_region_for_addr(addr)
                    .and_then(|ridx| pos.get(ridx))
                    .is_some_and(|&k| k != NOT_C && held[k as usize]);
                if !in_held {
                    return false;
                }
                let starts_object = matches!(find(addr), Ok(Some(_)));
                // SAFETY: `addr` starts a parsed object of a held region.
                let stamped = starts_object
                    && unsafe {
                        (*(addr as *const ObjectHeader)).gc_flags() & cratonvm_types::GC_FLAG_HEADER
                            != 0
                    };
                !(stamped && self.addr_is_followable_object(&regions, addr, "in-place-held-root"))
            })
            .collect();
        if held_objects_move {
            let mut named: Vec<(usize, usize)> = Vec::new();
            for (r, &holds) in roots.iter().zip(root_holds.iter()) {
                if holds {
                    let addr = r.as_ptr() as usize;
                    held_named(addr, may_end(addr), &mut named);
                }
            }
            if jit_frames_live_for_pins() {
                for addr in cratonvm_gc_core::gc_quiescence::pinned_jit_roots_snapshot() {
                    held_named(addr, true, &mut named);
                }
            }
            if pins_fix_only_marked {
                named.retain(|&(k, j)| objs[k].is_live(j));
                if released.is_empty() {
                    let mut keep = vec![false; order.len()];
                    for &(k, _) in &named {
                        keep[k] = true;
                    }
                    for (r, &holds) in roots.iter().zip(root_holds.iter()) {
                        let k = self
                            .lookup_region_for_addr(r.as_ptr() as usize)
                            .and_then(|ridx| pos.get(ridx).copied());
                        if let Some(k) = k.filter(|&k| holds && k != NOT_C) {
                            keep[k as usize] = true;
                        }
                    }
                    for (k, &i) in order.iter().enumerate() {
                        if held[k] && !keep[k] {
                            release_out.insert(i);
                        }
                    }
                    if !release_out.is_empty() {
                        return Err(HELD_REGIONS_RELEASED);
                    }
                }
            }
            for (k, j) in named {
                fixed[k][j] = true;
            }
        }
        let is_fixed = |k: usize, j: usize| fixed[k].get(j) == Some(&true);

        in_place_lap(&mut lap, 1);
        // 3. Plan: the compaction set split into contiguous chunks, each slid
        // into its own first regions (a destination never passes its source
        // inside a chunk, and chunks never share a region). One chunk on a
        // small set, so a tight heap loses no region to chunk tails.
        let chunks = plan_chunks(order.len(), workers);
        // A held region's dead runs and its tail above the last object, as
        // destinations (`CRATONVM_G1_IN_PLACE_FILL_HELD_HOLES`, default ON):
        // a held region's fixed objects stay, but objects from the regions above
        // it may slide into the space its dead (or packed-away) objects left.
        let fill_holes = hold_pinned;
        // Every root and conservative word into a held region, by region: no
        // object placed in a hole may cover one (it would turn a word naming
        // dead space -- a stale word into the tail -- into a pointer into an
        // object's fields, whose bytes the next pause's root screen decodes
        // as a header). A hole is split at each such word, so an object
        // placed there ends at it or starts at it.
        let mut pin_offs: Vec<Vec<usize>> = vec![Vec::new(); order.len()];
        if fill_holes {
            let mut note_pin = |addr: usize| {
                let Some(ridx) = self.lookup_region_for_addr(addr) else {
                    return;
                };
                let Some(&k) = pos.get(ridx) else {
                    return;
                };
                if k != NOT_C && held[k as usize] {
                    let off = addr.wrapping_sub(objs[k as usize].base);
                    if off < region_size {
                        pin_offs[k as usize].push(off);
                    }
                }
            };
            for (r, &holds) in roots.iter().zip(root_holds.iter()) {
                if holds {
                    note_pin(r.as_ptr() as usize);
                }
            }
            if jit_frames_live_for_pins() {
                for addr in cratonvm_gc_core::gc_quiescence::pinned_jit_roots_snapshot() {
                    note_pin(addr);
                }
            }
        }
        // A held region's unnamed live objects (`CRATONVM_G1_IN_PLACE_HELD_OBJECTS_MOVE`)
        // slide down INSIDE their own region, around its fixed objects, in
        // ascending order: each one fits at or below its own offset (a word
        // inside it or at either end would have fixed it, so it lies in one
        // gap between fixed objects and pin words). What that frees is one
        // run per gap instead of the dead objects' scattered runs, which the
        // lift below can fill from the movable regions. They never leave the
        // region: moved down into the movable regions they would only take
        // the space the lift frees there. Copied first, before the lift.
        let mut held_pairs: Vec<(usize, usize)> = Vec::new();
        let mut held_moved = 0usize;
        let mut holes: Vec<Vec<(usize, usize)>> = (0..order.len())
            .map(|k| {
                if !(held[k] && fill_holes) {
                    return Vec::new();
                }
                if !held_objects_move {
                    return split_holes_at(
                        held_region_holes(&objs[k], region_size),
                        &mut pin_offs[k],
                    );
                }
                let r = &objs[k];
                let mut fixed_spans: Vec<(usize, usize)> = Vec::new();
                for j in 0..r.offs.len() {
                    if is_fixed(k, j) {
                        let o = r.offs[j] as usize;
                        fixed_spans.push((o, o + r.sizes[j] as usize));
                    }
                }
                let gaps_between = |spans: &[(usize, usize)]| {
                    let mut gaps = Vec::new();
                    let mut at = 0usize;
                    for &(s0, s1) in spans
                        .iter()
                        .chain(std::iter::once(&(region_size, region_size)))
                    {
                        if s0 > at {
                            gaps.push((at, s0));
                        }
                        at = at.max(s1);
                    }
                    gaps
                };
                let gaps = split_holes_at(gaps_between(&fixed_spans), &mut pin_offs[k]);
                let mut occupied = fixed_spans;
                let (mut g, mut at) = (0usize, 0usize);
                for j in 0..r.offs.len() {
                    if !r.is_live(j) || is_fixed(k, j) {
                        continue;
                    }
                    let (o, size) = (r.offs[j] as usize, r.sizes[j] as usize);
                    let st = loop {
                        let Some(&(gs, ge)) = gaps.get(g) else {
                            break o;
                        };
                        let st = gs.max(at);
                        if st + size <= ge {
                            break st;
                        }
                        g += 1;
                        at = 0;
                    };
                    debug_assert!(st <= o, "an in-region slide passed its source");
                    at = st + size;
                    occupied.push((st, st + size));
                    held_pairs.push((r.base + o, r.base + st));
                    if st != o {
                        held_moved += size;
                    }
                }
                occupied.sort_unstable();
                split_holes_at(gaps_between(&occupied), &mut pin_offs[k])
            })
            .collect();
        // The lift (`CRATONVM_G1_IN_PLACE_LIFT_INTO_HELD_HOLES`, default ON):
        // the slide only moves objects down, so a held region above every
        // movable one takes nothing, and its dead space is lost until its
        // pin goes. `Zs3Nd3UpcallMints proxy 4096 -Xmx64m` under load threw
        // with three JIT-pinned Eden regions (3 MB, mostly dead) at the top of
        // the heap, over 60 packed Old regions with less than a region of
        // garbage: every pass declined. Here movable regions, cheapest first,
        // are emptied into the held regions' holes, wherever those are, one
        // whole region at a time while its live objects all fit. A hole is
        // dead space no source overlaps, and it leaves the slide's holes
        // before the slide is planned, so the copies run first, from intact
        // sources. A lifted region is no source of the slide; the slide may
        // still use it as a destination (its objects are out by then), and
        // what it does not use is freed.
        // Diagnostic (`CRATONVM_G1_DBG_IN_PLACE_RETAIN`): the holes the lift saw.
        let holes_before_lift = holes.clone();
        let mut lifted = vec![false; order.len()];
        let mut lift_pairs: Vec<(usize, usize)> = Vec::new();
        let mut lift_fills = vec![0usize; order.len()];
        let mut lift_bytes = 0usize;
        if fill_holes && g1_in_place_lift_enabled() && holes.iter().any(|h| !h.is_empty()) {
            // Cheapest first: the movable regions by live bytes.
            let mut by_live: Vec<(usize, usize)> = (0..order.len())
                .filter(|&k| !held[k])
                .map(|k| {
                    let r = &objs[k];
                    let live = r.live_indices().map(|j| r.sizes[j] as usize).sum();
                    (live, k)
                })
                .collect();
            by_live.sort_unstable();
            // g1ep3: the regions that have holes (held ones only), in order. A
            // trial copies their lists and fills alone; it used to clone the
            // hole list and fill of every compaction region, once per region
            // tried, and a pass tries every empty one (about 620 per pass on
            // `G1epKeepRounds 10 -Xmx1g`). The same first fit: an empty list
            // never placed an object.
            let with_holes: Vec<usize> =
                (0..order.len()).filter(|&k| !holes[k].is_empty()).collect();
            for &(live, k) in &by_live {
                if live == 0 {
                    lifted[k] = true;
                    continue;
                }
                let r = &objs[k];
                let mut trial: Vec<Vec<(usize, usize)>> =
                    with_holes.iter().map(|&hk| holes[hk].clone()).collect();
                let mut pairs: Vec<(usize, usize)> = Vec::new();
                let mut fills_k: Vec<usize> = with_holes.iter().map(|&hk| lift_fills[hk]).collect();
                let mut bytes = 0usize;
                let mut fits = true;
                for j in r.live_indices() {
                    let size = r.sizes[j] as usize;
                    let mut placed = None;
                    'find: for (t, hs) in trial.iter_mut().enumerate() {
                        for hole in hs.iter_mut() {
                            if hole.1 - hole.0 >= size {
                                placed = Some((t, hole.0));
                                hole.0 += size;
                                break 'find;
                            }
                        }
                    }
                    let Some((t, at)) = placed else {
                        fits = false;
                        break;
                    };
                    pairs.push((r.base + r.offs[j] as usize, objs[with_holes[t]].base + at));
                    fills_k[t] = fills_k[t].max(at + size);
                    bytes += size;
                }
                if !fits {
                    break;
                }
                for (t, &hk) in with_holes.iter().enumerate() {
                    holes[hk] = std::mem::take(&mut trial[t]);
                    lift_fills[hk] = fills_k[t];
                }
                lift_pairs.extend(pairs);
                lift_bytes += bytes;
                lifted[k] = true;
            }
        }
        // Then single objects, from the top movable regions down, into what
        // the holes still hold: each one lifted shrinks the slide's packing by
        // its size, so a heap whose regions each hold more live data than the
        // holes (a megabyte of holes over 60 regions about 99.9 % live, the
        // same row) still frees a region. A pass that would free none declines
        // before anything moves.
        let mut lifted_obj: Vec<Vec<bool>> = vec![Vec::new(); order.len()];
        let hole_left = |hs: &[Vec<(usize, usize)>]| -> usize {
            hs.iter().flat_map(|h| h.iter().map(|&(s, e)| e - s)).sum()
        };
        if fill_holes && g1_in_place_lift_enabled() && hole_left(&holes) > 0 {
            const OBJECT_LIFT_REGIONS: usize = 8;
            let mut scanned = 0usize;
            for k in (0..order.len()).rev() {
                if held[k] || lifted[k] {
                    continue;
                }
                if scanned == OBJECT_LIFT_REGIONS || hole_left(&holes) < 64 {
                    break;
                }
                scanned += 1;
                let r = &objs[k];
                for j in (0..r.offs.len()).rev() {
                    if !r.is_live(j) {
                        continue;
                    }
                    let size = r.sizes[j] as usize;
                    let mut placed = None;
                    'fit: for (hk, hs) in holes.iter_mut().enumerate() {
                        for hole in hs.iter_mut() {
                            if hole.1 - hole.0 >= size {
                                placed = Some((hk, hole.0));
                                hole.0 += size;
                                break 'fit;
                            }
                        }
                    }
                    let Some((hk, at)) = placed else {
                        continue;
                    };
                    if lifted_obj[k].is_empty() {
                        lifted_obj[k] = vec![false; r.offs.len()];
                    }
                    lifted_obj[k][j] = true;
                    lift_pairs.push((r.base + r.offs[j] as usize, objs[hk].base + at));
                    lift_fills[hk] = lift_fills[hk].max(at + size);
                    lift_bytes += size;
                }
            }
        }
        let lifted_objects = lifted_obj.iter().flatten().filter(|&&l| l).count();
        let lifted_regions = lifted.iter().filter(|&&l| l).count();
        // Each chunk plans on its own (the pool claims chunks): its pairs, its
        // regions' fills, live and moved bytes, or why it refused.
        struct ChunkPlan {
            pairs: Vec<(usize, usize)>,
            fills: Vec<usize>,
            live: usize,
            moved: usize,
            held_live: usize,
        }
        let plan_chunk = |lo: usize, hi: usize| -> Result<ChunkPlan, &'static str> {
            // g1ep4: room for every pair the chunk can record -- a held
            // region's every object, another's live ones -- so the list never
            // grows by copy (each doubling past the allocator's 64 KiB arena
            // cap was a fresh mapping; the unused tail is never faulted in).
            let most: usize = (lo..hi)
                .map(|k| {
                    if held[k] {
                        objs[k].offs.len()
                    } else {
                        let words = objs[k].live.iter();
                        words.map(|w| w.load(Ordering::Relaxed).count_ones() as usize).sum()
                    }
                })
                .sum();
            let mut plan = ChunkPlan {
                pairs: Vec::with_capacity(most),
                fills: vec![0usize; hi - lo],
                live: 0,
                moved: 0,
                held_live: 0,
            };
            // The destination: region `d` at offset `d_off`; in a held
            // region, its hole `h` onwards.
            let mut d = lo;
            let mut d_off = 0usize;
            let mut h = 0usize;
            for k in lo..hi {
                let r = &objs[k];
                if lifted[k] {
                    continue;
                }
                if held[k] {
                    // A held region: every fixed object keeps its address (an
                    // identity forward) and its fill is at least its parse
                    // end. The destination stays where it is: it is below
                    // this region (every source so far is), and the sources
                    // above may reach this region's holes. Its other live
                    // objects were placed inside it before the lift.
                    for j in 0..r.offs.len() {
                        if is_fixed(k, j) {
                            let old = r.base + r.offs[j] as usize;
                            plan.pairs.push((old, old));
                            plan.held_live += r.sizes[j] as usize;
                        }
                    }
                    let parse_end = r
                        .offs
                        .last()
                        .zip(r.sizes.last())
                        .map(|(&o, &n)| o as usize + n as usize)
                        .unwrap_or(0);
                    plan.fills[k - lo] = plan.fills[k - lo].max(parse_end).max(1);
                    continue;
                }
                for j in r.live_indices() {
                    if lifted_obj[k].get(j) == Some(&true) {
                        continue;
                    }
                    let size = r.sizes[j] as usize;
                    let at = loop {
                        if d >= hi {
                            return Err("plan-overflow");
                        }
                        if held[d] {
                            let mut fit = None;
                            while let Some(&(hs, he)) = holes[d].get(h) {
                                let st = hs.max(d_off);
                                if st + size <= he {
                                    fit = Some(st);
                                    break;
                                }
                                h += 1;
                            }
                            match fit {
                                Some(st) => break st,
                                None => {
                                    d += 1;
                                    d_off = 0;
                                    h = 0;
                                }
                            }
                        } else if d_off + size > region_size {
                            d += 1;
                            d_off = 0;
                            h = 0;
                        } else {
                            break d_off;
                        }
                    };
                    let old = r.base + r.offs[j] as usize;
                    let new_addr = objs[d].base + at;
                    if new_addr > old {
                        return Err("plan-passed-source");
                    }
                    d_off = at + size;
                    plan.fills[d - lo] = plan.fills[d - lo].max(d_off);
                    plan.live += size;
                    if new_addr != old {
                        plan.moved += size;
                    }
                    plan.pairs.push((old, new_addr));
                }
            }
            Ok(plan)
        };
        let planned: Vec<Mutex<Option<Result<ChunkPlan, &'static str>>>> =
            (0..chunks.len()).map(|_| Mutex::new(None)).collect();
        if chunks.len() > 1 {
            let next = AtomicUsize::new(0);
            let body = |_w: usize| loop {
                let c = next.fetch_add(1, Ordering::Relaxed);
                if c >= chunks.len() {
                    break;
                }
                let (lo, hi) = chunks[c];
                *planned[c].lock() = Some(plan_chunk(lo, hi));
            };
            self.evac_pool().scope(helpers.min(chunks.len() - 1), &body, || body(helpers));
        } else if let Some(&(lo, hi)) = chunks.first() {
            *planned[0].lock() = Some(plan_chunk(lo, hi));
        }
        let mut fills = vec![0usize; order.len()];
        let mut chunk_pairs: Vec<Vec<(usize, usize)>> = Vec::with_capacity(chunks.len());
        let mut moved_bytes = 0usize;
        let mut live_bytes_total = 0usize;
        for (slot, &(lo, _)) in planned.into_iter().zip(chunks.iter()) {
            let plan = slot.into_inner().ok_or("plan-overflow")??;
            fills[lo..lo + plan.fills.len()].copy_from_slice(&plan.fills);
            live_bytes_total += plan.live;
            moved_bytes += plan.moved;
            debug_assert!(plan.held_live <= region_size * (plan.fills.len()));
            chunk_pairs.push(plan.pairs);
        }
        // The lift's objects: in the held regions' fills, and as one more set
        // of pairs (the map's and the fillers' input; copied before the slide).
        let slide_chunks = chunk_pairs.len();
        moved_bytes += held_moved;
        let lift_chunk = (!lift_pairs.is_empty()).then_some(slide_chunks);
        if !lift_pairs.is_empty() {
            for (f, &l) in fills.iter_mut().zip(lift_fills.iter()) {
                *f = (*f).max(l);
            }
            live_bytes_total += lift_bytes;
            moved_bytes += lift_bytes;
            chunk_pairs.push(std::mem::take(&mut lift_pairs));
        }
        // The held regions' own slides: identity-free pairs, copied first.
        let held_chunk = (!held_pairs.is_empty()).then_some(chunk_pairs.len());
        if !held_pairs.is_empty() {
            chunk_pairs.push(std::mem::take(&mut held_pairs));
        }
        // What a slide can give back: the movable regions' bytes (a held
        // region keeps its cursor; its dead objects become fillers, not space).
        let used_before: usize = order
            .iter()
            .zip(held.iter())
            .filter(|&(_, &h)| !h)
            .map(|(&i, _)| regions[i].cursor())
            .sum();
        // Decline when the slide would empty no region: the walk above is all
        // this pass costs then. Counted in REGIONS, not garbage bytes: the
        // region tails above every cursor are reclaimed by the packing too --
        // on `Zs3Nd3UpcallMints proxy -Xmx64m` 61 Old regions held 62.6 MB,
        // less than a region of garbage but 1.4 MB of tails, and the one
        // region they free was the difference between a wedged heap and a
        // served allocation.
        if slide_frees_nothing(&fills, &was_free) {
            if dbg_retain_wanted(regions.as_slice()) {
                let jit_root: Vec<bool> = roots
                    .iter()
                    .map(|r| {
                        cratonvm_gc_core::gc_quiescence::lookup_jit_root_provenance(r.as_ptr() as usize)
                            .is_some()
                    })
                    .collect();
                let mark =
                    |seed_roots: u8, seed_pins: bool, seed_holders: bool, only: Option<usize>| {
                        let mut seen: Vec<Vec<bool>> =
                            objs.iter().map(|r| vec![false; r.offs.len()]).collect();
                        let mut stack: Vec<(usize, usize)> = Vec::new();
                        let mut named: Vec<(usize, usize)> = Vec::new();
                        let rview = RegionView::Raw(base, regions_len);
                        let mut push = |kj: (usize, usize), stack: &mut Vec<(usize, usize)>| {
                            if !seen[kj.0][kj.1] {
                                seen[kj.0][kj.1] = true;
                                stack.push(kj);
                            }
                        };
                        if seed_roots != 0 {
                            for (ri, r) in roots.iter().enumerate() {
                                if seed_roots == 2 && jit_root[ri] {
                                    continue;
                                }
                                let addr = r.as_ptr() as usize;
                                held_named(addr, may_end(addr), &mut named);
                                if named.is_empty() {
                                    if let Ok(Some(kj)) = find(addr) {
                                        push(kj, &mut stack);
                                    }
                                }
                                for kj in named.drain(..) {
                                    push(kj, &mut stack);
                                }
                            }
                            for &a in self.pending_finalizer_roots.lock().iter() {
                                if let Ok(Some(kj)) = find(a) {
                                    push(kj, &mut stack);
                                }
                            }
                            if cratonvm_gc_core::external_roots::has_providers() {
                                let mut rows = Vec::new();
                                cratonvm_gc_core::external_roots::scan_external_roots(&mut rows);
                                for r in rows {
                                    if let Ok(Some(kj)) = find(r.as_ptr() as usize) {
                                        push(kj, &mut stack);
                                    }
                                }
                            }
                        }
                        if seed_pins || only.is_some() {
                            for (n, addr) in cratonvm_gc_core::gc_quiescence::pinned_jit_roots_snapshot()
                                .into_iter()
                                .enumerate()
                            {
                                if only.is_some_and(|o| o != n) {
                                    continue;
                                }
                                held_named(addr, true, &mut named);
                                for kj in named.drain(..) {
                                    push(kj, &mut stack);
                                }
                            }
                        }
                        if seed_holders {
                            for &s in &sources {
                                walk_region(&regions, s, &jit_skips, |p, h, _| {
                                    for_each_ref_raw(self, rview, p, h, |raw| {
                                        if let Ok(Some(kj)) = find(raw) {
                                            push(kj, &mut stack);
                                        }
                                    });
                                });
                            }
                        }
                        while let Some((k, j)) = stack.pop() {
                            let r = &objs[k];
                            let p = (r.base + r.offs[j] as usize) as *mut u8;
                            // SAFETY: a parsed object of a compaction region, unmoved.
                            let h = unsafe { &*(p as *const ObjectHeader) };
                            for_each_ref_raw(self, rview, p, h, |raw| {
                                if let Ok(Some(kj)) = find(raw) {
                                    push(kj, &mut stack);
                                }
                            });
                        }
                        seen
                    };
                let domain = self.layout_domain();
                let diff = |a: &Vec<Vec<bool>>, b: &Vec<Vec<bool>>| -> (usize, String) {
                    let mut by: std::collections::HashMap<u32, (usize, usize)> =
                        std::collections::HashMap::new();
                    let mut total = 0usize;
                    for (k, r) in objs.iter().enumerate() {
                        for j in 0..r.offs.len() {
                            if a[k][j] && !b[k][j] {
                                let p = r.base + r.offs[j] as usize;
                                // SAFETY: as above.
                                let h = unsafe { &*(p as *const ObjectHeader) };
                                let e = by.entry(h.class_id.as_u32()).or_default();
                                e.0 += 1;
                                e.1 += r.sizes[j] as usize;
                                total += r.sizes[j] as usize;
                            }
                        }
                    }
                    let mut v: Vec<_> = by.into_iter().collect();
                    v.sort_by(|x, y| y.1 .1.cmp(&x.1 .1));
                    let top: Vec<String> = v
                        .iter()
                        .take(12)
                        .map(|(c, (n, b))| {
                            format!(
                                "{}x{n}={b}",
                                cratonvm_gc_core::collector::class_name_for_diagnostics_in(domain, *c)
                            )
                        })
                        .collect();
                    (total, top.join(","))
                };
                // `CRATONVM_G1_DBG_IN_PLACE_RETAIN_PATH=<class substring>`: the
                // shortest path from a root to the first two objects of a
                // matching class (breadth first over the compaction set).
                if let Ok(want) =
                    Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
                {
                    let name_of = |k: usize, j: usize| {
                        let p = objs[k].base + objs[k].offs[j] as usize;
                        // SAFETY: a parsed object of a compaction region, unmoved.
                        let h = unsafe { &*(p as *const ObjectHeader) };
                        cratonvm_gc_core::collector::class_name_for_diagnostics_in(domain, h.class_id.as_u32())
                    };
                    let rview = RegionView::Raw(base, regions_len);
                    let mut parent: std::collections::HashMap<
                        (usize, usize),
                        Option<(usize, usize)>,
                    > = std::collections::HashMap::new();
                    let mut queue: std::collections::VecDeque<(usize, usize)> =
                        std::collections::VecDeque::new();
                    let mut named = Vec::new();
                    for r in roots.iter() {
                        let addr = r.as_ptr() as usize;
                        held_named(addr, may_end(addr), &mut named);
                        if named.is_empty() {
                            if let Ok(Some(kj)) = find(addr) {
                                named.push(kj);
                            }
                        }
                        for kj in named.drain(..) {
                            if let std::collections::hash_map::Entry::Vacant(e) = parent.entry(kj) {
                                e.insert(None);
                                queue.push_back(kj);
                            }
                        }
                    }
                    let mut found = 0;
                    while let Some((k, j)) = queue.pop_front() {
                        if name_of(k, j).contains(want.as_str()) {
                            let mut chain = Vec::new();
                            let mut at = Some((k, j));
                            while let Some(kj) = at {
                                chain.push(format!(
                                    "{}@{:#x}",
                                    name_of(kj.0, kj.1),
                                    objs[kj.0].base + objs[kj.0].offs[kj.1] as usize
                                ));
                                at = parent[&kj];
                                if chain.len() > 40 {
                                    chain.push("...".into());
                                    break;
                                }
                            }
                            chain.reverse();
                            eprintln!("[g1][IN-PLACE-RETAIN-PATH] {}", chain.join(" -> "));
                            found += 1;
                            if found == 2 {
                                break;
                            }
                        }
                        let p = (objs[k].base + objs[k].offs[j] as usize) as *mut u8;
                        // SAFETY: as above.
                        let h = unsafe { &*(p as *const ObjectHeader) };
                        for_each_ref_raw(self, rview, p, h, |raw| {
                            if let Ok(Some(kj)) = find(raw) {
                                if let std::collections::hash_map::Entry::Vacant(e) =
                                    parent.entry(kj)
                                {
                                    e.insert(Some((k, j)));
                                    queue.push_back(kj);
                                }
                            }
                        });
                    }
                }
                let none: Vec<Vec<bool>> = objs.iter().map(|r| vec![false; r.offs.len()]).collect();
                let all = mark(1, true, true, None);
                let no_pins = mark(1, false, true, None);
                let no_holders = mark(1, true, false, None);
                let no_jit_roots = mark(2, false, true, None);
                let (live_all, live_top) = diff(&all, &none);
                let (by_jit_roots, jit_roots_top) = diff(&all, &no_jit_roots);
                let (by_pins, pins_top) = diff(&all, &no_pins);
                let (by_holders, holders_top) = diff(&all, &no_holders);
                let snap = cratonvm_gc_core::gc_quiescence::pinned_jit_roots_snapshot();
                let snap_in = |k: usize| {
                    snap.iter()
                        .filter(|&&a| self.lookup_region_for_addr(a) == Some(order[k]))
                        .count()
                };
                let held_desc: Vec<String> = (0..order.len())
                    .filter(|&k| held[k])
                    .map(|k| {
                        let r = &objs[k];
                        let (mut nf, mut bf, mut bm) = (0usize, 0usize, 0usize);
                        for j in 0..r.offs.len() {
                            if is_fixed(k, j) {
                                nf += 1;
                                bf += r.sizes[j] as usize;
                            } else if r.is_live(j) {
                                bm += r.sizes[j] as usize;
                            }
                        }
                        let hs = &holes_before_lift[k];
                        let total: usize = hs.iter().map(|&(a, b)| b - a).sum();
                        let max = hs.iter().map(|&(a, b)| b - a).max().unwrap_or(0);
                        let ge64k = hs.iter().filter(|&&(a, b)| b - a >= 65536).count();
                        let left: usize = holes[k].iter().map(|&(a, b)| b - a).sum();
                        format!(
                            "{}:{:?}/cur{}K fixed={nf}/{bf} packed={bm} pins={}/snap{} holes={}/{}K max={}K ge64k={ge64k} left={}K",
                            order[k],
                            regions[order[k]].region_type(),
                            regions[order[k]].cursor() >> 10,
                            pin_offs[k].len(),
                            snap_in(k),
                            hs.len(),
                            total >> 10,
                            max >> 10,
                            left >> 10
                        )
                    })
                    .collect();
                eprintln!(
                    "[g1][IN-PLACE-HELD] lift_bytes={lift_bytes} lifted_regions={} lifted_objects={} {}",
                    lifted.iter().filter(|&&l| l).count(),
                    lifted_obj.iter().flatten().filter(|&&l| l).count(),
                    held_desc.join(" ")
                );
                let src_desc: Vec<String> = sources
                    .iter()
                    .map(|&s| {
                        format!(
                            "{s}:{:?}/{}K/pin{}",
                            regions[s].region_type(),
                            regions[s].cursor() >> 10,
                            u8::from(regions[s].pinned || regions[s].pin_total() != 0)
                        )
                    })
                    .collect();
                let mut pin_desc: Vec<String> = Vec::new();
                for (n, addr) in cratonvm_gc_core::gc_quiescence::pinned_jit_roots_snapshot()
                    .into_iter()
                    .enumerate()
                {
                    let mut nm = Vec::new();
                    held_named(addr, true, &mut nm);
                    if nm.is_empty() {
                        continue;
                    }
                    let only = mark(0, false, false, Some(n));
                    let (own, own_top) = diff(&only, &no_pins);
                    let names: Vec<String> = nm
                        .iter()
                        .map(|&(k, j)| {
                            let r = &objs[k];
                            let p = r.base + r.offs[j] as usize;
                            // SAFETY: as above.
                            let h = unsafe { &*(p as *const ObjectHeader) };
                            format!(
                                "{}@{:#x}+{}/{}",
                                cratonvm_gc_core::collector::class_name_for_diagnostics_in(
                                    domain,
                                    h.class_id.as_u32()
                                ),
                                p,
                                addr.wrapping_sub(p),
                                r.sizes[j]
                            )
                        })
                        .collect();
                    pin_desc.push(format!(
                        "\n   pin {addr:#x} region={} names=[{}] own_retained={own} [{own_top}] prov={}",
                        self.lookup_region_for_addr(addr).unwrap_or(usize::MAX),
                        names.join(" "),
                        cratonvm_gc_core::gc_quiescence::lookup_jit_root_provenance(addr)
                            .unwrap_or_else(|| "-".into())
                    ));
                }
                eprintln!(
                    "[g1][IN-PLACE-RETAIN] free={} live={live_all} [{live_top}] roots={} jit_roots={} \
                     by_jit_roots={by_jit_roots} [{jit_roots_top}] by_pins={by_pins} [{pins_top}] \
                     by_holders={by_holders} [{holders_top}] sources=[{}] held_idx={:?}{}",
                    regions.iter().filter(|r| r.region_type() == RegionType::Free).count(),
                    roots.len(),
                    jit_root.iter().filter(|&&j| j).count(),
                    src_desc.join(" "),
                    order.iter().zip(held.iter()).filter(|&(_, &h)| h).map(|(&i, _)| i).collect::<Vec<_>>(),
                    pin_desc.join("")
                );
            }
            if dbg {
                let n_held = held.iter().filter(|&&h| h).count();
                let hole_bytes: usize = holes
                    .iter()
                    .flat_map(|hs| hs.iter().map(|&(s, e)| e - s))
                    .sum();
                let held_tops: Vec<usize> =
                    order.iter().zip(held.iter()).filter(|&(_, &h)| h).map(|(&i, _)| i).collect();
                IN_PLACE_DBG_LEFT.lock().push_str(&format!(
                    " regions={} held={n_held} held_idx={held_tops:?} live={live_bytes_total} \
                     moved={moved_bytes} movable_used={used_before} held_holes={hole_bytes} \
                     lift_bytes={lift_bytes}",
                    order.len()
                ));
            }
            return Err("less-than-a-region-to-reclaim");
        }
        in_place_lap(&mut lap, 2);
        let mut pointer_map = cratonvm_types::PointerMap::new();
        if g1_full_pointer_map_sorted_enabled() {
            // g1ep: the sorted form, by the young pause's parallel region
            // bucket sort; every key is a live object of the compaction set
            // and appears once (a lifted object is not also slid).
            let pool = self.evac_pool();
            let run = |jobs: usize, job: &(dyn Fn(usize) + Sync)| {
                let next = AtomicUsize::new(0);
                let body = |_helper: usize| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= jobs {
                        break;
                    }
                    job(i);
                };
                pool.scope(jobs.saturating_sub(1).min(helpers), &body, || body(0));
            };
            // g1sz: the plan walks the compaction set in region order and each
            // region in address order, so without held slides or a lift the
            // pairs arrive already sorted and unique; the bucket sort was
            // 0.33-0.42 s of nine `G1epKeepRounds 10` full collections for an
            // order already present. One linear check decides.
            //
            // g1ep3: only for a pass that planned ONE chunk (a set under 128
            // regions, `plan_chunks`). Otherwise the chunks go to the bucket
            // sort as its sources, read where they lie: flattening them into
            // one vector for the check was a serial copy of every pair
            // (194-219 ms of the same nine collections, every one of which
            // lifted objects and so took the bucket sort anyway), and the sort
            // passes over an already ordered run cheaply.
            if chunk_pairs.len() == 1 && chunk_pairs[0].windows(2).all(|w| w[0].0 < w[1].0) {
                // g1ep4: the copy goes into a kept buffer, and the map hands
                // both of its buffers back when the VM drops it.
                let mut pairs = self.sorted_buffers.take_pairs(chunk_pairs[0].len());
                pairs.extend_from_slice(&chunk_pairs[0]);
                pointer_map = cratonvm_types::PointerMap::from_sorted_unique_par_in(
                    pairs,
                    workers,
                    &run,
                    Some(&self.sorted_buffers),
                );
            } else {
                let sources: Vec<&[(usize, usize)]> =
                    chunk_pairs.iter().map(Vec::as_slice).collect();
                pointer_map = sorted_pointer_map_by_region(
                    &sources,
                    self.arena_base,
                    self.region_shift,
                    regions_len,
                    workers,
                    &run,
                    Some(&self.sorted_buffers),
                );
            }
        } else {
            let pool = self.evac_pool();
            pointer_map.par_extend_pairs_with(&chunk_pairs, workers, |n, job| {
                if n > 1 {
                    let body = |helper: usize| job(helper + 1);
                    pool.scope((n - 1).min(helpers), &body, || job(0));
                } else {
                    (0..n).for_each(job);
                }
            });
        }

        in_place_lap(&mut lap, 3);
        // 4. Adjust everything outside C (and its rsets into C).
        for &i in &order {
            regions[i].rset.clear();
        }
        for root in roots.iter_mut() {
            if let Some(&new_addr) = pointer_map.get(&(root.as_ptr() as usize)) {
                // SAFETY: a planned destination of a live object.
                *root = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
        let _ = self.update_references_in_regions(&mut regions, &cset, &pointer_map, None);

        in_place_lap(&mut lap, 4);
        // 5. The lift's copies first, from sources nothing has overwritten yet
        // into holes nothing else takes. Then compact each chunk, in parallel,
        // ascending: each destination is at or below its source, and every
        // byte it overwrites belongs to an object of the same chunk already
        // moved (or lifted).
        if let Some(held_slides) = held_chunk.map(|c| &chunk_pairs[c]) {
            for &(old, new_addr) in held_slides {
                if old == new_addr {
                    continue;
                }
                let (k, j) = find(old).ok().flatten().expect("a held object");
                let size = objs[k].sizes[j] as usize;
                // SAFETY: inside one held region, ascending, each destination
                // at or below its source and clear of every fixed object;
                // `ptr::copy` is memmove.
                unsafe { std::ptr::copy(old as *const u8, new_addr as *mut u8, size) };
            }
        }
        if let Some(lift) = lift_chunk.map(|c| &chunk_pairs[c]) {
            for &(old, new_addr) in lift {
                let (k, j) = find(old).ok().flatten().expect("a lifted object");
                let size = objs[k].sizes[j] as usize;
                // SAFETY: a live object's span, and a hole's span of the same
                // size in a held region: disjoint, both inside the arena.
                unsafe {
                    std::ptr::copy_nonoverlapping(old as *const u8, new_addr as *mut u8, size)
                };
            }
        }
        {
            let next = AtomicUsize::new(0);
            let body = |_w: usize| loop {
                let c = next.fetch_add(1, Ordering::Relaxed);
                if c >= slide_chunks {
                    break;
                }
                for &(old, new_addr) in &chunk_pairs[c] {
                    if old == new_addr {
                        continue;
                    }
                    let (k, j) = find(old).ok().flatten().expect("a planned object");
                    let size = objs[k].sizes[j] as usize;
                    // SAFETY: both spans are inside one chunk of the arena,
                    // ascending; `ptr::copy` is memmove.
                    unsafe { std::ptr::copy(old as *const u8, new_addr as *mut u8, size) };
                }
            };
            self.evac_pool().scope(helpers, &body, || body(helpers));
        }
        // The held regions' space no object occupies now -- their dead
        // objects, less what slid into them -- becomes fillers, up to the
        // larger of the old cursor and the new fill, before any walk below
        // reads the region again.
        if held.iter().any(|&h| h) {
            // Every object whose destination is a held region: its fixed
            // objects (identity forwards), and what slid or was lifted there,
            // its own unnamed objects included (which may not have moved).
            let mut occupied: Vec<Vec<(usize, usize)>> = vec![Vec::new(); order.len()];
            for pairs in &chunk_pairs {
                for &(old, new_addr) in pairs {
                    let Some(ridx) = self.lookup_region_for_addr(new_addr) else {
                        continue;
                    };
                    let k = pos[ridx];
                    if k == NOT_C || !held[k as usize] {
                        continue;
                    }
                    let (sk, sj) = find(old).ok().flatten().expect("a planned object");
                    let o = new_addr - objs[k as usize].base;
                    occupied[k as usize].push((o, o + objs[sk].sizes[sj] as usize));
                }
            }
            for (k, &i) in order.iter().enumerate() {
                if !held[k] {
                    continue;
                }
                let spans = &mut occupied[k];
                spans.sort_unstable();
                let end = regions[i].cursor().max(fills[k]);
                let base_i = regions[i].data.addr();
                let mut at = 0usize;
                for &(s0, s1) in spans.iter().chain(std::iter::once(&(end, end))) {
                    if s0 > at {
                        // SAFETY: `[at, s0)` lies under the region's new end,
                        // 8-aligned, between occupied spans: dead bytes no
                        // live or slid object holds.
                        unsafe { cratonvm_gc_core::heap::write_filler(base_i + at, s0 - at) };
                    }
                    at = at.max(s1);
                }
            }
        }
        let generation = self.rset_generation();
        let mut freed_regions = 0usize;
        for (k, &i) in order.iter().enumerate() {
            if held[k] {
                regions[i].held_in_place(fills[k], generation);
                continue;
            }
            if fills[k] == 0 {
                if regions[i].region_type() == RegionType::Old {
                    self.old_regions_reclaimed.fetch_add(1, Ordering::Relaxed);
                }
                regions[i].reset(generation);
                freed_regions += 1;
            } else {
                regions[i].compacted_in_place(fills[k], generation);
            }
        }

        in_place_lap(&mut lap, 5);
        // 6. Adjust the compacted objects themselves, in parallel, re-deriving
        // their outgoing edges and cards.
        {
            let compacted: Vec<usize> = order
                .iter()
                .zip(fills.iter())
                .filter(|&(_, &f)| f > 0)
                .map(|(&i, _)| i)
                .collect();
            let results: Vec<Mutex<Vec<(usize, usize)>>> =
                (0..compacted.len()).map(|_| Mutex::new(Vec::new())).collect();
            let empty = RegionSet::new();
            let holder_screen = true;
            let next = AtomicUsize::new(0);
            let table_after = RegionsBase(regions.as_mut_ptr());
            let body = |_w: usize| {
                let table: RegionsBase = table_after;
                // SAFETY: as above; regions are only read, and each region's
                // objects are written by the one worker that claimed it.
                let view: &[G1Region] =
                    unsafe { std::slice::from_raw_parts(table.0 as *const G1Region, regions_len) };
                let forwards = ForwardLookup {
                    collector: self,
                    cset: &cset,
                    map: &pointer_map,
                    regions: RegionView::Raw(table, regions_len),
                };
                let mut seen = RegionSet::new();
                loop {
                    let c = next.fetch_add(1, Ordering::Relaxed);
                    if c >= compacted.len() {
                        break;
                    }
                    let mut edges = Vec::new();
                    let _ = self.fixup_walk_region(
                        view,
                        compacted[c],
                        &empty,
                        &forwards,
                        true,
                        &[],
                        false,
                        holder_screen,
                        &mut edges,
                        &mut seen,
                        None,
                        false,
                    );
                    *results[c].lock() = edges;
                }
            };
            self.evac_pool().scope(helpers, &body, || body(helpers));
            for slot in results {
                for (target, source) in slot.into_inner() {
                    regions[target]
                        .rset
                        .add_reference_in_generation(source, generation);
                }
            }
        }

        // The VM's post-pause verdicts: in these ranges, a non-key address
        // held no live object (see `last_pause_slid`).
        *self.last_pause_slid.lock() = order
            .iter()
            .map(|&i| {
                let base = regions[i].data.addr();
                (base, base + region_size)
            })
            .collect();

        // The evacuation pauses' epilogue, for this pause's moves.
        self.publish_region_census(&regions);
        self.recompute_old_gen_bytes(&regions);
        self.publish_occupancy_metrics(&regions);
        self.overlay_rset_invalidate();
        self.end_mixed_phase("in-place full compaction");
        // The V7b verifier's rule (a slot into a collection-set region is a
        // forwarding key) is an evacuation's: here the compacted regions are
        // destinations too, so only the regions the slide emptied are asked.
        let emptied: RegionSet = order
            .iter()
            .zip(fills.iter())
            .filter(|&(_, &f)| f == 0)
            .map(|(&i, _)| i)
            .collect();
        self.verify_no_dangling_into_cset(&regions, &emptied, &pointer_map, roots);
        monitors.remap_after_gc(&pointer_map);
        self.remap_reference_skip_set(&cset, &pointer_map);
        self.remap_overlay_snapshot(&regions, &cset, &pointer_map, false);
        self.remap_string_dedup_table(&cset, &pointer_map);
        self.remap_gray_set_after_pause(&regions, &cset, &pointer_map);
        drop(regions);

        let reclaimed = used_before.saturating_sub(live_bytes_total);
        IN_PLACE_COMPACTIONS.fetch_add(1, Ordering::Relaxed);
        IN_PLACE_RECLAIMED_BYTES.fetch_add(reclaimed as u64, Ordering::Relaxed);
        IN_PLACE_MOVED_BYTES.fetch_add(moved_bytes as u64, Ordering::Relaxed);
        IN_PLACE_FREED_REGIONS.fetch_add(freed_regions as u64, Ordering::Relaxed);
        IN_PLACE_LIFTED_REGIONS.fetch_add(lifted_regions as u64, Ordering::Relaxed);
        IN_PLACE_LIFTED_OBJECTS.fetch_add(lifted_objects as u64, Ordering::Relaxed);
        if dbg {
            IN_PLACE_DBG_LEFT
                .lock()
                .push_str(&format!(
                    " lifted={lifted_regions} lifted_objects={lifted_objects} lift_bytes={lift_bytes}"
                ));
        }
        in_place_lap(&mut lap, 6);
        let pause_us = start.elapsed().as_micros() as u64;
        let stats = GcStats {
            objects_copied: pointer_map.len(),
            bytes_copied: moved_bytes,
            bytes_freed: reclaimed,
        };
        let mut phases = G1PausePhases::default();
        phases.compaction = true;
        phases.close(pause_us);
        self.record_collection_with_phases(G1CollectionType::Mixed, pause_us, &stats, phases);
        Ok(GcResult { stats, pointer_map })
    }

    /// The decline path's census (see [`g1_in_place_compaction_census`]).
    pub(super) fn note_in_place_compaction_declined(&self, why: &'static str) {
        note_declined(why);
    }
}

impl G1Region {
    /// A HELD region's bookkeeping after the pass
    /// (`CRATONVM_G1_IN_PLACE_HOLD_PINNED`): its objects stay where they are,
    /// it is Old afterwards, its cursor grows to `fill` when objects slid into
    /// its tail; its mark data and cards are reset as a compacted region's
    /// are, and its outgoing edges are re-derived by the pass's adjust walk.
    ///
    /// A new incarnation (`reuse_epoch`), as a compacted region gets: the
    /// pass cleared its bitmap and may have slid or lifted objects into its
    /// holes, so no mark-start snapshot from before the pass describes it.
    /// With the old incarnation a held region that was Old already still
    /// matched the snapshot, and every object below its TAMS -- the ones it
    /// kept and the ones moved in -- read as unmarked: `MarkProvenDead` then
    /// proved them dead, and the low-space remembered-set walk
    /// (`low_space_rset_source_liveness`) skipped them as holders, so their
    /// references into a young pause's collection set were never updated.
    /// `Zs3Nd3UpcallMints proxy 4096 -Xmx64m` logged
    /// `g1::for_each_object_reference: corrupt Value cell` in a third of its
    /// runs once the in-place pass ran routinely (none on the build before the
    /// lift, whose passes nearly all declined).
    fn held_in_place(&mut self, fill: usize, generation: u64) {
        if fill > self.cursor() {
            self.set_cursor(fill);
        }
        // A held Eden or Survivor region is Old afterwards, as an evacuation
        // failure's kept region is: a full collection leaves no young region,
        // and Old objects may have slid into its holes. No completed mark's
        // liveness describes it.
        if matches!(self.region_type(), RegionType::Eden | RegionType::Survivor) {
            self.set_region_type(RegionType::Old);
        }
        self.live_bytes = 0;
        self.gc_efficiency = 0.0;
        self.parallel_copy_end.store(0, Ordering::Release);
        self.cursor_at_cleanup = 0;
        self.cards
            .clear_range(self.data.as_ptr() as usize, self.data.len());
        self.reuse_epoch = self.reuse_epoch.wrapping_add(1);
        self.set_recycled_in_generation(generation);
        self.mark_bitmap.clear_exclusive();
        self.marked_bytes_below_tams.store(0, Ordering::Relaxed);
    }

    /// An Old region the in-place compaction slid `fill` bytes of live
    /// objects into (see the module doc): cursor `fill`, no mark data, its
    /// cards clean (the adjust walk re-dirties its holders), its own rset
    /// already rebuilt by the caller, and a new recycle stamp, so every rset
    /// entry naming it as a source from before the slide is dead (`reset`'s
    /// rule; the adjust walk re-records the live ones in `generation`).
    fn compacted_in_place(&mut self, fill: usize, generation: u64) {
        // A compacted Survivor region holds Old objects now (and may have
        // taken some from lower Old regions' slide).
        self.set_region_type(RegionType::Old);
        self.set_cursor(fill);
        self.parallel_copy_end.store(0, Ordering::Release);
        self.live_bytes = fill;
        self.gc_efficiency = fill as f64 / self.data.len().max(1) as f64;
        self.cursor_at_cleanup = 0;
        self.cards
            .clear_range(self.data.as_ptr() as usize, self.data.len());
        self.reuse_epoch = self.reuse_epoch.wrapping_add(1);
        self.set_recycled_in_generation(generation);
        self.mark_bitmap.clear_exclusive();
        self.marked_bytes_below_tams.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests;

/// Scale measurement (not run by default): the in-place pass over a
/// `R23W6KeepLong`-shaped 1 GB heap -- 64 chains of nodes holding a 224-byte
/// payload, the previous "round" dead. `cargo test --release -p cratonvm-gc
/// --lib in_place_scale -- --ignored --nocapture`.
#[cfg(test)]
mod scale;
