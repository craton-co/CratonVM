// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! zt3gn S2.4, slice 1 (t1 wave 6 lane Y4): **young-live evacuation as a
//! NURSERY-SCOPED slide** -- `CRATONVM_ZGC_GEN_YOUNG_EVACUATE`, default OFF.
//!
//! # What a young slide cost before this
//!
//! `CRATONVM_ZGC_GEN_YOUNG_RELOCATE=1` lets a young cycle run the ordinary
//! stop-the-world slide (`ZgcRealHeap::relocate_stw_admitted_hinted`), and the
//! slide is a WHOLE-LIVE-SET pass: the live list, the page census and the
//! reference rewrite all walk every survivor, old ones included. On a young
//! cycle that is the one O(heap) cost the generational split cannot bound,
//! which is why the young slide has been default OFF since zr z2/g2.
//!
//! # The scope
//!
//! A young cycle may move only objects whose referrers it can name without
//! walking the old generation. Those are YOUNG objects (header age below the
//! promotion age, old-start bit clear): every heap slot naming one is in
//!
//! 1. another young survivor (young->young edges are never carded), or
//! 2. an old object that is CARDED (the card barrier records every old->young
//!    store, and the card at promotion covers the edges an object wrote while
//!    young -- the same completeness the young MARK already relies on), or
//! 3. a root, or a VM-side table remapped through the slide's `PointerMap`
//!    exactly as for a whole-heap slide.
//!
//! So the scope is: the logical pages at and above `lo` (the first page
//! boundary at or above the young floor) that hold NO old-start bit
//! ([`ZYoungEvacScope::blocked_pages`]), with the slide's live list cut to the
//! registered bases in `[lo, low cursor)`. The rewrite then visits that list
//! (the slide's own pass) plus [`ZYoungEvacScope::extra`]: every young
//! survivor outside `[lo, low cursor)` and every carded object. Nothing else
//! can hold an edge into a moved object, so nothing else is walked.
//!
//! # What this slice does NOT do (the rest of S2.4)
//!
//! It runs AFTER the young sweep, which still visits every dead nursery
//! object (the complement sweep's per-dead-bit loop). It therefore makes the
//! young slide O(young live), not the young cycle: the cycle-level saving
//! needs slice 2 (evacuate BEFORE the sweep and clear the evacuated span's
//! registry bits in bulk), written on the zt3gn page under "t1 wave 6".
//!
//! Pure helpers only: the heap-side builder is `ZgcRealHeap::young_evac_scope`
//! and the slide-side hooks are in `relocate_stw_admitted_scoped`.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// ---- refusal codes (`young_evac_last_refusal=`) ----------------------------

/// The cycle is not a bitmap-pre-marked, young-aged-sweep cycle: without the
/// old-start bitmap "young" cannot be enumerated by words, and without the
/// aged sweep dead young objects below the floor stay registered.
pub(crate) const YE_REFUSE_OLD_BITS: usize = 1;
/// The registry is the `Hash` arm, has spilled bases, or its geometry differs
/// from the old-start bitmap's.
pub(crate) const YE_REFUSE_REGISTRY: usize = 2;
/// `CRATONVM_ZGC_GEN_ELEMENT_CARDS=1`: a granule card names a slot, not an
/// object, so the carded set is not a list of objects to rewrite.
pub(crate) const YE_REFUSE_ELEMENT_CARDS: usize = 3;
/// An evacuating slide arm (`CRATONVM_ZGC_SLIDE_EVACUATE`,
/// `CRATONVM_ZGC_REGION_RELOCATE`) is on: its destinations and quarantine are
/// not scoped.
pub(crate) const YE_REFUSE_EVACUATING_SLIDE: usize = 4;
/// This cycle's card scan rooted a coarsened page or skipped a baseless one:
/// the carded set does not name every old holder of a young reference.
pub(crate) const YE_REFUSE_REMSET_INCOMPLETE: usize = 5;
/// The floor is outside the arena, or no page boundary lies between the floor
/// and the low cursor (nothing to scope).
pub(crate) const YE_REFUSE_FLOOR: usize = 6;
/// The object straddling `lo` from below has no knowable extent.
pub(crate) const YE_REFUSE_STRADDLER: usize = 7;
/// The scoped slide ran but selected no page: every admitted page was
/// withheld by the pin screen, or none was a candidate (t1 wave 6 handback;
/// before it, the selective cycle gate declined here SILENTLY).
pub(crate) const YE_DECLINED_NOTHING_SELECTED: usize = 8;
/// The scoped slide selected pages but moved nothing (no-move decline, every
/// survivor already in place, or an unsizable survivor stopped it).
pub(crate) const YE_DECLINED_NOTHING_MOVED: usize = 9;

/// One young cycle's evacuation scope. Built at the slide point, after the
/// sweep, under the stop-the-world token.
#[derive(Debug, Default)]
pub(crate) struct ZYoungEvacScope {
    /// First logical-page boundary at or above the young floor (absolute).
    pub(crate) lo: usize,
    /// `lo`'s logical page id (arena-base grid).
    pub(crate) lo_page: u64,
    /// The low cursor (absolute) the scope was cut at.
    pub(crate) low_end: usize,
    /// Pages at or above `lo_page` that must not be selected: an old-start
    /// bit on them, or the tail of an object straddling `lo`. Sorted, unique.
    pub(crate) blocked_pages: Vec<u64>,
    /// Registered bases in `[lo, low_end)`, ascending: the slide's live list.
    pub(crate) live: Vec<usize>,
    /// Registered bases OUTSIDE `[lo, low_end)` the rewrite must visit: young
    /// survivors and carded objects. Sorted, unique, disjoint from `live`.
    pub(crate) extra: Vec<usize>,
}

impl ZYoungEvacScope {
    /// May the slide select logical page `page_id`?
    #[inline]
    pub(crate) fn admits_page(&self, page_id: u64) -> bool {
        page_id >= self.lo_page && self.blocked_pages.binary_search(&page_id).is_err()
    }
}

/// Logical page id of `addr` on the arena-base grid.
#[inline]
pub(crate) fn page_of(addr: usize, base: usize, page_bytes: usize) -> u64 {
    (addr.saturating_sub(base) / page_bytes.max(1)) as u64
}

/// The first page boundary (arena-base grid) at or above `floor`, or `None`
/// when the floor lies below the arena base or the boundary is not below
/// `low_end` (nothing to scope).
pub(crate) fn scope_low_bound(
    floor: usize,
    base: usize,
    low_end: usize,
    page_bytes: usize,
) -> Option<usize> {
    if floor < base || page_bytes == 0 {
        return None;
    }
    let lo = base.checked_add(
        (floor - base)
            .div_ceil(page_bytes)
            .checked_mul(page_bytes)?,
    )?;
    (lo < low_end).then_some(lo)
}

/// Pages an object `[obj_lo, obj_hi)` that starts BELOW `lo` occupies at or
/// above `lo` -- the slide's obstacle screen only knows objects in its live
/// list, so these pages are withheld instead.
pub(crate) fn push_straddler_pages(
    obj_lo: usize,
    obj_hi: usize,
    lo: usize,
    base: usize,
    page_bytes: usize,
    out: &mut Vec<u64>,
) {
    if obj_lo >= lo || obj_hi <= lo {
        return;
    }
    let first = page_of(lo, base, page_bytes);
    let last = page_of(obj_hi - 1, base, page_bytes);
    out.extend(first..=last);
}

/// Walk words `[w0, w1)` of two bitmaps on the same grid. Every base in
/// `[lo, hi)` whose start bit is set: with `want_old`, push its PAGE when its
/// old bit is set (into `pages`); otherwise push the BASE when its old bit is
/// clear (into `bases`). `word_base(w)` is the address of word `w`'s bit 0;
/// bit `k` names `word_base(w) + 8k`.
#[allow(clippy::too_many_arguments)]
fn walk_words(
    w0: usize,
    w1: usize,
    word_base: &dyn Fn(usize) -> usize,
    start: &dyn Fn(usize) -> u64,
    old: &dyn Fn(usize) -> u64,
    lo: usize,
    hi: usize,
    want_old: bool,
    mut on_base: impl FnMut(usize),
) {
    for w in w0..w1 {
        let s = start(w);
        if s == 0 {
            continue;
        }
        // gkr: only the bases of the wanted kind are visited. Below the
        // scope floor nearly every base is old, and visiting each one to
        // drop it was the whole of the scope build's young walk (~9 of
        // ~10 ms a scoped slide on `Zr1GnGenShape`, 176.57).
        let o = old(w);
        let mut bits = if want_old { s & o } else { s & !o };
        if bits == 0 {
            continue;
        }
        let wb = word_base(w);
        while bits != 0 {
            let b = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let addr = wb + (b << 3);
            if addr < lo || addr >= hi {
                continue;
            }
            on_base(addr);
        }
    }
}

/// Young bases (start bit set, old bit clear) in `[lo, hi)`, ascending.
#[allow(clippy::too_many_arguments)]
pub(crate) fn young_bases_in_words(
    w0: usize,
    w1: usize,
    word_base: &dyn Fn(usize) -> usize,
    start: &dyn Fn(usize) -> u64,
    old: &dyn Fn(usize) -> u64,
    lo: usize,
    hi: usize,
    out: &mut Vec<usize>,
) {
    walk_words(w0, w1, word_base, start, old, lo, hi, false, |addr| out.push(addr));
}

/// Pages holding an old-start base in `[lo, hi)` (may repeat; see
/// [`finish_blocked`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn old_start_pages(
    w0: usize,
    w1: usize,
    word_base: &dyn Fn(usize) -> usize,
    start: &dyn Fn(usize) -> u64,
    old: &dyn Fn(usize) -> u64,
    lo: usize,
    hi: usize,
    base: usize,
    page_bytes: usize,
    out: &mut Vec<u64>,
) {
    walk_words(w0, w1, word_base, start, old, lo, hi, true, |addr| {
        let p = page_of(addr, base, page_bytes);
        if out.last() != Some(&p) {
            out.push(p);
        }
    });
}

/// Sort and dedup a blocked-page list.
pub(crate) fn finish_blocked(pages: &mut Vec<u64>) {
    pages.sort_unstable();
    pages.dedup();
}

/// The rewrite's extra set: `young` plus `carded`, minus anything in
/// `[lo, hi)` (the slide's own live list), sorted and unique -- each slot must
/// be rewritten EXACTLY once, because a destination may be another object's
/// source and a second pass would chase it one hop too far.
pub(crate) fn merge_extra(
    mut young: Vec<usize>,
    carded: impl IntoIterator<Item = usize>,
    lo: usize,
    hi: usize,
) -> Vec<usize> {
    young.extend(carded);
    young.retain(|b| *b < lo || *b >= hi);
    young.sort_unstable();
    young.dedup();
    young
}

/// `CRATONVM_ZGC_GEN_YOUNG_BULK_DEAD` -- zt3gn S2.4 slice 2 step 6 (t1 wave
/// 8 lane ZT3GN): may the young complement sweep take a bitmap word's dead
/// nursery bits WHOLE (popcount + one mask) when nothing per object is owed
/// for them (no dead list, dead-header skipping on, the word below the
/// large-object end)? The counts and registry bits are the per-bit loop's.
/// **Default ON inside generational ZGC** since t1 2026-10-27 (`=0` kills;
/// WARNING t1-user-flip-20261027): flips11 K arm vs a generational baseline, x5.
pub(crate) fn zgc_gen_young_bulk_dead() -> bool {
    // WARNING t1-user-flip-20261027: default ON, `=0` kills.
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_GEN_YOUNG_BULK_DEAD")
}

/// `CRATONVM_ZGC_GEN_YOUNG_EVACUATE_ALL_PAGES` -- zt3gn S2.4 slice 2 step 1
/// (t1 wave 8 lane ZT3GN): does a nursery-scoped slide take EVERY admitted
/// page as its relocation set, as a HotSpot young collection does?
///
/// **Default ON inside the opt-in `CRATONVM_ZGC_GEN_YOUNG_EVACUATE` (`=0`
/// kill).** Without it the slide asked the whole-heap selector, whose 25 %
/// live-occupancy cutoff and evacuation budget left a few dense pages (the
/// newest survivors, the partial top page) unselected; one unmoved survivor
/// near the top pinned the cursor, the slide reclaimed 0 bytes, the G2 floor
/// advance then marched the floor to that cursor every young cycle, and once
/// the cursor met the arena's end the nursery was allocated from free-list
/// holes BELOW the floor, which the per-object pass (1) sweeps one header at
/// a time (t1 wave 8 rows: `pass1_us` 150-200 ms per minor on `Zr1GnGenShape`).
pub(crate) fn zgc_gen_young_evacuate_all_pages() -> bool {
    !matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_ZGC_GEN_YOUNG_EVACUATE_ALL_PAGES"),
        Ok(v) if cratonvm_types::flags::value_is_off(&v)
    )
}

/// `CRATONVM_ZGC_GEN_PRUNE_KEYS` -- zt3gn S2.4 slice 2 step 5 (t1 wave 8
/// lane ZT3GN): may a GENERATIONAL cycle use the monitor-cleanup key probe
/// (`CRATONVM_ZGC_SWEEP_LIVE_PROPORTIONAL`'s `prune_keys`) instead of a dead
/// list? One inflated monitor anywhere makes `want_dead` true, and then every
/// young sweep pushes one address per dead nursery object (3.5M per minor on
/// `Zr1GnGenShape`) and cannot take a word's dead bits whole. The probe's
/// answer ("registered before the sweep, unregistered after it") is the same
/// set on a young cycle: both sweep passes unregister exactly what they free,
/// and old objects stay registered. **Default ON inside generational ZGC** since
/// t1 2026-10-27 (`=0` kills; WARNING t1-user-flip-20261027).
pub(crate) fn zgc_gen_prune_keys() -> bool {
    // WARNING t1-user-flip-20261027: default ON, `=0` kills.
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_GEN_PRUNE_KEYS")
}

/// `CRATONVM_ZGC_GEN_YOUNG_EVACUATE_FLOOR_HOLD` -- t1 wave 8 lane ZT3GN:
/// does a nursery-scoped slide leave the young floor where it was, instead of
/// the G2 advance to the post-slide cursor? (A conservatively pinned survivor
/// on the newest page usually keeps that cursor at the top.) Default ON
/// inside the opt-in scoped slide (`=0` kill). See the `floor_hold` site in
/// `collect_garbage`.
pub(crate) fn zgc_gen_young_evacuate_floor_hold() -> bool {
    !matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_ZGC_GEN_YOUNG_EVACUATE_FLOOR_HOLD"),
        Ok(v) if cratonvm_types::flags::value_is_off(&v)
    )
}

/// `CRATONVM_ZGC_GEN_YOUNG_EVACUATE_SCOPED_PINS` -- t1 wave 10 lane ZT3GN
/// (`t1-ZT3GN-scoped-young-slide-costs-more-than-it-saves-20261026.md`, fix
/// 1): on a nursery-scoped slide, the pin screen skips every pin word below
/// the scope floor (`lo` is a page boundary and every admitted page is at or
/// above it, so neither the word's page nor the page of an object based at or
/// below the word can be selected), and resolves an interior word against the
/// scope's own live list (`scope.live`, the registered bases in `[lo,
/// low_end)`) by one binary search, instead of the registry's backward bitmap
/// walk, which on a post-sweep nursery crosses every dead run between the word
/// and the next survivor below it. Over-pins at worst (an unsizable base is
/// taken as containing the word). Default ON inside the opt-in
/// `CRATONVM_ZGC_GEN_YOUNG_EVACUATE` (`=0` kill).
pub(crate) fn zgc_gen_young_evacuate_scoped_pins() -> bool {
    !matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_ZGC_GEN_YOUNG_EVACUATE_SCOPED_PINS"),
        Ok(v) if cratonvm_types::flags::value_is_off(&v)
    )
}

/// `CRATONVM_ZGC_GEN_YOUNG_EVACUATE_LOCAL_INSTALL` -- t1 wave 10 lane ZT3GN
/// (fix 2): a nursery-scoped slide installs its cursor and vacated spans with
/// `Arena::compact_nursery_to`, which leaves every free block wholly below the
/// scope floor where it is and edits only the blocks reaching into `[lo, old
/// cursor)`, instead of `Arena::compact_low_to`'s sort, clear and re-add of
/// the WHOLE low free list with an `O(blocks x vacated)` overlap test. The
/// per-page vacated spans are published merged (fix 3: `[dest, pinned page)`
/// is one block when a pin keeps the cursor at the top). Same blocks kept,
/// dropped and truncated as `compact_low_to` would (unit-tested against it).
/// Default ON inside the opt-in (`=0` kill).
pub(crate) fn zgc_gen_young_evacuate_local_install() -> bool {
    !matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_ZGC_GEN_YOUNG_EVACUATE_LOCAL_INSTALL"),
        Ok(v) if cratonvm_types::flags::value_is_off(&v)
    )
}

/// `CRATONVM_ZGC_GEN_YOUNG_EVACUATE_DEFER_VACATED_ZERO` -- t1 wave 10 lane
/// ZT3GN (fix 2, the memset half): with the local install, a scoped slide
/// does not memset the vacated spans it free-lists; the hand-out that next
/// takes those bytes zeroes them (`take_last_hand_out_zero_plan` /
/// `ZeroPlan::zero`), as it already does for the slide's tail under
/// `CRATONVM_ZGC_SLIDE_TAIL_ZERO_AT_ALLOCATION` and for every dead object's
/// body under the sweep's dead-header skip. When a pin holds the cursor at
/// the top, the vacated span is the whole emptied nursery, and the memset was
/// `O(nursery bytes)` inside the pause.
///
/// Sound on the dead-header skip's audit (`zgc_sweep_skip_dead_headers_enabled`,
/// zr z8/zb): every reader of arena bytes goes through the registry, which
/// the slide rebuilt without the vacated bases, and every free-list byte is
/// zeroed at hand-out. So it engages only where that skip does (it is off
/// under the stale-reference hunts and `CRATONVM_DBG_ZGC_CORPSE`). Default ON
/// inside the opt-in (`=0` kill).
pub(crate) fn zgc_gen_young_evacuate_defer_vacated_zero() -> bool {
    !matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_ZGC_GEN_YOUNG_EVACUATE_DEFER_VACATED_ZERO"),
        Ok(v) if cratonvm_types::flags::value_is_off(&v)
    )
}

/// The scoped pin screen's interior lookup (see
/// [`zgc_gen_young_evacuate_scoped_pins`]): the base in `live` (ascending)
/// of the object that contains `probe` or ends exactly at it, or `None`.
/// `size_of(base)` is the object's extent, `None` when unsizable -- taken as
/// containing the probe (over-pinning is the safe side).
pub(crate) fn scoped_pin_base(
    probe: usize,
    live: &[usize],
    size_of: impl Fn(usize) -> Option<usize>,
) -> Option<usize> {
    let i = live.partition_point(|&b| b <= probe);
    let b = *live.get(i.checked_sub(1)?)?;
    if b == probe {
        return Some(b);
    }
    match size_of(b) {
        // `<=`: one past the end still names the object, as in
        // `resolve_interior_for_pin`.
        Some(sz) => (probe <= b.saturating_add(sz)).then_some(b),
        None => Some(b),
    }
}

/// `CRATONVM_ZGC_GEN_YOUNG_BELOW_FLOOR_COMPLEMENT` -- t1 wave 8 lane ZT3GN:
/// may the young complement sweep's pass (1) on the aged arm free the dead
/// young objects BELOW the floor as complement windows (the bytes between
/// the live objects around each dead run) instead of one `sweep_one` per
/// dead object? Needs no dead list and dead-header skipping (else the
/// per-object pass runs). Default OFF (opt-in `=1`). See
/// `ZgcRealHeap::sweep_below_floor_windows`.
pub(crate) fn zgc_gen_young_below_floor_complement() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_ZGC_GEN_YOUNG_BELOW_FLOOR_COMPLEMENT")
}

/// Per-heap seed and engagement counters (lives in `ZgcCounters`, not in the
/// hot heap struct).
#[derive(Debug, Default)]
pub(crate) struct ZYoungEvacCounters {
    /// `CRATONVM_ZGC_GEN_YOUNG_EVACUATE`, seeded per heap.
    pub(crate) on: AtomicBool,
    /// Set by each young cycle's card scan: a coarsened or baseless page made
    /// the carded set incomplete this cycle.
    pub(crate) remset_incomplete: AtomicBool,
    /// t1 wave 8 lane ZT3GN (`CRATONVM_ZGC_GEN_YOUNG_BULK_DEAD`, slice 2 step
    /// 6): the young complement sweep takes a word's dead bits whole (one
    /// popcount) instead of one bit at a time. Seeded per heap.
    pub(crate) bulk_dead: AtomicBool,
    /// Bitmap words whose dead bits the young complement took whole
    /// (cumulative; the engagement counter of `bulk_dead`).
    pub(crate) bulk_dead_words: AtomicUsize,
    /// t1 wave 8 lane ZT3GN (`CRATONVM_ZGC_GEN_YOUNG_EVACUATE_ALL_PAGES`,
    /// slice 2 step 1): a scoped slide selects EVERY admitted page instead of
    /// the occupancy / budget selector's answer. Seeded per heap.
    pub(crate) all_pages: AtomicBool,
    /// t1 wave 8 lane ZT3GN (`CRATONVM_ZGC_GEN_PRUNE_KEYS`, slice 2 step 5):
    /// a generational cycle asks the monitor cleanup for its KEYS (the zr
    /// z5/sw key probe) instead of having the sweep list every dead base.
    /// Seeded per heap.
    pub(crate) prune_keys: AtomicBool,
    /// t1 wave 8 lane ZT3GN (`CRATONVM_ZGC_GEN_YOUNG_EVACUATE_FLOOR_HOLD`): a
    /// scoped slide leaves the young floor where it was. Seeded per heap; `floor_holds` counts the cycles it fired.
    pub(crate) floor_hold: AtomicBool,
    pub(crate) floor_holds: AtomicUsize,
    /// t1 wave 8 lane ZT3GN (`CRATONVM_ZGC_GEN_YOUNG_BELOW_FLOOR_COMPLEMENT`):
    /// the young complement's aged pass (1) sweeps below the floor as
    /// complement windows. Seeded per heap.
    pub(crate) below_floor_complement: AtomicBool,
    /// Complement windows pass (1) opened below the floor (cumulative).
    pub(crate) below_windows_total: AtomicUsize,
    /// t1 wave 10 lane ZT3GN (`CRATONVM_ZGC_GEN_YOUNG_EVACUATE_SCOPED_PINS`):
    /// the scoped pin screen. Seeded per heap; `pins_skipped` counts the pin
    /// words it skipped as below the scope floor (cumulative).
    pub(crate) scoped_pins: AtomicBool,
    pub(crate) pins_skipped: AtomicUsize,
    /// t1 wave 10 lane ZT3GN (`CRATONVM_ZGC_GEN_YOUNG_EVACUATE_LOCAL_INSTALL`):
    /// the nursery-local cursor install. Seeded per heap; `local_installs`
    /// counts the scoped slides that used it (cumulative).
    pub(crate) local_install: AtomicBool,
    pub(crate) local_installs: AtomicUsize,
    /// t1 wave 10 lane ZT3GN
    /// (`CRATONVM_ZGC_GEN_YOUNG_EVACUATE_DEFER_VACATED_ZERO`): seeded per heap;
    /// `deferred_zero_bytes` counts the vacated bytes free-listed without the
    /// memset (cumulative).
    pub(crate) defer_vacated_zero: AtomicBool,
    pub(crate) deferred_zero_bytes: AtomicUsize,
    scopes: AtomicUsize,
    refusals: AtomicUsize,
    last_refusal: AtomicUsize,
    live_scoped: AtomicUsize,
    extra_rewritten: AtomicUsize,
    blocked_pages: AtomicUsize,
    moved: AtomicUsize,
    cycles_moved: AtomicUsize,
}

/// A copy of [`ZYoungEvacCounters`]' cumulative figures.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ZYoungEvacReport {
    /// Young cycles whose slide ran scoped.
    pub(crate) scopes: usize,
    /// Young cycles that asked and were refused (no slide ran, codes 1-7),
    /// plus scoped slides that moved nothing (codes 8-9, also in `scopes`).
    pub(crate) refusals: usize,
    /// The last refusal code (`YE_REFUSE_*`), 0 = none yet.
    pub(crate) last_refusal: usize,
    /// Bases handed to scoped slides as their live list.
    pub(crate) live_scoped: usize,
    /// Bases rewritten outside the scope (young survivors + carded).
    pub(crate) extra_rewritten: usize,
    /// Pages withheld for an old object or a straddler.
    pub(crate) blocked_pages: usize,
    /// Objects the scoped slides moved, and the scoped slides that moved any.
    pub(crate) moved: usize,
    pub(crate) cycles_moved: usize,
}

impl ZYoungEvacCounters {
    pub(crate) fn new(on: bool) -> Self {
        let c = Self::default();
        c.on.store(on, Ordering::Relaxed);
        c.bulk_dead
            .store(zgc_gen_young_bulk_dead(), Ordering::Relaxed);
        c.all_pages
            .store(zgc_gen_young_evacuate_all_pages(), Ordering::Relaxed);
        c.prune_keys.store(zgc_gen_prune_keys(), Ordering::Relaxed);
        c.floor_hold
            .store(zgc_gen_young_evacuate_floor_hold(), Ordering::Relaxed);
        c.below_floor_complement
            .store(zgc_gen_young_below_floor_complement(), Ordering::Relaxed);
        c.scoped_pins
            .store(zgc_gen_young_evacuate_scoped_pins(), Ordering::Relaxed);
        c.local_install
            .store(zgc_gen_young_evacuate_local_install(), Ordering::Relaxed);
        c.defer_vacated_zero.store(
            zgc_gen_young_evacuate_defer_vacated_zero(),
            Ordering::Relaxed,
        );
        c
    }

    pub(crate) fn note_scope(&self, scope: &ZYoungEvacScope) {
        self.scopes.fetch_add(1, Ordering::Relaxed);
        self.live_scoped
            .fetch_add(scope.live.len(), Ordering::Relaxed);
        self.blocked_pages
            .fetch_add(scope.blocked_pages.len(), Ordering::Relaxed);
    }

    pub(crate) fn note_refusal(&self, code: usize) {
        self.refusals.fetch_add(1, Ordering::Relaxed);
        self.last_refusal.store(code, Ordering::Relaxed);
    }

    pub(crate) fn note_extra_rewritten(&self, n: usize) {
        self.extra_rewritten.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn note_moved(&self, n: usize) {
        if n > 0 {
            self.moved.fetch_add(n, Ordering::Relaxed);
            self.cycles_moved.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn report(&self) -> ZYoungEvacReport {
        ZYoungEvacReport {
            scopes: self.scopes.load(Ordering::Relaxed),
            refusals: self.refusals.load(Ordering::Relaxed),
            last_refusal: self.last_refusal.load(Ordering::Relaxed),
            live_scoped: self.live_scoped.load(Ordering::Relaxed),
            extra_rewritten: self.extra_rewritten.load(Ordering::Relaxed),
            blocked_pages: self.blocked_pages.load(Ordering::Relaxed),
            moved: self.moved.load(Ordering::Relaxed),
            cycles_moved: self.cycles_moved.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: usize = 2 * 1024 * 1024;
    const BASE: usize = 0x1000_0000;

    #[test]
    fn the_low_bound_rounds_the_floor_up_to_the_page_grid() {
        assert_eq!(
            scope_low_bound(BASE, BASE, BASE + 3 * PAGE, PAGE),
            Some(BASE)
        );
        assert_eq!(
            scope_low_bound(BASE + 1, BASE, BASE + 3 * PAGE, PAGE),
            Some(BASE + PAGE)
        );
        assert_eq!(
            scope_low_bound(BASE + PAGE, BASE, BASE + 3 * PAGE, PAGE),
            Some(BASE + PAGE)
        );
        // No boundary below the cursor: nothing to scope.
        assert_eq!(
            scope_low_bound(BASE + PAGE + 8, BASE, BASE + 2 * PAGE, PAGE),
            None
        );
        // A floor below the arena is not a floor.
        assert_eq!(scope_low_bound(BASE - 8, BASE, BASE + 3 * PAGE, PAGE), None);
    }

    #[test]
    fn a_page_is_admitted_only_at_or_above_lo_and_unblocked() {
        let s = ZYoungEvacScope {
            lo: BASE + 2 * PAGE,
            lo_page: 2,
            low_end: BASE + 6 * PAGE,
            blocked_pages: vec![3, 5],
            ..Default::default()
        };
        let admitted: Vec<u64> = (0..7).filter(|p| s.admits_page(*p)).collect();
        assert_eq!(admitted, vec![2, 4, 6]);
    }

    #[test]
    fn a_straddler_blocks_the_pages_it_reaches_above_lo() {
        let lo = BASE + 2 * PAGE;
        let mut out = Vec::new();
        // Ends exactly at lo: no page.
        push_straddler_pages(lo - 64, lo, lo, BASE, PAGE, &mut out);
        assert!(out.is_empty());
        // Starts at lo: not a straddler (it is in the live list).
        push_straddler_pages(lo, lo + 64, lo, BASE, PAGE, &mut out);
        assert!(out.is_empty());
        push_straddler_pages(lo - 64, lo + 64, lo, BASE, PAGE, &mut out);
        assert_eq!(out, vec![2]);
        out.clear();
        push_straddler_pages(lo - 64, lo + PAGE + 8, lo, BASE, PAGE, &mut out);
        assert_eq!(out, vec![2, 3]);
    }

    /// Two bitmaps over one grid: word `w` covers `BASE + 512w`, bit `k` the
    /// base `BASE + 512w + 8k`.
    fn grid(starts: &[usize], olds: &[usize], words: usize) -> (Vec<u64>, Vec<u64>) {
        let mut s = vec![0u64; words];
        let mut o = vec![0u64; words];
        for &a in starts {
            let off = (a - BASE) >> 3;
            s[off / 64] |= 1u64 << (off % 64);
        }
        for &a in olds {
            let off = (a - BASE) >> 3;
            o[off / 64] |= 1u64 << (off % 64);
        }
        (s, o)
    }

    #[test]
    fn the_word_walk_splits_young_bases_from_old_pages() {
        // Small pages so a few words span several of them.
        let page = 1024;
        let starts = [
            BASE,
            BASE + 8,
            BASE + 600,
            BASE + 1100,
            BASE + 2048,
            BASE + 3000,
        ];
        let olds = [BASE + 8, BASE + 1100, BASE + 3000];
        let (s, o) = grid(&starts, &olds, 8);
        let wb = |w: usize| BASE + (w << 9);
        let st = |w: usize| s[w];
        let ol = |w: usize| o[w];

        let mut young = Vec::new();
        young_bases_in_words(0, 8, &wb, &st, &ol, BASE, BASE + 4096, &mut young);
        assert_eq!(young, vec![BASE, BASE + 600, BASE + 2048]);

        // Bounded: [BASE + 512, BASE + 2048) holds one young base.
        let mut bounded = Vec::new();
        young_bases_in_words(0, 8, &wb, &st, &ol, BASE + 512, BASE + 2048, &mut bounded);
        assert_eq!(bounded, vec![BASE + 600]);

        let mut pages = Vec::new();
        old_start_pages(
            0,
            8,
            &wb,
            &st,
            &ol,
            BASE,
            BASE + 4096,
            BASE,
            page,
            &mut pages,
        );
        finish_blocked(&mut pages);
        // Old bases at offsets 8 (page 0), 1100 (page 1), 3000 (page 2).
        assert_eq!(pages, vec![0, 1, 2]);
    }

    /// t1 wave 10 lane ZT3GN: the scoped pin lookup answers what
    /// `resolve_interior_for_pin` answers for a base in the scope (exact hit,
    /// interior, one past the end), `None` for a word in a dead run or below
    /// the first base, and over-pins an unsizable base.
    #[test]
    fn the_scoped_pin_lookup_finds_the_containing_scope_base() {
        let live = [1000usize, 1100, 1300, 5000];
        // 1000: 64 bytes, 1100: 200 bytes, 1300: 16 bytes, 5000: unsizable.
        let size_of = |b: usize| match b {
            1000 => Some(64),
            1100 => Some(200),
            1300 => Some(16),
            _ => None,
        };
        assert_eq!(scoped_pin_base(999, &live, size_of), None);
        assert_eq!(scoped_pin_base(1000, &live, size_of), Some(1000));
        assert_eq!(scoped_pin_base(1063, &live, size_of), Some(1000));
        // One past the end names the object below.
        assert_eq!(scoped_pin_base(1064, &live, size_of), Some(1000));
        // A dead run between two survivors.
        assert_eq!(scoped_pin_base(1065, &live, size_of), None);
        assert_eq!(scoped_pin_base(1299, &live, size_of), Some(1100));
        assert_eq!(scoped_pin_base(1301, &live, size_of), Some(1300));
        assert_eq!(scoped_pin_base(1400, &live, size_of), None);
        // Unsizable: over-pin.
        assert_eq!(scoped_pin_base(90_000, &live, size_of), Some(5000));
        assert_eq!(scoped_pin_base(10, &[], size_of), None);
    }

    #[test]
    fn the_extra_set_is_unique_sorted_and_disjoint_from_the_scope() {
        let lo = 1000;
        let hi = 2000;
        let young = vec![900, 10, 2500, 1500];
        let carded = vec![10, 300, 1999, 2000, 2500];
        let extra = merge_extra(young, carded, lo, hi);
        assert_eq!(extra, vec![10, 300, 900, 2000, 2500]);
    }

    #[test]
    fn the_counters_book_scopes_refusals_and_moves() {
        let c = ZYoungEvacCounters::new(true);
        assert!(c.on.load(Ordering::Relaxed));
        let s = ZYoungEvacScope {
            live: vec![1, 2, 3],
            blocked_pages: vec![7],
            ..Default::default()
        };
        c.note_scope(&s);
        c.note_refusal(YE_REFUSE_FLOOR);
        c.note_extra_rewritten(4);
        c.note_moved(0);
        c.note_moved(5);
        let r = c.report();
        assert_eq!(
            r,
            ZYoungEvacReport {
                scopes: 1,
                refusals: 1,
                last_refusal: YE_REFUSE_FLOOR,
                live_scoped: 3,
                extra_rewritten: 4,
                blocked_pages: 1,
                moved: 5,
                cycles_moved: 1,
            }
        );
    }
}
