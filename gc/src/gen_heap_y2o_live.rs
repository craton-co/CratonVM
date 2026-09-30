// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gen r5w6/conc10 (2026-09-27) — the young-to-old seeds of the generational
//! CONCURRENT cycle's initial mark and remark, taken from the young objects
//! that are actually live (`CRATONVM_GEN_Y2O_LIVE_SEED`, opt-in).
//!
//! Page: `docs/internal/gc/gengc-r5w6-conc10-young-to-old-seeding-keeps-dead-old-objects-FIXED-20260929.md`.
//!
//! # What was wrong
//!
//! The concurrent old-generation marker never traces a young object, so both
//! of its pauses seed the old mark with every young object's reference into
//! the old generation (`GenerationalHeap::collect_young_to_old_roots`). That
//! walk takes EVERY parsable young object, dead or alive: an old object that
//! only a dead young object still names is marked, and so is everything it
//! reaches. The measured victim is a dead class loader
//! (`GenR5W5ConcUnloadProbe`: `roots=young_to_old+young_instance_loaders` at
//! every remark, 63 remarks, never unloaded), and a second shape is the
//! referent of a `java.lang.ref.Reference` that is itself YOUNG: its slot 0
//! is a young→old edge like any other, so its OLD referent is seeded
//! strongly and the remark's reference processing can never find it dead.
//! A `Reference` held in a static stays young for good under the JIT (the
//! non-moving young sweep pins every root value against promotion).
//!
//! # What this computes instead
//!
//! [`GenerationalHeap::collect_young_to_old_seeds`], two refinements, each on
//! its own input:
//!
//! 1. **Seed from the live young set** (`live_only`). The young set is the
//!    YOUNG COLLECTOR'S OWN live set for the same root set: the same marker
//!    (`scan_young_object`, `mark_edge_precise`: fields, overlay owners,
//!    `loader_pin` / `mirror_pin` / `metadata_pin` of scanned owners), the same
//!    old-owner side-table seeds (`seed_mirror_pins_of_old_owners`,
//!    `seed_metadata_pins_of_old_owners`, the overlay edges of old owners,
//!    `seed_loaders_of_old_instances`), and — in place of the dirty-card seed —
//!    the young referents of EVERY old object's reference slots (minor-GC
//!    semantics: the old generation is treated as live; the full scan is the
//!    `CRATONVM_GC_FULL_RSET_SCAN` oracle, so no barrier or card-table
//!    completeness is assumed). The caller adds its extra live objects
//!    (`extra_live`: the finalizable objects and the JNI weak-global referents,
//!    see below). Conservative roots are resolved against the young object
//!    grid (base or interior → the containing object).
//! 2. **Hide the referent slot of a young `Reference`** (`hidden_young_refs`):
//!    slot 0 of each listed object is not a seed when it names an OLD object.
//!    A YOUNG referent is still followed (the concurrent cycle never clears a
//!    reference to a young object). The caller lists only active
//!    weak / soft / phantom rows of the reference processor, shape- and
//!    identity-checked, and only on a cycle that processes references at its
//!    remark — the one-commit rule of `ConcurrentMarker::set_reference_skip`.
//!
//! # Soundness (why no live old object is dropped)
//!
//! * The live set is a superset of what the young collector would keep for
//!   the same roots, because it is that marker with a WIDER old→young seed
//!   (every old object instead of the dirty cards). A young object this set
//!   misses is one the next young collection would free, so nothing can reach
//!   it — except by resurrection through a table that is not a root. The
//!   one such table the VM has for young objects is the JNI weak globals,
//!   whose referents the caller passes as live (`extra_live`); the
//!   finalizable objects are passed too, because the next young collection
//!   RESURRECTS a dead one and `finalize()` then reads its fields.
//! * Every other old→young→old path is covered by the old-owner seeds and
//!   the full old scan; every young→old edge of a live young object is a seed,
//!   as before.
//! * Any doubt falls back to the all-young enumeration, unchanged: a young
//!   walk anomaly (the grid could not be proved complete), an old-generation
//!   walk that does not cover `used()` (the old scan would miss a holder), or a
//!   conservative candidate that resolves to no object but names a side-table
//!   or overlay OWNER. [`YoungToOldSeeds::fallback`] says which.
//!
//! What the argument does NOT cover, and why this stays opt-in: a VM table
//! that is not a root yet can hand a young object back to a mutator (a
//! resurrection vector other than the JNI weak globals). None is known; the
//! inventory is the flip gate on the page.
//!
//! # Cost
//!
//! One young grid walk (the all-young path walks young too), one old walk
//! plus a read of every old reference slot, and a closure over the live young
//! set — inside the pause, only when the flag is on.

use rustc_hash::{FxHashMap, FxHashSet};

use super::{
    active_narrow_geometry, for_each_conservative_ref_slot, for_each_ref_slot, scan_young_object,
    seed_loaders_of_old_instances, seed_metadata_pins_of_old_owners,
    seed_mirror_pins_of_old_owners, unreached_young_pinned_loaders, GenerationalHeap, GridStep,
    YoungGridCursor, YoungMarkCtx,
};
use crate::heap::{ObjectHeader, ObjectKind};
use crate::is_compact_object;
use crate::old_gen::OldGen;
use crate::young_mark::YoungMarkBits;

/// The switch (declared; `types/src/flag_groups.rs`, token `gen-y2o-live-seed`).
pub const Y2O_LIVE_SEED_FLAG: &str = "CRATONVM_GEN_Y2O_LIVE_SEED";

/// Is `CRATONVM_GEN_Y2O_LIVE_SEED` on? Read per pause by the driver (one
/// declared-flag lookup per concurrent pause).
pub fn y2o_live_seed_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on(Y2O_LIVE_SEED_FLAG)
}

/// What [`GenerationalHeap::collect_young_to_old_seeds`] is asked for.
#[derive(Debug, Clone, Copy)]
pub struct YoungToOldSeedRequest<'a> {
    /// The pause's root values, precise and conservative alike (addresses).
    pub roots: &'a [usize],
    /// Young objects the caller knows are live although no root names them:
    /// the finalizable objects and the JNI weak-global referents.
    pub extra_live: &'a [usize],
    /// Young `Reference` objects whose slot 0 must not seed an OLD referent.
    pub hidden_young_refs: &'a [usize],
    /// Seed from the live young set (else every parsable young object).
    pub live_only: bool,
}

/// Why a `live_only` request fell back to the all-young enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Y2oFallback {
    /// The young grid walk met an anomaly (or stopped early), so the grid the
    /// conservative roots are resolved against is not proved complete.
    YoungWalkAnomaly,
    /// The old-generation walk did not cover `used()`, so the full old→young
    /// scan could miss a holder.
    OldWalkIncomplete,
    /// A candidate resolved to no young object but names a side-table or
    /// overlay owner (a zero-headed owner the grid steps over).
    UnresolvedOwnerCandidate,
}

/// The seeds, and what produced them.
#[derive(Debug, Default)]
pub struct YoungToOldSeeds {
    /// Old-generation addresses named by the seeding young objects.
    pub old_refs: Vec<usize>,
    /// Class ids of the seeding young objects (the concurrent class unload's
    /// `young_instance_loaders` input).
    pub class_ids: FxHashSet<u32>,
    /// `true` when the seeds came from the live young set.
    pub from_live_set: bool,
    /// Set when a `live_only` request fell back.
    pub fallback: Option<Y2oFallback>,
    /// Young objects enumerated.
    pub young_objects: usize,
    /// Young objects that seeded (every enumerated one on the all-young path).
    pub seeding_objects: usize,
    /// Old referents NOT seeded because their slot 0 was hidden.
    pub hidden_referents_skipped: usize,
    /// `false` when the all-young walk stepped over an unparseable stretch:
    /// its words were seeded (conservatively), but the classes of the objects
    /// inside it are NOT in [`Self::class_ids`], so a caller turning classes
    /// into live loaders must keep every loader instead. Always `true` on the
    /// live path (its grid is complete or it falls back).
    pub young_walk_complete: bool,
}

/// The key `for_each_ref_slot` passes for `Reference.referent` (field 0) of
/// the object `header` heads: the slot INDEX of a legacy object, the byte
/// DISPLACEMENT of a compact one (the displacement `ConcurrentMarker`'s
/// hidden-slot test uses), `None` for an array or a compact object whose field
/// 0 is not a reference.
fn referent_slot_key(header: &ObjectHeader) -> Option<usize> {
    if header.kind() == ObjectKind::Array {
        return None;
    }
    if is_compact_object(header) {
        crate::heap::with_compact_oop_scan(header, |layout, _body| {
            if layout.is_ref.first() == Some(&true) {
                layout.field_disps.first().map(|&d| d as usize)
            } else {
                None
            }
        })
        .flatten()
    } else {
        Some(0)
    }
}

/// gen r5w6/conc10 — the young→old seeds of the from-space stretches the
/// all-young walk could NOT parse (`GenerationalHeap::walk_young_objects_and_gaps`):
/// every aligned word — and, under compressed oops, every decoded half — that
/// names an address inside the old generation. Returns how many it added.
///
/// # Why (`docs/internal/gc/gengc-r5w6-conc10-concurrent-young-to-old-seeding-drops-unparseable-young-stretches-FIXED-20260928.md`)
///
/// The walk re-anchors at the next free block after an anomaly (a zero span,
/// an implausible header) and so DROPS every real object in between from its
/// enumeration. The stop-the-world seed (`mark_young_to_old_refs`) has always
/// word-scanned such a stretch; the concurrent cycle's seed did not, so an old
/// object named only by a live young object inside the stretch was never
/// marked, and the concurrent sweep freed it under that object. Over-seeding
/// is safe: the concurrent marker admits only exact object starts
/// (`markable_old_object`).
pub(super) fn seed_old_words_of_young_stretches(
    stretches: &[(usize, usize)],
    old: &OldGen,
    out: &mut Vec<usize>,
) -> usize {
    if stretches.is_empty() {
        return 0;
    }
    let before = out.len();
    let narrow = active_narrow_geometry();
    for &(lo, hi) in stretches {
        // SAFETY: each stretch is an absolute `[lo, hi)` inside the mapped
        // from-space the walk just read (`hi <= base + used`), and the world
        // is stopped. `base = 0` makes the scanner's offsets absolute.
        unsafe {
            for_each_conservative_ref_slot(0, lo, hi, narrow, |_slot, _width, value| {
                if value != 0 && value & 7 == 0 && old.contains(value as *const u8) {
                    out.push(value);
                }
            });
        }
    }
    let seeded = out.len() - before;
    tracing::debug!(
        target: "cratonvm::gc",
        stretches = stretches.len(),
        seeded,
        "concurrent young->old seeding word-scanned unparseable young stretches",
    );
    seeded
}

/// The grid object `[base, base + size)` containing `addr`, if any.
fn grid_object_containing(grid: &[(usize, usize)], addr: usize) -> Option<usize> {
    let idx = grid.partition_point(|&(start, _)| start <= addr).checked_sub(1)?;
    let (start, size) = grid[idx];
    (addr < start + size).then_some(idx)
}

/// Seed the old refs of the young object at `obj` into `seeds`, skipping the
/// referent slot when `hide` names it and the referent is old.
///
/// # Safety
/// `obj` is a valid young object header readable for its whole extent, and
/// the world is stopped.
unsafe fn seed_from_young_object(
    obj: *mut u8,
    hide: bool,
    is_old: &dyn Fn(usize) -> bool,
    seeds: &mut YoungToOldSeeds,
) {
    // SAFETY: (caller contract) a valid, stable young object.
    let header = unsafe { &*(obj as *const ObjectHeader) };
    seeds.class_ids.insert(header.class_id.as_u32());
    let hidden_key = if hide { referent_slot_key(header) } else { None };
    let mut skipped = 0usize;
    let old_refs = &mut seeds.old_refs;
    // SAFETY: as above.
    unsafe {
        for_each_ref_slot(obj, header, |r, slot| {
            let value = r as usize;
            if is_old(value) {
                if hidden_key == Some(slot) {
                    skipped += 1;
                } else {
                    old_refs.push(value);
                }
            }
        });
    }
    seeds.hidden_referents_skipped += skipped;
}

impl GenerationalHeap {
    /// gen r5w6/conc10 — the concurrent cycle's young→old seeds (see the module
    /// doc). With `live_only == false` and no hidden reference this is exactly
    /// [`Self::collect_young_to_old_roots`] (same enumeration, same order),
    /// plus the class ids. Must be called inside a pause, with no heap lock
    /// held (it takes the young-from lock, then the old-gen lock).
    pub fn collect_young_to_old_seeds(&self, req: &YoungToOldSeedRequest<'_>) -> YoungToOldSeeds {
        let hidden: FxHashSet<usize> = req.hidden_young_refs.iter().copied().collect();
        let mut fallback = None;
        if req.live_only {
            match self.young_to_old_seeds_from_live_set(req, &hidden) {
                Ok(seeds) => return seeds,
                Err(why) => fallback = Some(why),
            }
        }
        // The all-young enumeration, as `collect_young_to_old_roots` walks it
        // (its unparseable stretches word-scanned the same way).
        let (young_objs, stretches) = self.walk_young_objects_and_gaps();
        let old = self.old_gen.lock();
        let is_old = |a: usize| old.contains(a as *const u8);
        let mut seeds = YoungToOldSeeds {
            fallback,
            young_objects: young_objs.len(),
            seeding_objects: young_objs.len(),
            young_walk_complete: stretches.is_empty(),
            ..YoungToOldSeeds::default()
        };
        for (ptr, _size) in young_objs {
            let hide = !hidden.is_empty() && hidden.contains(&(ptr as usize));
            // SAFETY: `ptr` came from the hardened young walk; the world is
            // stopped, so its header and body are stable.
            unsafe { seed_from_young_object(ptr, hide, &is_old, &mut seeds) };
        }
        let _ = seed_old_words_of_young_stretches(&stretches, &old, &mut seeds.old_refs);
        seeds
    }

    /// The `live_only` arm of [`Self::collect_young_to_old_seeds`], or why it
    /// declined.
    fn young_to_old_seeds_from_live_set(
        &self,
        req: &YoungToOldSeedRequest<'_>,
        hidden: &FxHashSet<usize>,
    ) -> Result<YoungToOldSeeds, Y2oFallback> {
        self.with_live_young_mark(req.roots, req.extra_live, &|_| true, |live| {
            let is_old = |a: usize| live.old.contains(a as *const u8);
            // The live grid objects: every mark resolved to the object
            // containing it (the marker's precise edges can in principle land
            // off-grid; the containing object is kept, the conservative answer).
            let marked = live.marked_grid_objects();
            let mut seeds = YoungToOldSeeds {
                from_live_set: true,
                young_objects: live.grid.len(),
                young_walk_complete: true,
                ..YoungToOldSeeds::default()
            };
            for (idx, &(obj, _size)) in live.grid.iter().enumerate() {
                if !marked[idx] {
                    continue;
                }
                seeds.seeding_objects += 1;
                let hide = !hidden.is_empty() && hidden.contains(&obj);
                // SAFETY: a grid object of mapped from-space; the world is
                // stopped.
                unsafe { seed_from_young_object(obj as *mut u8, hide, &is_old, &mut seeds) };
            }
            seeds
        })
    }

    /// gcd d1/c (2026-09-27) — the YOUNG objects reachable from the old
    /// objects `through` but not from the pause's strong live set: what a
    /// concurrent remark's dead-finalizer retention keeps alive only for
    /// `finalize()`
    /// (`docs/internal/gc/gengc-r5w6-conc10-remark-weak-refs-to-young-objects-behind-a-retained-finalizable-FIXED-20260928.md`).
    ///
    /// The strong young set is [`Self::collect_young_to_old_seeds`]' live set
    /// for `roots` and `extra_live`, with the old→young seed (the scan of every
    /// old object's reference slots) restricted to the old holders
    /// `strong_holder` accepts; the caller passes "marked by the remark and not
    /// in the retention closure", and lists the retention closure as
    /// `through`. The young referents of `through` are then traced by the same
    /// young marker; what that newly marks is the answer (object starts, in
    /// grid order).
    ///
    /// Every other old-owner seed (overlay edges, mirror / metadata pins, the
    /// loaders of old instances) is taken from EVERY old owner, and a young
    /// object the retention closure reaches through one of those is not
    /// reported: both make the strong set larger or the answer smaller, which
    /// only keeps a weak reference the collector could have cleared (the
    /// behaviour before this call existed). An answer never names a young
    /// object the strong set holds, so a caller that clears references to it
    /// clears nothing that is strongly reachable.
    ///
    /// `Err` is [`Self::collect_young_to_old_seeds`]' fallback reason: the
    /// caller then reports nothing. Must be called inside a pause, with no heap
    /// lock held (it takes the young-from lock, then the old-gen lock).
    pub fn young_reached_only_through(
        &self,
        roots: &[usize],
        extra_live: &[usize],
        strong_holder: &dyn Fn(usize) -> bool,
        through: &[usize],
    ) -> Result<Vec<usize>, Y2oFallback> {
        if through.is_empty() {
            return Ok(Vec::new());
        }
        self.with_live_young_mark(roots, extra_live, strong_holder, |live| {
            let strong = live.marked_grid_objects();
            let mut work: Vec<usize> = Vec::new();
            for &holder in through {
                if holder & 7 != 0 || !live.old.contains(holder as *const u8) {
                    continue;
                }
                // SAFETY: an 8-aligned old-generation object start the
                // remark's marker recorded (it scanned it); the world is
                // stopped and the old-gen lock is held.
                let oh = unsafe { &*(holder as *const ObjectHeader) };
                // SAFETY: as above.
                unsafe {
                    for_each_ref_slot(holder as *mut u8, oh, |r, _slot| {
                        let _ = mark_grid_object(live.grid, live.bits, r as usize, &mut work);
                    });
                }
            }
            while let Some(addr) = work.pop() {
                scan_young_object(addr, live.ctx, live.bits, &mut work);
            }
            let after = live.marked_grid_objects();
            live.grid
                .iter()
                .enumerate()
                .filter(|&(idx, _)| after[idx] && !strong[idx])
                .map(|(_, &(obj, _size))| obj)
                .collect()
        })
    }

    /// The young collector's live marking of from-space for `roots` and
    /// `extra_live` (see the module doc), with the old→young slot seed taken
    /// only from the old objects `strong_holder` accepts; `then` runs on the
    /// result with the young-from and old-gen locks still held. `Err` when the
    /// marking cannot be proved complete.
    fn with_live_young_mark<R>(
        &self,
        roots: &[usize],
        extra_live: &[usize],
        strong_holder: &dyn Fn(usize) -> bool,
        then: impl FnOnce(&LiveYoungMark<'_>) -> R,
    ) -> Result<R, Y2oFallback> {
        let young = self.lock_young_from();
        let base = young.base_ptr() as usize;
        let used = young.used();
        let end = base + used;
        // The free list plus the reserved TLAB tails of frozen in-JIT peers,
        // as every linear from-space walk merges them (offsets, ascending).
        let skips: Vec<(usize, usize)> = {
            let mut v = young.free_blocks_sorted();
            v.extend(self.jit_tlab_skip_offsets(base, end));
            v.sort_by_key(|&(off, _)| off);
            v
        };
        // The grid, all or nothing: an anomaly means some object is not in it.
        let mut grid: Vec<(usize, usize)> = Vec::new();
        let complete = {
            let mut cursor = YoungGridCursor::new(base, used, &skips, &[]);
            for step in cursor.by_ref() {
                if let GridStep::Object { off, size } = step {
                    grid.push((base + off, size));
                }
            }
            cursor.anomalies == 0 && !cursor.truncated
        };
        if !complete {
            return Err(Y2oFallback::YoungWalkAnomaly);
        }
        let old = self.old_gen.lock();
        let (walked, gaps) = old.walk_objects_with_gaps();
        let walked_bytes: usize = walked.iter().map(|&(_, size)| size).sum();
        if !gaps.is_empty() || walked_bytes != old.used() {
            return Err(Y2oFallback::OldWalkIncomplete);
        }
        let is_old = |a: usize| old.contains(a as *const u8);

        // The young collector's marker context, exactly as
        // `sweep_young_non_moving` builds it.
        let loader_pin_on = cratonvm_types::loader_pin::loader_pinning_enabled();
        let ctx = YoungMarkCtx {
            from_base: base,
            from_end: end,
            loader_pin_on,
            overlay_owners: crate::external_roots::external_owner_addrs()
                .map(|owners| owners.into_iter().collect()),
            metadata_pins: cratonvm_types::metadata_pin::snapshot(),
            mark_why: crate::heap::young_mark_watch(),
            loader_pins: if loader_pin_on {
                cratonvm_types::loader_pin::snapshot_where(|a| a >= base && a < end)
            } else {
                None
            },
            mirror_pins: cratonvm_types::mirror_pin::snapshot(),
        };
        let is_owner = |a: usize| -> bool {
            ctx.overlay_owners.as_ref().is_some_and(|o| o.contains(&a))
                || ctx.mirror_pins.as_ref().is_some_and(|m| m.contains_key(&a))
                || ctx.metadata_pins.as_ref().is_some_and(|m| m.contains_key(&a))
                || ctx
                    .loader_pins
                    .as_ref()
                    .is_some_and(|m: &FxHashMap<u32, usize>| m.values().any(|&l| l == a))
        };
        let bits = YoungMarkBits::new(base, used);
        let mut work: Vec<usize> = Vec::new();
        // A candidate (root word, old slot value, overlay edge): the grid
        // object containing it, marked. Outside every object it names nothing
        // with a field (a free / skip block, a GAP filler, a run of empty
        // objects) — unless it is a side-table owner, which only a zero-headed
        // object in an empty run could be; that case is refused, not guessed.
        let mut unresolved_owner = false;
        let mut seed = |addr: usize, work: &mut Vec<usize>| {
            if !mark_grid_object(&grid, &bits, addr, work)
                && addr >= base
                && addr < end
                && (is_owner(addr) || is_owner(addr & !7))
            {
                unresolved_owner = true;
            }
        };
        for &r in roots.iter().chain(extra_live) {
            seed(r, &mut work);
        }
        // Minor-GC semantics for the old generation, widened from the dirty
        // cards to every old object (see the module doc). gcd d1/c: only the
        // holders `strong_holder` accepts (every one, for the seeds).
        for &(optr, _size) in &walked {
            if !strong_holder(optr as usize) {
                continue;
            }
            // SAFETY: `walk_objects_with_gaps` yields initialised old-gen
            // object starts; the world is stopped and the old-gen lock held.
            let oh = unsafe { &*(optr as *const ObjectHeader) };
            // SAFETY: as above.
            unsafe {
                for_each_ref_slot(optr, oh, |r, _slot| seed(r as usize, &mut work));
            }
        }
        for overlay_ref in
            crate::external_roots::external_roots_for_matching_owners(&|owner_addr| {
                is_old(owner_addr)
            })
        {
            seed(overlay_ref.as_ptr() as usize, &mut work);
        }
        if unresolved_owner {
            return Err(Y2oFallback::UnresolvedOwnerCandidate);
        }
        let _ = seed_mirror_pins_of_old_owners(&ctx, &is_old, &bits, &mut work);
        let _ = seed_metadata_pins_of_old_owners(&ctx, &is_old, &bits, &mut work);
        while let Some(addr) = work.pop() {
            scan_young_object(addr, &ctx, &bits, &mut work);
        }
        // A young loader whose only live instances are old (the young
        // collector's `seed_loaders_of_old_instances` step).
        let unreached = unreached_young_pinned_loaders(&ctx, &bits);
        if !unreached.is_empty() {
            let old_class_ids = walked.iter().map(|&(optr, _)| {
                // SAFETY: an old-gen object start from the walk above.
                unsafe { (*(optr as *const ObjectHeader)).class_id.as_u32() }
            });
            let _ = seed_loaders_of_old_instances(&ctx, unreached, old_class_ids, &bits, &mut work);
            while let Some(addr) = work.pop() {
                scan_young_object(addr, &ctx, &bits, &mut work);
            }
        }
        Ok(then(&LiveYoungMark {
            grid: &grid,
            bits: &bits,
            ctx: &ctx,
            old: &*old,
        }))
    }
}

/// What [`GenerationalHeap::with_live_young_mark`] hands its continuation: the
/// from-space object grid, the young marks, the marker context and the old
/// generation, under the locks the marking took.
struct LiveYoungMark<'a> {
    grid: &'a [(usize, usize)],
    bits: &'a YoungMarkBits,
    ctx: &'a YoungMarkCtx,
    old: &'a OldGen,
}

impl LiveYoungMark<'_> {
    /// Per grid object: does it hold a mark? Every mark is resolved to the
    /// object containing it (the marker's precise edges can in principle land
    /// off-grid; the containing object is kept, the conservative answer).
    fn marked_grid_objects(&self) -> Vec<bool> {
        let mut marked = vec![false; self.grid.len()];
        for addr in self.bits.collect_marked() {
            if let Some(idx) = grid_object_containing(self.grid, addr) {
                marked[idx] = true;
            }
        }
        marked
    }
}

/// Mark the grid object containing `addr` (a young from-space address) and
/// queue it if the mark is new. Returns whether `addr` resolved to a grid
/// object; an address outside from-space resolves to nothing.
fn mark_grid_object(
    grid: &[(usize, usize)],
    bits: &YoungMarkBits,
    addr: usize,
    work: &mut Vec<usize>,
) -> bool {
    match grid_object_containing(grid, addr) {
        Some(idx) => {
            let obj = grid[idx].0;
            if bits.try_mark(obj) {
                work.push(obj);
            }
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::{ArrayElementType, GC_FLAG_OLD_GEN, HEADER_SIZE, SLOT_SIZE};
    use cratonvm_types::{ClassId, ObjectRef, Value};

    /// A heap with room for a handful of objects in each generation.
    fn heap() -> GenerationalHeap {
        GenerationalHeap::with_sizes(64 * 1024, 64 * 1024)
    }

    /// An old-generation object of `slots` legacy reference cells, allocated
    /// directly (as selective promotion would place it).
    fn old_object(heap: &GenerationalHeap, class: u32, slots: u32) -> ObjectRef {
        let size = HEADER_SIZE + slots as usize * SLOT_SIZE;
        let mut og = heap.old_gen_lock();
        let p = og.alloc(size, 8).expect("old-gen room");
        // SAFETY: a freshly allocated (zeroed) old-gen block of `size` bytes.
        unsafe {
            let h = &mut *(p as *mut ObjectHeader);
            h.class_id = ClassId::new(class);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(slots);
            h.set_gc_flags(GC_FLAG_OLD_GEN);
            ObjectRef::from_raw(p)
        }
    }

    fn addr(r: ObjectRef) -> usize {
        r.as_ptr() as usize
    }

    fn request<'a>(
        roots: &'a [usize],
        hidden: &'a [usize],
        live_only: bool,
    ) -> YoungToOldSeedRequest<'a> {
        YoungToOldSeedRequest {
            roots,
            extra_live: &[],
            hidden_young_refs: hidden,
            live_only,
        }
    }

    /// THE PAGE'S CLAIM: a dead young object's old referent is not seeded, a
    /// live one's is — and the all-young enumeration (the default, and the
    /// fallback) seeds both, exactly as `collect_young_to_old_roots` does.
    #[test]
    fn a_dead_young_objects_old_referent_is_not_seeded_and_a_live_ones_is() {
        let heap = heap();
        let dead_target = old_object(&heap, 2, 0);
        let live_target = old_object(&heap, 2, 0);
        let live = heap.alloc_object(ClassId::new(1), 1);
        let dead = heap.alloc_object(ClassId::new(1), 1);
        heap.set_field(live, 0, Value::Object(Some(live_target)));
        heap.set_field(dead, 0, Value::Object(Some(dead_target)));

        let roots = [addr(live)];
        let seeds = heap.collect_young_to_old_seeds(&request(&roots, &[], true));
        assert!(seeds.from_live_set, "fell back: {:?}", seeds.fallback);
        assert!(seeds.old_refs.contains(&addr(live_target)), "the live holder seeds");
        assert!(
            !seeds.old_refs.contains(&addr(dead_target)),
            "a young object nothing reaches must not keep its old referent"
        );
        assert_eq!(seeds.seeding_objects, 1);

        let all = heap.collect_young_to_old_seeds(&request(&roots, &[], false));
        assert!(!all.from_live_set);
        assert!(all.old_refs.contains(&addr(live_target)));
        assert!(all.old_refs.contains(&addr(dead_target)));
        let mut legacy = heap.collect_young_to_old_roots();
        let mut ours = all.old_refs.clone();
        legacy.sort_unstable();
        ours.sort_unstable();
        assert_eq!(ours, legacy, "flag off: the same seeds as before");
    }

    /// A conservative INTERIOR root keeps the object it points into, and the
    /// young objects that one reaches keep their old referents too.
    #[test]
    fn an_interior_root_and_its_young_closure_seed() {
        let heap = heap();
        let target = old_object(&heap, 2, 0);
        let inner = heap.alloc_object(ClassId::new(1), 1);
        let outer = heap.alloc_object(ClassId::new(1), 2);
        heap.set_field(inner, 0, Value::Object(Some(target)));
        heap.set_field(outer, 1, Value::Object(Some(inner)));

        let roots = [addr(outer) + HEADER_SIZE + 8];
        let seeds = heap.collect_young_to_old_seeds(&request(&roots, &[], true));
        assert!(seeds.from_live_set, "fell back: {:?}", seeds.fallback);
        assert!(seeds.old_refs.contains(&addr(target)));
    }

    /// Minor-GC semantics for the old generation: a young object whose only
    /// holder is OLD is live (every old object is scanned, not only the dirty
    /// cards), and so its own old referent is seeded; `extra_live` (the
    /// finalizable objects, the JNI weak referents) seeds like a root.
    #[test]
    fn a_young_object_held_from_old_or_passed_as_extra_live_seeds() {
        let heap = heap();
        let holder = old_object(&heap, 2, 1);
        let target_a = old_object(&heap, 2, 0);
        let target_b = old_object(&heap, 2, 0);
        let via_old = heap.alloc_object(ClassId::new(1), 1);
        let finalizable = heap.alloc_object(ClassId::new(1), 1);
        heap.set_field(via_old, 0, Value::Object(Some(target_a)));
        heap.set_field(finalizable, 0, Value::Object(Some(target_b)));
        heap.set_field(holder, 0, Value::Object(Some(via_old)));

        let extra = [addr(finalizable)];
        let req = YoungToOldSeedRequest {
            roots: &[],
            extra_live: &extra,
            hidden_young_refs: &[],
            live_only: true,
        };
        let seeds = heap.collect_young_to_old_seeds(&req);
        assert!(seeds.from_live_set, "fell back: {:?}", seeds.fallback);
        assert!(seeds.old_refs.contains(&addr(target_a)), "held from an old object");
        assert!(seeds.old_refs.contains(&addr(target_b)), "an extra live object");
    }

    /// The hidden referent slot: slot 0 of a listed young object does not
    /// seed its OLD referent (on both paths), every other slot does, and a
    /// YOUNG referent in slot 0 is still followed.
    #[test]
    fn a_hidden_young_reference_does_not_seed_its_old_referent() {
        let heap = heap();
        let referent = old_object(&heap, 2, 0);
        let queue = old_object(&heap, 2, 0);
        let reference = heap.alloc_object(ClassId::new(1), 2);
        heap.set_field(reference, 0, Value::Object(Some(referent)));
        heap.set_field(reference, 1, Value::Object(Some(queue)));

        let roots = [addr(reference)];
        let hidden = [addr(reference)];
        for live_only in [true, false] {
            let seeds = heap.collect_young_to_old_seeds(&request(&roots, &hidden, live_only));
            assert!(!seeds.old_refs.contains(&addr(referent)), "live_only={live_only}");
            assert!(seeds.old_refs.contains(&addr(queue)), "live_only={live_only}");
            assert_eq!(seeds.hidden_referents_skipped, 1);
            let open = heap.collect_young_to_old_seeds(&request(&roots, &[], live_only));
            assert!(open.old_refs.contains(&addr(referent)), "not listed: seeded");
        }

        // A young referent is followed through the hidden slot: its own old
        // referent is seeded on the live path.
        let far = old_object(&heap, 2, 0);
        let young_referent = heap.alloc_object(ClassId::new(1), 1);
        heap.set_field(young_referent, 0, Value::Object(Some(far)));
        heap.set_field(reference, 0, Value::Object(Some(young_referent)));
        let seeds = heap.collect_young_to_old_seeds(&request(&roots, &hidden, true));
        assert!(seeds.from_live_set, "fell back: {:?}", seeds.fallback);
        assert!(seeds.old_refs.contains(&addr(far)));
        assert_eq!(seeds.hidden_referents_skipped, 0);
    }

    /// THE DEFAULT-PATH FIX: a young object the walk cannot parse (here an
    /// implausible `num_slots`) is dropped from the enumeration together with
    /// everything up to the next free block. `collect_young_to_old_roots`
    /// (every concurrent pause, flag or not) now word-scans that stretch, as
    /// the stop-the-world seed always did, so the old object only that young
    /// holder names is still seeded; the live path declines (its grid is not
    /// complete) and says so.
    #[test]
    fn the_concurrent_seed_word_scans_an_unparseable_young_stretch() {
        let heap = heap();
        let target = old_object(&heap, 2, 0);
        let holder = heap.alloc_object(ClassId::new(1), 1);
        heap.set_field(holder, 0, Value::Object(Some(target)));
        let _after = heap.alloc_object(ClassId::new(1), 0);
        // SAFETY: `holder` is a live young object; only its slot count is
        // rewritten, to a value every walk rejects as implausible.
        unsafe { (*(holder.as_ptr() as *mut ObjectHeader)).set_num_slots(1 << 30) };

        let (objs, stretches) = heap.walk_young_objects_and_gaps();
        assert!(!stretches.is_empty(), "precondition: the walk met an anomaly");
        assert!(
            !objs.iter().any(|&(p, _)| p == holder.as_ptr()),
            "precondition: the unparseable holder is not enumerated"
        );
        assert!(
            heap.collect_young_to_old_roots().contains(&addr(target)),
            "the stretch's words must seed the old object its holder names"
        );

        let roots = [addr(holder)];
        let seeds = heap.collect_young_to_old_seeds(&request(&roots, &[], true));
        assert!(!seeds.from_live_set);
        assert_eq!(seeds.fallback, Some(Y2oFallback::YoungWalkAnomaly));
        assert!(!seeds.young_walk_complete, "the class ids are incomplete");
        assert!(seeds.old_refs.contains(&addr(target)));
    }

    /// gcd d1/c (`gengc-r5w6-conc10-remark-weak-refs-to-young-objects-behind-a-retained-finalizable`):
    /// an old "finalizable" F whose slot names young R, R naming young R2, and
    /// nothing else naming either: with F excluded from the strong holders, R
    /// and R2 are reported as reachable only through F; a young object that a
    /// root (or a strong old holder) also reaches is not; with F a strong
    /// holder nothing is reported.
    #[test]
    fn gcd_d1c_young_objects_behind_a_retained_holder_are_reported() {
        let heap = heap();
        let fin = old_object(&heap, 2, 3);
        let strong_old = old_object(&heap, 2, 1);
        let r = heap.alloc_object(ClassId::new(1), 1);
        let r2 = heap.alloc_object(ClassId::new(1), 0);
        let shared_with_root = heap.alloc_object(ClassId::new(1), 0);
        let shared_with_old = heap.alloc_object(ClassId::new(1), 0);
        heap.set_field(r, 0, Value::Object(Some(r2)));
        heap.set_field(fin, 0, Value::Object(Some(r)));
        heap.set_field(fin, 1, Value::Object(Some(shared_with_root)));
        heap.set_field(fin, 2, Value::Object(Some(shared_with_old)));
        heap.set_field(strong_old, 0, Value::Object(Some(shared_with_old)));

        let roots = [addr(shared_with_root)];
        let through = [addr(fin)];
        let not_fin = |a: usize| a != addr(fin);
        let mut only = heap
            .young_reached_only_through(&roots, &[], &not_fin, &through)
            .expect("the live marking is complete");
        only.sort_unstable();
        let mut want = vec![addr(r), addr(r2)];
        want.sort_unstable();
        assert_eq!(only, want);

        let every = |_: usize| true;
        let none = heap
            .young_reached_only_through(&roots, &[], &every, &through)
            .expect("complete");
        assert!(none.is_empty(), "{none:x?}");
        let nothing = heap
            .young_reached_only_through(&roots, &[], &not_fin, &[])
            .expect("complete");
        assert!(nothing.is_empty());
    }

    /// The class ids come from the seeding objects only on the live path (the
    /// concurrent class unload's `young_instance_loaders`).
    #[test]
    fn the_class_ids_are_the_live_objects_classes() {
        let heap = heap();
        let live = heap.alloc_object(ClassId::new(11), 0);
        let _dead = heap.alloc_object(ClassId::new(12), 0);
        let roots = [addr(live)];
        let seeds = heap.collect_young_to_old_seeds(&request(&roots, &[], true));
        assert!(seeds.from_live_set, "fell back: {:?}", seeds.fallback);
        assert!(seeds.class_ids.contains(&11));
        assert!(!seeds.class_ids.contains(&12));
        let all = heap.collect_young_to_old_seeds(&request(&roots, &[], false));
        assert!(all.class_ids.contains(&11) && all.class_ids.contains(&12));
    }
}
