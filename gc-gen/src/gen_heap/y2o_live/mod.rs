// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! gen r5w6/conc10 (2026-09-27) — the young-to-old seeds of the generational
//! CONCURRENT cycle's initial mark and remark, taken from the young objects
//! that are actually live (`CRATONVM_GEN_Y2O_LIVE_SEED`; opt-in when written,
//! default ON since the gce close, 2026-09-29, `=0` restores the all-young
//! enumeration).
//!
//! Page: `docs/feature-designs/generative-gc-architecture.md`.
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
//!    the referent edge is excluded from the concurrent cycle's strong young
//!    closure, whether it names an old or young object. A young referent that
//!    is also reached through a strong path is still marked and its fields are
//!    scanned. The caller lists only active
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
//! What the argument does NOT cover (why this was opt-in until its flip, and
//! what the `=0` kill is for): a VM table
//! that is not a root yet can hand a young object back to a mutator (a
//! resurrection vector other than the JNI weak globals). None is known; the
//! inventory is the flip gate on the page.
//!
//! # Cost
//!
//! One young grid walk (the all-young path walks young too), one old walk
//! plus a read of every old reference slot, and a closure over the live young
//! set — inside the pause, whenever `CRATONVM_GEN_Y2O_LIVE_SEED` is on (the
//! default; `=0` kills it). gcj j2/og2 (O8): this said "only when the flag is
//! on", from before the flip.

use rustc_hash::{FxHashMap, FxHashSet};

use super::{
    active_narrow_geometry, for_each_conservative_ref_slot, for_each_ref_slot, mark_edge_precise,
    scan_young_object, seed_loaders_of_old_instances, seed_metadata_pins_of_old_owners,
    unreached_young_pinned_loaders, GenerationalHeap, GridStep, YoungGridCursor, YoungMarkCtx,
};
use cratonvm_gc_core::heap::{ObjectHeader, ObjectKind};
use cratonvm_gc_core::is_compact_object;
use crate::old_gen::OldGen;
use cratonvm_gc_core::young_mark::YoungMarkBits;

/// The switch (declared; `types/src/flag_groups.rs`, token `gen-y2o-live-seed`).
pub use crate::concurrent_mark::Y2O_LIVE_SEED_FLAG;

pub use crate::concurrent_mark::{y2o_live_seed_enabled, y2o_old_holder_census_enabled};

/// During a concurrent class-unload snapshot, a young mirror that is present
/// only because an old loader owns a `mirror_pin` row must not seed that
/// loader back into the old mark through `Class.classLoader`. Preserve the
/// mirror's other young-to-old edges, though: a live loader can own mirrors
/// whose outgoing references are relevant to this concurrent cycle. A mirror
/// reached from this pause's roots and young-object closure retains its
/// defining-loader edge; marks contributed later by the conservative
/// old-holder scan do not count as independent reachability.
fn seed_mirror_referents_without_old_owner(
    ctx: &YoungMarkCtx,
    is_old: &dyn Fn(usize) -> bool,
    grid: &[(usize, usize)],
    bits: &YoungMarkBits,
    root_reached_mirrors: &FxHashSet<usize>,
    worklist: &mut Vec<usize>,
    old_refs: &mut Vec<usize>,
) -> Result<usize, Y2oFallback> {
    let Some(pins) = ctx.mirror_pins.as_ref() else {
        return Ok(0);
    };
    let mut followed = 0usize;
    for (&owner, mirrors) in pins {
        if !is_old(owner) {
            continue;
        }
        for &mirror in mirrors {
            if mirror < ctx.from_base
                || mirror >= ctx.from_end
                || root_reached_mirrors.contains(&mirror)
            {
                continue;
            }
            let Ok(index) = grid.binary_search_by_key(&mirror, |&(obj, _)| obj) else {
                // The grid was proved complete before this point. Refuse the
                // narrowed seed and let the caller take its conservative path
                // rather than decode an unvalidated header.
                return Err(Y2oFallback::UnresolvedOwnerCandidate);
            };
            let (obj, _size) = grid[index];
            // SAFETY: `grid` contains only validated young object starts from
            // the complete from-space walk, and the world is stopped here.
            let header = unsafe { &*(obj as *const ObjectHeader) };
            // SAFETY: the validated grid start and header satisfy the scanner
            // contract. The defining-loader edge is supplied by the old
            // concurrent marker when that loader is independently live.
            unsafe {
                for_each_ref_slot(obj as *mut u8, header, |referent, _slot| {
                    let referent = referent as usize;
                    if referent != owner {
                        if is_old(referent) {
                            old_refs.push(referent);
                        } else {
                            mark_edge_precise(
                                referent,
                                ctx,
                                bits,
                                worklist,
                                "mirror-old-owner-field",
                            );
                        }
                    }
                });
            }
            followed += 1;
        }
    }
    Ok(followed)
}

/// gch h3/mj -- step 0 of `gcg-g4og-proposal-concurrent-initial-mark-seeds-only-root-reached-young-20261002`
/// (diagnostic, opt-in; token `gen-conc-y2o-old-holder-census`).
pub use crate::concurrent_mark::Y2O_OLD_HOLDER_CENSUS_FLAG;


pub use cratonvm_gc_core::concurrent_mark::Y2oOldHolderCensus;

/// The census of `seeded` (the live seed's old refs, duplicates allowed)
/// against `root_reached` (the old refs of the young objects reached without
/// any old holder). `old_walked` is the old generation's walk, ascending: an
/// object start found there contributes its size.
fn old_holder_census_of(
    seeded: &[usize],
    root_reached: &[usize],
    old_walked: &[(*mut u8, usize)],
) -> Y2oOldHolderCensus {
    let root: FxHashSet<usize> = root_reached.iter().copied().collect();
    let mut distinct: FxHashSet<usize> = FxHashSet::default();
    let mut census = Y2oOldHolderCensus::default();
    for &a in seeded {
        if !distinct.insert(a) {
            continue;
        }
        census.seeded += 1;
        if root.contains(&a) {
            continue;
        }
        census.via_old_holders += 1;
        let idx = old_walked.partition_point(|&(p, _)| (p as usize) <= a);
        if let Some(&(p, size)) = idx.checked_sub(1).and_then(|i| old_walked.get(i)) {
            if p as usize == a {
                census.via_old_holders_bytes += size;
            }
        }
    }
    census
}

/// What [`GenerationalHeap::collect_young_to_old_seeds`] is asked for.
#[derive(Debug, Clone, Copy)]
pub struct YoungToOldSeedRequest<'a> {
    /// The pause's root values, precise and conservative alike (addresses).
    pub roots: &'a [usize],
    /// Young objects the caller knows are live although no root names them:
    /// the finalizable objects and the JNI weak-global referents.
    pub extra_live: &'a [usize],
    /// Active young `Reference` objects whose weak referent slot is excluded
    /// from this cycle's strong live closure and old-generation seeds.
    pub hidden_young_refs: &'a [usize],
    /// Seed from the live young set (else every parsable young object).
    pub live_only: bool,
}

pub use cratonvm_gc_core::concurrent_mark::{Y2oFallback, YoungToOldSeeds};


/// The key `for_each_ref_slot` passes for `Reference.referent` (field 0) of
/// the object `header` heads: the slot INDEX of a legacy object, the byte
/// DISPLACEMENT of a compact one (the displacement `ConcurrentMarker`'s
/// hidden-slot test uses), `None` for an array or a compact object whose field
/// 0 is not a reference.
pub(super) fn referent_slot_key(header: &ObjectHeader) -> Option<usize> {
    if header.kind() == ObjectKind::Array {
        return None;
    }
    if is_compact_object(header) {
        cratonvm_gc_core::heap::with_compact_oop_scan(header, |layout, _body| {
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
/// # Why (`docs/feature-designs/generative-gc-architecture.md`)
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
    let idx = grid
        .partition_point(|&(start, _)| start <= addr)
        .checked_sub(1)?;
    let (start, size) = grid[idx];
    (addr < start + size).then_some(idx)
}

/// Seed the old refs of the young object at `obj` into `seeds`, skipping the
/// referent slot when `hide` names it and the referent is old, and skipping
/// class-unload mirror-owner edges present in `mirror_owner_edges`.
///
/// # Safety
/// `obj` is a valid young object header readable for its whole extent, and
/// the world is stopped.
unsafe fn seed_from_young_object(
    obj: *mut u8,
    hide: bool,
    is_old: &dyn Fn(usize) -> bool,
    mirror_owner_edges: Option<&FxHashMap<usize, FxHashSet<usize>>>,
    seeds: &mut YoungToOldSeeds,
) {
    // SAFETY: (caller contract) a valid, stable young object.
    let header = unsafe { &*(obj as *const ObjectHeader) };
    seeds.class_ids.insert(header.class_id.as_u32());
    let hidden_key = if hide {
        referent_slot_key(header)
    } else {
        None
    };
    let mut skipped = 0usize;
    let old_refs = &mut seeds.old_refs;
    // SAFETY: as above.
    unsafe {
        for_each_ref_slot(obj, header, |r, slot| {
            let value = r as usize;
            if is_old(value) {
                if hidden_key == Some(slot) {
                    skipped += 1;
                } else if mirror_owner_edges
                    .and_then(|edges| edges.get(&(obj as usize)))
                    .is_some_and(|owners| owners.contains(&value))
                {
                    // During class unloading, a young mirror reached only
                    // through conservative old-holder/side-table closure must
                    // not feed its defining loader back into the old mark.
                    // A mirror reachable from the pause's true roots is kept
                    // in `root_reached_mirrors` and retains this edge.
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
            unsafe { seed_from_young_object(ptr, hide, &is_old, None, &mut seeds) };
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
        let mut seeds = self.live_set_seeds(req, hidden)?;
        // gch h3/mj: g4og step 0, at the initial mark only (the remark's
        // seed is not what g4og changes), after the seeds are final.
        if self.conc_initial_mark_open() && y2o_old_holder_census_enabled() {
            seeds.old_holder_census = Some(self.old_holder_census(req, hidden, &seeds.old_refs));
        }
        Ok(seeds)
    }

    /// gch h3/mj: is a concurrent cycle in its initial-mark pause? (The
    /// driver claims `InitialMark` before that pause's seeds and moves to
    /// `ConcurrentMark` after it; the remark's seeds run in `ConcurrentMark`,
    /// before `remark_begin`.)
    fn conc_initial_mark_open(&self) -> bool {
        self.conc_cycle_state
            .as_deref()
            .is_some_and(|s| s.phase() == crate::concurrent_mark::ConcurrentGcPhase::InitialMark)
    }

    /// gch h3/mj -- g4og step 0 (see [`Y2oOldHolderCensus`]): a second live
    /// marking with NO old holder (`strong_holder = |_| false`: roots, extra
    /// live objects and the old-owner side-table seeds only, g4og's item 1
    /// seed), and the census of `seeded` against its old refs. Diagnostic:
    /// one more young grid walk, old walk and young closure per initial mark,
    /// only under the switch.
    fn old_holder_census(
        &self,
        req: &YoungToOldSeedRequest<'_>,
        hidden: &FxHashSet<usize>,
        seeded: &[usize],
    ) -> Result<Y2oOldHolderCensus, Y2oFallback> {
        // gch h4/pc (h3/mj review item 8): no mark-why watch on this second
        // marking, so a `CRATONVM_DBG` watch prints once per initial mark.
        self.with_live_young_mark(req.roots, req.extra_live, &|_| false, false, Some(hidden), |live| {
            let is_old = |a: usize| live.old.contains(a as *const u8);
            let marked = live.marked_grid_objects();
            let mut root_reached = YoungToOldSeeds::default();
            for (idx, &(obj, _size)) in live.grid.iter().enumerate() {
                if !marked[idx] {
                    continue;
                }
                let hide = !hidden.is_empty() && hidden.contains(&obj);
                // SAFETY: a grid object of mapped from-space; the world is
                // stopped.
                unsafe {
                    seed_from_young_object(obj as *mut u8, hide, &is_old, None, &mut root_reached)
                };
            }
            old_holder_census_of(seeded, &root_reached.old_refs, live.old_walked)
        })
    }

    /// The live-set seeds proper (the body of
    /// [`Self::young_to_old_seeds_from_live_set`] before gch h3/mj).
    fn live_set_seeds(
        &self,
        req: &YoungToOldSeedRequest<'_>,
        hidden: &FxHashSet<usize>,
    ) -> Result<YoungToOldSeeds, Y2oFallback> {
        self.with_live_young_mark(req.roots, req.extra_live, &|_| true, true, Some(hidden), |live| {
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
            seeds.old_refs.extend(live.mirror_old_refs.iter().copied());
            let mirror_owner_edges = if cratonvm_gc_core::gc_quiescence::class_unload_marking() {
                live.ctx.mirror_pins.as_ref().map(|pins| {
                    let mut edges: FxHashMap<usize, FxHashSet<usize>> = FxHashMap::default();
                    for (&owner, mirrors) in pins {
                        for &mirror in mirrors {
                            if !live.root_reached_mirrors.contains(&mirror) {
                                edges.entry(mirror).or_default().insert(owner);
                            }
                        }
                    }
                    edges
                })
            } else {
                None
            };
            let trace_mirror_edges = cratonvm_gc_core::gc_quiescence::class_unload_marking()
                && false;
            let mut traced_mirror_edges = 0usize;
            for (idx, &(obj, _size)) in live.grid.iter().enumerate() {
                if !marked[idx] {
                    continue;
                }
                seeds.seeding_objects += 1;
                let hide = !hidden.is_empty() && hidden.contains(&obj);
                if trace_mirror_edges && traced_mirror_edges < 64 {
                    if let Some(pins) = live.ctx.mirror_pins.as_ref() {
                        // SAFETY: `obj` is a validated young object start in
                        // the complete live grid; the pause keeps it stable.
                        let header = unsafe { &*(obj as *const ObjectHeader) };
                        // SAFETY: the validated grid entry proves the header
                        // and all reference slots lie inside from-space.
                        unsafe {
                            for_each_ref_slot(obj as *mut u8, header, |referent, slot| {
                                let owner = referent as usize;
                                if pins
                                    .get(&owner)
                                    .is_some_and(|mirrors| mirrors.contains(&obj))
                                    && traced_mirror_edges < 64
                                {
                                    let class_name = cratonvm_gc_core::gc::resolve_class_info(
                                        header.class_id.as_u32(),
                                    )
                                    .map(|(name, _)| name)
                                    .unwrap_or_else(|| "<unresolved>".to_string());
                                    let will_seed = live.root_reached_mirrors.contains(&obj);
                                    eprintln!(
                                        "[conc-y2o-mirror-edge] owner={owner:#x} source={obj:#x} class={class_name} slot={slot} root_reached={will_seed} direct_root={} extra_live={} will_seed={will_seed}",
                                        req.roots.contains(&obj),
                                        req.extra_live.contains(&obj),
                                    );
                                    traced_mirror_edges += 1;
                                }
                            });
                        }
                    }
                }
                // SAFETY: a grid object of mapped from-space; the world is
                // stopped.
                unsafe {
                    seed_from_young_object(
                        obj as *mut u8,
                        hide,
                        &is_old,
                        mirror_owner_edges.as_ref(),
                        &mut seeds,
                    )
                };
            }
            seeds
        })
    }

    /// gcd d1/c (2026-09-27) — the YOUNG objects reachable from the old
    /// objects `through` but not from the pause's strong live set: what a
    /// concurrent remark's dead-finalizer retention keeps alive only for
    /// `finalize()`
    /// (`docs/feature-designs/generative-gc-architecture.md`).
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
        self.with_live_young_mark(roots, extra_live, strong_holder, true, None, |live| {
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
                scan_young_object(addr, live.ctx, live.bits, &mut work, false);
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
    /// marking cannot be proved complete. `mark_why_watch`: whether the
    /// `CRATONVM_DBG` mark-why watch (`heap::young_mark_watch`) applies to this
    /// marking (gch h4/pc: not on a diagnostic second marking).
    fn with_live_young_mark<R>(
        &self,
        roots: &[usize],
        extra_live: &[usize],
        strong_holder: &dyn Fn(usize) -> bool,
        mark_why_watch: bool,
        hidden_young_refs: Option<&FxHashSet<usize>>,
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
            overlay_owners: cratonvm_gc_core::external_roots::external_owner_addrs()
                .map(|owners| owners.into_iter().collect()),
            metadata_pins: cratonvm_types::metadata_pin::snapshot(),
            mark_why: if mark_why_watch {
                cratonvm_gc_core::heap::young_mark_watch()
            } else {
                0
            },
            loader_pins: if loader_pin_on {
                cratonvm_types::loader_pin::snapshot_where(|a| a >= base && a < end)
            } else {
                None
            },
            mirror_pins: cratonvm_types::mirror_pin::snapshot(),
        };
        // O3: snapshot loader-owner membership once per marking invocation,
        // only if an unresolved candidate actually needs the lookup. A class
        // table can contain many rows for the same loader; do not scan all of
        // them again for every candidate or retain addresses across pauses.
        let loader_owners = std::cell::OnceCell::<FxHashSet<usize>>::new();
        let is_owner = |a: usize| -> bool {
            ctx.overlay_owners.as_ref().is_some_and(|o| o.contains(&a))
                || ctx.mirror_pins.as_ref().is_some_and(|m| m.contains_key(&a))
                || ctx
                    .metadata_pins
                    .as_ref()
                    .is_some_and(|m| m.contains_key(&a))
                || ctx
                    .loader_pins
                    .as_ref()
                    .is_some_and(|m: &FxHashMap<u32, usize>| {
                        loader_owners
                            .get_or_init(|| m.values().copied().collect())
                            .contains(&a)
                    })
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
            // gcj j2/og2 (O3 of `gcj-j1og-concurrent-cycle-and-old-gen-review-residuals`):
            // an address outside from-space resolves to no grid object and
            // cannot be an unresolved young owner, so it is dropped before the
            // grid's binary search -- the common case, since most slots the
            // old-object loop below hands in are old -> old. Same answers.
            if addr < base || addr >= end {
                return;
            }
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
        // Record which young mirrors are reachable from this pause's actual
        // roots before the conservative old-holder and old-owner pin seeds
        // are added. In a class-unload snapshot, only mirrors outside this set
        // may have their defining-loader back edge suppressed.
        while let Some(addr) = work.pop() {
            scan_young_object(
                addr,
                &ctx,
                &bits,
                &mut work,
                hidden_young_refs.is_some_and(|hidden| hidden.contains(&addr)),
            );
        }
        let mut root_reached_mirrors = FxHashSet::default();
        if cratonvm_gc_core::gc_quiescence::class_unload_marking() {
            if let Some(pins) = ctx.mirror_pins.as_ref() {
                for mirrors in pins.values() {
                    root_reached_mirrors.extend(
                        mirrors
                            .iter()
                            .copied()
                            .filter(|&mirror| bits.contains(mirror)),
                    );
                }
            }
        }
        // Minor-GC semantics for the old generation, widened from the dirty
        // cards to every old object (see the module doc). gcd d1/c: only the
        // holders `strong_holder` accepts (every one, for the seeds).
        let class_unload = cratonvm_gc_core::gc_quiescence::class_unload_marking();
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
            cratonvm_gc_core::external_roots::external_roots_for_matching_owners(&|owner_addr| {
                is_old(owner_addr)
            })
        {
            seed(overlay_ref.as_ptr() as usize, &mut work);
        }
        if unresolved_owner {
            return Err(Y2oFallback::UnresolvedOwnerCandidate);
        }
        let mut mirror_old_refs = Vec::new();
        if class_unload {
            seed_mirror_referents_without_old_owner(
                &ctx,
                &is_old,
                &grid,
                &bits,
                &root_reached_mirrors,
                &mut work,
                &mut mirror_old_refs,
            )?;
        } else {
            let _ = super::seed_mirror_pins_of_old_owners(&ctx, &is_old, &bits, &mut work);
        }
        let _ = seed_metadata_pins_of_old_owners(&ctx, &is_old, &bits, &mut work);
        while let Some(addr) = work.pop() {
            scan_young_object(
                addr,
                &ctx,
                &bits,
                &mut work,
                hidden_young_refs.is_some_and(|hidden| hidden.contains(&addr)),
            );
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
                scan_young_object(
                    addr,
                    &ctx,
                    &bits,
                    &mut work,
                    hidden_young_refs.is_some_and(|hidden| hidden.contains(&addr)),
                );
            }
        }
        Ok(then(&LiveYoungMark {
            grid: &grid,
            bits: &bits,
            ctx: &ctx,
            old: &*old,
            old_walked: &walked,
            mirror_old_refs: &mirror_old_refs,
            root_reached_mirrors: &root_reached_mirrors,
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
    /// gch h3/mj: the old generation's walk (ascending, complete: the
    /// marking refuses an incomplete one).
    old_walked: &'a [(*mut u8, usize)],
    /// Old referents seeded from side-table-only mirrors during a concurrent
    /// class-unload snapshot, excluding the defining-loader back edge.
    mirror_old_refs: &'a [usize],
    /// Young mirrors reached through pause roots before conservative
    /// old-holder or old-owner side-table closure was added.
    root_reached_mirrors: &'a FxHashSet<usize>,
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
mod tests;
