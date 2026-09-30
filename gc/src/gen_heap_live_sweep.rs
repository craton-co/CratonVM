// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gen r5w5/old9 (2026-09-27) — the O(live) stop-the-world old-gen collection
//! (`CRATONVM_GC_OLD_LIVE_SWEEP`, opt-in).
//!
//! `docs/internal/gc/gengc-r5w3-oldgen7-proposal-bot-object-base-oracle-for-an-o-live-sweep-DONE-20260928.md`
//! (steps 1, 3 and 4) and the O(live) half of
//! `docs/internal/gc/gengc-r4w4-oldgen4-proposal-parallel-compaction-and-an-o-live-sweep-RETIRED-20260927.md`.
//!
//! # Why
//!
//! `old_gen_gc_inner`'s in-place arm — every default old-gen collection, on
//! the moving path's `major_gc` and the non-moving path's
//! `sweep_old_gen_non_moving` alike — starts by walking EVERY allocated object
//! (`walk_objects_with_gaps`) because the walked grid is the mark's admission
//! oracle; it then closes the live set over the whole grid and frees from a
//! third pass over it. On a large old generation whose live set is small that
//! is three passes over what died.
//!
//! # What this does instead
//!
//! The same mark — the same roots, the same young→old seed
//! (`mark_young_to_old_refs`), the same overlay / loader / mirror / metadata
//! edges, the same push sites (`mark_and_push_old_gen_in`,
//! `scan_object_for_old_refs_in`) — with [`LiveOracle`] as the admission
//! oracle: `OldGen::object_at`, a bounded stride from a block-offset card's
//! anchor. The oracle also records every mark the push sites make, so the
//! kept set is known without a walk. Then `OldGen::close_live_set_by_oracle`
//! (the GCAUD-8 closure over kept objects only) and
//! `OldGen::sweep_dead_runs_around` (the dead runs between kept objects, freed
//! without decoding a dead header; the table re-anchored from the kept set).
//!
//! # When it steps aside
//!
//! It answers only when it can vouch for the answer. Any oracle query it could
//! not answer (a walk gap, an overlapping free list, the table off), a kept
//! grid the sweep rejects, or — under `CRATONVM_DBG_OLD_LIVE_SWEEP_VERIFY` — any
//! disagreement with the walked grid, and it clears every mark it set, frees
//! nothing, and returns `None`: `old_gen_gc_inner` then runs the walked
//! collection from scratch. gen r5w6/old10: the same when a registered
//! finalizable in old gen is left unreached by the strong closure (only the
//! walked collection resurrects it); a REACHED one no longer keeps the
//! collection off this path. The diagnostics that report per DEAD object (the
//! sweep-liveness assertion, the doomed-referrer scan, the old-mark census,
//! the A2 breadcrumb, the overlay-owner reporter, the interior-pin negative
//! control, `CRATONVM_NO_OLDGEN_COALESCE`) keep the walked collection, so a
//! run that asks for them gets them ([`eligible`]).
//!
//! # A stale mark
//!
//! The walked collection's closure and free loop pass over every walked
//! object, so an object carrying a `GC_FLAG_MARKED` that this pause did not
//! set (a walk-gap survivor of an earlier collection) is kept and its
//! referents traced by the closure. Here only marked objects this pause
//! reached are kept, so a push site that finds a mark it did not make asks
//! the oracle ([`super::OldMarkGrid::retrace_stale_mark`]) and traces the
//! object once. A stale mark on an object nothing reaches is simply freed
//! with its run.

use std::cell::{Cell, RefCell};
use std::sync::atomic::Ordering;

use rustc_hash::FxHashSet;

use cratonvm_types::ObjectRef;

use super::{
    doomed_referrers_dbg, mark_and_push_old_gen_in, note_interior_old_root, oldmark_census,
    GenerationalHeap, OldMarkGrid, OLDMARK_INTERIOR_ROOT_PINS, OLD_SWEEP_CLOSURE_RESCUES,
    OLD_SWEEP_ESCAPE_HITS,
};
use crate::arena::Arena;
use crate::gc_flags;
use crate::heap::{ObjectHeader, GC_FLAG_MARKED};
use crate::old_gen::{ObjectAt, OldGen};

/// The switch. Read per collection (one declared-flag lookup per major).
const LIVE_SWEEP_FLAG: &str = "CRATONVM_GC_OLD_LIVE_SWEEP";
/// The differential: every collection the live sweep answers is also walked,
/// and every walked object must agree with the oracle and the kept set.
const LIVE_SWEEP_VERIFY_FLAG: &str = "CRATONVM_DBG_OLD_LIVE_SWEEP_VERIFY";

/// May this collection take the O(live) path? The flag, and none of the
/// per-dead-object diagnostics (see the module doc).
pub(super) fn eligible() -> bool {
    cratonvm_types::flags::runtime_flag_on(LIVE_SWEEP_FLAG)
        && !oldmark_census::enabled()
        && !gc_flags().dbg_sweep_liveness
        && !gc_flags().no_oldgen_coalesce
        && !doomed_referrers_dbg()
        && !crate::a2dbg::enabled()
        && !cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_OLDSWEEP_OWNERS")
        && !cratonvm_types::flags::runtime_flag_on("CRATONVM_GC_NO_OLD_INTERIOR_PINS")
}

/// The block-offset oracle as the mark's [`OldMarkGrid`], plus the kept set.
///
/// `marked` is every old-gen base this pause marked (through a push site's
/// [`OldMarkGrid::note_marked`], or [`mark_live`]) or found carrying a stale
/// mark and re-traced. `unknown` counts queries the table could not answer;
/// any at all and the attempt is abandoned.
struct LiveOracle<'a> {
    old_gen: &'a OldGen,
    marked: RefCell<FxHashSet<usize>>,
    unknown: Cell<u64>,
    stale: Cell<u64>,
}

impl<'a> LiveOracle<'a> {
    fn new(old_gen: &'a OldGen) -> Self {
        Self {
            old_gen,
            marked: RefCell::new(FxHashSet::default()),
            unknown: Cell::new(0),
            stale: Cell::new(0),
        }
    }

    /// `OldGen::object_at`, counting the unanswerable.
    fn at(&self, addr: usize) -> ObjectAt {
        let a = self.old_gen.object_at(addr);
        if a == ObjectAt::Unknown {
            self.unknown.set(self.unknown.get() + 1);
        }
        a
    }
}

impl OldMarkGrid for LiveOracle<'_> {
    fn has_base(&self, addr: usize) -> bool {
        matches!(self.at(addr), ObjectAt::Base { .. })
    }

    fn interior_base(&self, addr: usize) -> Option<(usize, usize)> {
        match self.at(addr) {
            ObjectAt::Interior { base, size } => Some((base, size)),
            _ => None,
        }
    }

    /// Authoritative everywhere in the generation: an address that is not a
    /// base (inside an object, in a free block, or unanswerable — the last
    /// counted, and the attempt abandoned) is never admitted.
    fn authoritative_base(&self, addr: usize) -> Option<bool> {
        Some(matches!(self.at(addr), ObjectAt::Base { .. }))
    }

    fn note_marked(&self, addr: usize) {
        self.marked.borrow_mut().insert(addr);
    }

    fn retrace_stale_mark(&self, addr: usize) -> bool {
        let fresh = self.marked.borrow_mut().insert(addr);
        if fresh {
            self.stale.set(self.stale.get() + 1);
        }
        fresh
    }
}

/// Mark the old-gen object at `base` (a base the oracle resolved) and push
/// it, or re-trace it once if it carried a mark this pause did not set — the
/// root loop's twin of `mark_and_push_old_gen_in`'s tail.
fn mark_live(oracle: &LiveOracle<'_>, base: usize, worklist: &mut Vec<*mut u8>) {
    // SAFETY: `base` is an object base `OldGen::object_at` resolved under the
    // old-gen lock this pause holds, so its header is mapped and valid; the
    // mark word is atomic.
    let header = unsafe { &*(base as *const ObjectHeader) };
    if header.gc_flags() & GC_FLAG_MARKED == 0 {
        header.add_gc_flags(GC_FLAG_MARKED);
        oracle.note_marked(base);
        worklist.push(base as *mut u8);
    } else if oracle.retrace_stale_mark(base) {
        worklist.push(base as *mut u8);
    }
}

/// Clear `GC_FLAG_MARKED` on every base in `bases` (an abandoned attempt, or
/// the survivors after the sweep).
fn clear_marks(bases: impl Iterator<Item = usize>) {
    for b in bases {
        // SAFETY: every base passed here was resolved by the oracle and marked
        // by this pause, under the old-gen lock; nothing has been freed that
        // holds it (an abandoned attempt freed nothing, and the sweep keeps
        // every kept object).
        unsafe { &*(b as *const ObjectHeader) }.clear_gc_flags(GC_FLAG_MARKED);
    }
}

impl GenerationalHeap {
    /// The O(live) in-place old-gen collection — see the module doc. `Some`
    /// is exactly what the walked in-place arm returns (identity entries for
    /// watched survivors); `None` means nothing changed and the caller must
    /// run the walked collection.
    ///
    /// `fin_candidates` (gen r5w6/old10): the collection's registered
    /// finalizables (`OldGenFinalizerPass::candidates`; empty without a
    /// pass). Any old-gen one the strong closure did not reach declines the
    /// attempt — see the check after the mark.
    pub(super) fn old_gen_gc_live(
        roots: &[ObjectRef],
        young_from: &Arena,
        old_gen: &mut OldGen,
        young_skips: &[(usize, usize)],
        fin_candidates: &[usize],
    ) -> Option<cratonvm_types::PointerMap> {
        let t0 = std::time::Instant::now();
        if !old_gen.block_offset_table_enabled() {
            Self::live_sweep_fallback(old_gen, "the block-offset table is off", &[]);
            return None;
        }
        // ---- Mark (the walked arm's, over the oracle) ----
        let marked: Result<Vec<usize>, (&'static str, Vec<usize>)> = {
            let og: &OldGen = old_gen;
            let oracle = LiveOracle::new(og);
            let mut worklist: Vec<*mut u8> = Vec::new();
            let mut interior_bases: FxHashSet<usize> = FxHashSet::default();
            for root in roots {
                let ptr = root.as_ptr();
                match oracle.at(ptr as usize) {
                    ObjectAt::Base { .. } => mark_live(&oracle, ptr as usize, &mut worklist),
                    ObjectAt::Interior { base, size } => {
                        // H2-CID0: an interior conservative root keeps the
                        // object it points into (never moved here either).
                        if interior_bases.insert(base) {
                            let n = OLDMARK_INTERIOR_ROOT_PINS.fetch_add(1, Ordering::Relaxed);
                            if n < 8 {
                                note_interior_old_root(ptr, base, size);
                            }
                        }
                        mark_live(&oracle, base, &mut worklist);
                    }
                    // Young, outside the heap, or a free block: no old object.
                    ObjectAt::Free | ObjectAt::Unknown => {}
                }
            }
            Self::mark_young_to_old_refs(young_from, og, &oracle, young_skips, &mut worklist, None);
            for overlay_ref in
                crate::external_roots::external_roots_for_matching_owners(&|owner_addr| {
                    young_from.contains(owner_addr as *mut u8)
                })
            {
                mark_and_push_old_gen_in(
                    overlay_ref.as_ptr(),
                    og,
                    &oracle,
                    &mut worklist,
                    "external-overlay(young owner)",
                );
            }
            let loader_pin_on = cratonvm_types::loader_pin::loader_pinning_enabled();
            let mirror_pin_owners: FxHashSet<usize> =
                cratonvm_types::mirror_pin::pinned_owner_addrs()
                    .into_iter()
                    .collect();
            let metadata_pins = cratonvm_types::metadata_pin::snapshot();
            while let Some(obj_ptr) = worklist.pop() {
                Self::scan_object_for_old_refs_in(obj_ptr, og, &oracle, &mut worklist);
                // SAFETY: `obj_ptr` is a marked old-gen object base.
                let owner_class = Some(
                    unsafe { &*(obj_ptr as *const ObjectHeader) }
                        .class_id
                        .as_u32(),
                );
                crate::external_roots::with_external_roots_for_owner(
                    obj_ptr as usize,
                    owner_class,
                    |overlay_refs| {
                        for overlay_ref in overlay_refs {
                            mark_and_push_old_gen_in(
                                overlay_ref.as_ptr(),
                                og,
                                &oracle,
                                &mut worklist,
                                "external-overlay(BFS owner)",
                            );
                        }
                    },
                );
                if loader_pin_on {
                    // SAFETY: as above.
                    let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                    if let Some(loader_addr) = cratonvm_types::loader_pin::loader_pin_addr_where(
                        header.class_id.as_u32(),
                        |a| og.contains(a as *const u8),
                    ) {
                        mark_and_push_old_gen_in(
                            loader_addr as *mut u8,
                            og,
                            &oracle,
                            &mut worklist,
                            "loader_pin",
                        );
                    }
                }
                if mirror_pin_owners.contains(&(obj_ptr as usize)) {
                    if let Some(mirror_addrs) =
                        cratonvm_types::mirror_pin::mirrors_for_loader(obj_ptr as usize)
                    {
                        for mirror_addr in mirror_addrs {
                            mark_and_push_old_gen_in(
                                mirror_addr as *mut u8,
                                og,
                                &oracle,
                                &mut worklist,
                                "mirror_pin",
                            );
                        }
                    }
                }
                if let Some(metadata_addrs) = metadata_pins
                    .as_ref()
                    .and_then(|m| m.get(&(obj_ptr as usize)))
                {
                    for &metadata_addr in metadata_addrs {
                        mark_and_push_old_gen_in(
                            metadata_addr as *mut u8,
                            og,
                            &oracle,
                            &mut worklist,
                            "metadata_pin",
                        );
                    }
                }
            }
            // gen r5w6/old10 — the finalizer pass's question, asked where the
            // walked pass asks it (`OldGenFinalizerPass::seed`: the strong
            // closure's worklist just ran dry): is any registered finalizable
            // an old-gen object this closure did not reach? Then it is due for
            // resurrection, which only the walked collection performs, so this
            // attempt declines. Otherwise that pass would find nothing to do
            // and the answer below is the same as the walked collection's.
            let fin_unreached = fin_candidates.iter().any(|&a| {
                matches!(oracle.at(a), ObjectAt::Base { .. })
                    // SAFETY: `a` is an object base the oracle resolved under
                    // the old-gen lock this pause holds; the mark word is
                    // atomic.
                    && unsafe { &*(a as *const ObjectHeader) }.gc_flags() & GC_FLAG_MARKED == 0
            });
            let unknown = oracle.unknown.get();
            let stale = oracle.stale.get();
            let set: Vec<usize> = oracle.marked.into_inner().into_iter().collect();
            if stale > 0 {
                tracing::debug!(
                    target: "cratonvm::gc",
                    stale,
                    "old-gen live sweep: re-traced objects that carried a mark this pause did not set",
                );
            }
            if unknown > 0 {
                Err(("the block-offset oracle could not answer a mark query (a walk gap?)", set))
            } else if fin_unreached {
                Err((
                    "an old-gen finalizable is unreached (the walked collection resurrects it)",
                    set,
                ))
            } else {
                Ok(set)
            }
        };
        let marked = match marked {
            Ok(m) => m,
            Err((why, set)) => {
                Self::live_sweep_fallback(old_gen, why, &set);
                return None;
            }
        };

        // ---- Sizes, closure, (verify) ----
        let mut live: Vec<(usize, usize)> = Vec::with_capacity(marked.len());
        for &b in &marked {
            match old_gen.object_at(b) {
                ObjectAt::Base { size } => live.push((b, size)),
                _ => {
                    Self::live_sweep_fallback(old_gen, "a marked object is not a base", &marked);
                    return None;
                }
            }
        }
        let closure = old_gen.close_live_set_by_oracle(&mut live);
        if closure.unknown > 0 {
            let all: Vec<usize> = live.iter().map(|&(b, _)| b).collect();
            Self::live_sweep_fallback(old_gen, "the closure met an unanswerable reference", &all);
            return None;
        }
        live.sort_unstable_by_key(|&(b, _)| b);
        if closure.promoted > 0 {
            OLD_SWEEP_CLOSURE_RESCUES.fetch_add(closure.promoted as u64, Ordering::Relaxed);
            tracing::warn!(
                "old-gen live sweep: the mark phase missed {} object(s) that a LIVE old-gen \
                 object still references; retaining them (the walked sweep's close_live_set \
                 rescue — see old-sweep-liveness.md).",
                closure.promoted,
            );
        }
        if closure.escaped > 0 {
            let n = OLD_SWEEP_ESCAPE_HITS.fetch_add(1, Ordering::Relaxed);
            if n < 8 {
                tracing::warn!(
                    escaped = closure.escaped,
                    "old-gen live sweep: a live old-gen object references an in-old-gen address \
                     that is not an object base — an EARLIER reclamation already freed a live \
                     block. See `old_gen::COMPACT_ESCAPE_HITS` and old-sweep-liveness.md.",
                );
            }
        }
        if cratonvm_types::flags::runtime_flag_on(LIVE_SWEEP_VERIFY_FLAG) {
            let mismatches = Self::live_sweep_verify(old_gen, &live);
            if mismatches > 0 {
                let all: Vec<usize> = live.iter().map(|&(b, _)| b).collect();
                let why = "verify: the oracle disagrees with the walk";
                Self::live_sweep_fallback(old_gen, why, &all);
                return None;
            }
        }

        // ---- Sweep ----
        let outcome = match old_gen.sweep_dead_runs_around(&live) {
            Ok(o) => o,
            Err(why) => {
                let all: Vec<usize> = live.iter().map(|&(b, _)| b).collect();
                Self::live_sweep_fallback(old_gen, why.label(), &all);
                return None;
            }
        };
        let watched = crate::gc_quiescence::watched_referents_snapshot();
        let mut watched_survivors = cratonvm_types::PointerMap::default();
        if let Some(w) = watched.as_ref() {
            for &(b, _) in &live {
                if w.contains(&b) {
                    watched_survivors.insert(b, b);
                }
            }
        }
        clear_marks(live.iter().map(|&(b, _)| b));
        let merged = old_gen.coalesce_free_blocks();
        let _ = old_gen.after_in_place_sweep(gc_flags().old_give_back);
        tracing::debug!(
            target: "cratonvm::gc",
            kept = outcome.kept_objects,
            kept_bytes = outcome.kept_bytes,
            dead_runs = outcome.dead_runs,
            freed_bytes = outcome.freed_bytes,
            merged_blocks = merged,
            micros = t0.elapsed().as_micros() as u64,
            "old-gen live sweep (O(live))",
        );
        Some(watched_survivors)
    }

    /// Abandon a live-sweep attempt: clear the marks it set on `bases`, count
    /// it, and say why (rate-limited). Nothing was freed.
    fn live_sweep_fallback(old_gen: &mut OldGen, why: &'static str, bases: &[usize]) {
        clear_marks(bases.iter().copied());
        let n = old_gen.note_live_sweep_fallback();
        if n < 8 || n.is_power_of_two() {
            tracing::info!(
                target: "cratonvm::gc",
                why,
                fallbacks = n + 1,
                "old-gen live sweep declined; the walked collection runs instead",
            );
        }
    }

    /// `CRATONVM_DBG_OLD_LIVE_SWEEP_VERIFY`: walk the generation and check that
    /// the oracle names every walked object's base and an interior byte of it,
    /// and that the kept set is exactly the walked objects that are marked.
    /// Prints one `[old-live-sweep] verify` line (stderr) and returns the
    /// number of disagreements (0 on a correct oracle).
    fn live_sweep_verify(old_gen: &OldGen, live: &[(usize, usize)]) -> usize {
        let (walked, gaps) = old_gen.walk_objects_with_gaps();
        let mut mismatches = 0usize;
        let mut walked_marked = 0usize;
        for &(p, size) in &walked {
            let p = p as usize;
            if old_gen.object_at(p) != (ObjectAt::Base { size }) {
                mismatches += 1;
            }
            if size > 8
                && old_gen.object_at(p + 8)
                    != (ObjectAt::Interior {
                        base: p,
                        size,
                    })
            {
                mismatches += 1;
            }
            // SAFETY: a base the walk yielded, under the old-gen lock.
            let marked = unsafe { &*(p as *const ObjectHeader) }.gc_flags() & GC_FLAG_MARKED != 0;
            let kept = live.binary_search_by_key(&p, |&(b, _)| b).is_ok();
            if marked {
                walked_marked += 1;
            }
            if marked != kept {
                mismatches += 1;
            }
        }
        if gaps.is_empty() && walked_marked != live.len() {
            mismatches += 1;
        }
        eprintln!(
            "[old-live-sweep] verify walked={} kept={} gaps={} mismatches={}",
            walked.len(),
            live.len(),
            gaps.len(),
            mismatches,
        );
        mismatches
    }
}

#[cfg(test)]
mod tests {
    use super::super::{OldGenFinalizerPass, PROMOTION_AGE};
    use super::*;
    use crate::collector::MonitorCleanup;
    use cratonvm_types::{ClassId, Value};

    struct NoMonitors;
    impl MonitorCleanup for NoMonitors {
        fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
    }

    fn stw() -> crate::collector::StopTheWorldToken {
        // SAFETY: these unit tests run the heap single-threaded.
        unsafe { crate::collector::StopTheWorldToken::new() }
    }

    /// `n` promoted objects in a chain: object `i` holds `Int(i)` in slot 0
    /// and object `i + 1` in slot 1 (the last holds nothing there).
    fn promoted_chain(heap: &GenerationalHeap, n: usize) -> Vec<ObjectRef> {
        let mut objs: Vec<ObjectRef> =
            (0..n).map(|_| heap.alloc_object(ClassId::new(0), 2)).collect();
        for i in 0..n {
            heap.set_field(objs[i], 0, Value::Int(i as i32));
            if i + 1 < n {
                heap.set_field(objs[i], 1, Value::Object(Some(objs[i + 1])));
            }
        }
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&stw(), &mut objs, &NoMonitors);
        }
        for o in &objs {
            assert!(heap.is_in_old(o.as_ptr()), "precondition: promoted");
        }
        objs
    }

    /// Run the in-place major over `roots` with the live sweep on or off.
    fn major(heap: &GenerationalHeap, roots: &mut [ObjectRef], live: bool) -> usize {
        let v = if live { Some("1") } else { None };
        cratonvm_types::flags::with_thread_overrides(&[(LIVE_SWEEP_FLAG, v)], || {
            let young_from = heap.lock_young_from();
            let mut old_gen = heap.old_gen.lock();
            let _ = GenerationalHeap::major_gc(roots, &young_from, &mut old_gen, &[]);
            old_gen.used()
        })
    }

    /// The O(live) major keeps exactly what the walked major keeps: two heaps
    /// built the same way, the chain cut in the middle (its tail unreachable),
    /// one collected each way — the same `used()`, the reachable half intact
    /// and readable, the counters saying which path ran.
    #[test]
    fn the_live_major_keeps_exactly_what_the_walked_major_keeps() {
        let mut results = Vec::new();
        for live in [false, true] {
            let heap = GenerationalHeap::with_sizes(256 * 1024, 1024 * 1024);
            let objs = promoted_chain(&heap, 200);
            // Cut the chain after object 79: 80..200 die.
            heap.set_field(objs[79], 1, Value::Object(None));
            let mut roots = vec![objs[0]];
            let before = heap.old_gen.lock().used();
            let after = major(&heap, &mut roots, live);
            assert!(after < before, "live={live}: the tail was reclaimed");
            // The kept half is intact: walk the chain from the root.
            let mut cur = Some(roots[0]);
            let mut seen = 0;
            while let Some(o) = cur {
                assert_eq!(heap.get_field(o, 0).as_int(), Some(seen), "live={live}");
                seen += 1;
                cur = match heap.get_field(o, 1) {
                    Value::Object(next) => next,
                    _ => None,
                };
            }
            assert_eq!(seen, 80, "live={live}");
            let stats = heap.old_gen.lock().sizing_stats();
            assert_eq!(stats.live_sweeps, u64::from(live), "live={live}: {stats:?}");
            assert_eq!(stats.live_sweep_fallbacks, 0, "live={live}: {stats:?}");
            results.push(after);
        }
        assert_eq!(results[0], results[1], "both paths keep the same bytes");
    }

    /// A reachable object carrying a STALE mark (one this pause did not set)
    /// is re-traced, so what only it references survives; the walked path's
    /// closure gives the same answer.
    #[test]
    fn a_stale_mark_on_a_reachable_object_does_not_lose_its_referents() {
        let heap = GenerationalHeap::with_sizes(256 * 1024, 1024 * 1024);
        let objs = promoted_chain(&heap, 10);
        // A stale mark in the middle of the chain.
        // SAFETY: an old-gen object base of this heap.
        unsafe { &*(objs[4].as_ptr() as *const ObjectHeader) }.add_gc_flags(GC_FLAG_MARKED);
        let mut roots = vec![objs[0]];
        let _ = major(&heap, &mut roots, true);
        let mut cur = Some(roots[0]);
        let mut seen = 0;
        while let Some(o) = cur {
            assert_eq!(heap.get_field(o, 0).as_int(), Some(seen));
            // SAFETY: a kept object base.
            let m = unsafe { &*(o.as_ptr() as *const ObjectHeader) }.gc_flags() & GC_FLAG_MARKED;
            assert_eq!(m, 0, "the sweep cleared every kept mark");
            seen += 1;
            cur = match heap.get_field(o, 1) {
                Value::Object(next) => next,
                _ => None,
            };
        }
        assert_eq!(seen, 10, "the objects behind the stale mark survived");
        assert_eq!(heap.old_gen.lock().sizing_stats().live_sweeps, 1);
    }

    /// gen r5w6/old10 — an old-gen finalizer candidate no longer makes the
    /// collection ineligible: a REACHED one leaves the answer to the live
    /// sweep (the walked pass would do nothing), an UNREACHED one declines it
    /// and the walked collection resurrects the candidate, as before.
    #[test]
    fn a_reached_old_finalizable_keeps_the_live_sweep_and_an_unreached_one_declines_it() {
        for reached in [true, false] {
            let heap = GenerationalHeap::with_sizes(256 * 1024, 1024 * 1024);
            let objs = promoted_chain(&heap, 10);
            // Cut the chain after object 4: 5..10 die.
            heap.set_field(objs[4], 1, Value::Object(None));
            let fin = objs[if reached { 2 } else { 7 }].as_ptr() as usize;
            let candidates = [fin];
            let mut roots = vec![objs[0]];
            let resurrected = cratonvm_types::flags::with_thread_overrides(
                &[(LIVE_SWEEP_FLAG, Some("1"))],
                || {
                    let young_from = heap.lock_young_from();
                    let mut old_gen = heap.old_gen.lock();
                    let mut pass = OldGenFinalizerPass::new(&candidates);
                    let _ = GenerationalHeap::major_gc_finalizing(
                        &mut roots,
                        &young_from,
                        &mut old_gen,
                        &[],
                        Some(&mut pass),
                        false,
                    );
                    pass.resurrected.clone()
                },
            );
            let stats = heap.old_gen.lock().sizing_stats();
            if reached {
                assert_eq!(stats.live_sweeps, 1, "{stats:?}");
                assert_eq!(stats.live_sweep_fallbacks, 0, "{stats:?}");
                assert!(resurrected.is_empty(), "a reached finalizable is not resurrected");
            } else {
                assert_eq!(stats.live_sweeps, 0, "{stats:?}");
                assert_eq!(stats.live_sweep_fallbacks, 1, "{stats:?}");
                assert_eq!(resurrected, vec![fin], "the walked pass resurrects it");
            }
            // Whatever answered, the reachable half is intact.
            for (i, o) in objs.iter().enumerate().take(5) {
                assert_eq!(heap.get_field(*o, 0).as_int(), Some(i as i32), "reached={reached}");
            }
        }
    }

    /// A walk gap (a header nobody can size) in front of reachable objects:
    /// the oracle refuses, the attempt is abandoned with every mark cleared,
    /// and the walked collection (with its gap recovery) answers instead.
    #[test]
    fn a_walk_gap_hands_the_collection_back_to_the_walked_path() {
        let heap = GenerationalHeap::with_sizes(256 * 1024, 1024 * 1024);
        let objs = promoted_chain(&heap, 30);
        // Break the header of object 10: kind byte to an invalid tag.
        // SAFETY: a byte inside an old-gen header of this heap.
        unsafe {
            std::ptr::write(
                (objs[10].as_ptr() as *mut u8).add(cratonvm_types::KIND_TAGS_BYTE_OFFSET),
                0xFF,
            )
        };
        let mut roots = vec![objs[0]];
        let _ = major(&heap, &mut roots, true);
        let stats = heap.old_gen.lock().sizing_stats();
        assert_eq!(stats.live_sweeps, 0, "{stats:?}");
        assert!(stats.live_sweep_fallbacks >= 1, "{stats:?}");
        // The objects in front of the break are intact.
        for (i, o) in objs.iter().enumerate().take(10) {
            assert_eq!(heap.get_field(*o, 0).as_int(), Some(i as i32));
        }
    }
}

/// gcd d1/d (2026-09-27) — `gen_heap.rs` functions of lane d's region, tested
/// here (a file of its own) so parallel lanes do not meet at the end of
/// `gen_heap.rs`.
#[cfg(test)]
mod gcd_d1d_tests {
    use super::super::*;

    const KIB: usize = 1024;

    /// `gengc-r5w5-sizer9-refill-can-carve-less-than-the-object`, clamp 2: a
    /// young generation whose only free span is an 8 KiB bump tail cannot hold
    /// a 20 KiB object, so a refill for it declines WITHOUT carving; a floor
    /// the tail can hold is served from the tail; the unfloored refill still
    /// carves the tail, byte for byte as before.
    #[test]
    fn a_refill_that_cannot_hold_the_object_declines_without_carving() {
        let heap = GenerationalHeap::with_sizes(256 * KIB, 256 * KIB);
        let (cap, used) = {
            let f = heap.lock_young_from();
            (f.capacity(), f.used())
        };
        let fill = (cap - used - 8 * KIB) & !7;
        let (_, got) = heap.refill_tlab(fill).expect("room for the fill");
        assert_eq!(got, fill);
        let tail = {
            let f = heap.lock_young_from();
            f.capacity() - f.used()
        };
        assert!((8 * KIB..20 * KIB).contains(&tail), "tail {tail}");
        let used_before = heap.lock_young_from().used();
        assert!(
            heap.refill_tlab_at_least(64 * KIB, 20 * KIB).is_none(),
            "no span can hold a 20 KiB object"
        );
        assert_eq!(
            heap.lock_young_from().used(),
            used_before,
            "nothing was carved"
        );
        let (_, small) = heap
            .refill_tlab_at_least(64 * KIB, 4 * KIB)
            .expect("the tail holds 4 KiB");
        assert_eq!(small, tail & !7, "served from the tail");
        assert!(heap.refill_tlab(64 * KIB).is_none(), "young is now full");
    }

    /// `gengc-r4w6-review6-old-pinned-compaction-residuals`, "Also noted" 1: an
    /// old-gen object named only by an INTERIOR word of a young stretch the
    /// walk cannot parse is pinned AND marked and pushed for the BFS when a
    /// pinned compaction is planned (so its side-table edges are traced like
    /// any marked object's); without a plan, nothing changes (not marked).
    #[test]
    fn an_interior_word_in_an_unparseable_young_stretch_is_marked_for_the_bfs() {
        let heap = GenerationalHeap::with_sizes(64 * KIB, 64 * KIB);
        let x = {
            let mut og = heap.old_gen.lock();
            let p = og
                .alloc(HEADER_SIZE + SLOT_SIZE * 4, 8)
                .expect("room in old gen");
            // SAFETY: a fresh, zeroed block sized for a header and 4 slots.
            unsafe { (*(p as *mut ObjectHeader)).set_num_slots(4) };
            p as usize
        };
        // The young stretch: an object whose header claims far more than
        // from-space holds (the walk cannot parse it), carrying `x + 8`.
        let tail = heap.alloc_object(ClassId::new(0), 8);
        // SAFETY: `tail` is a live young object of 8 legacy slots; its first
        // slot word and its header are inside it.
        unsafe {
            std::ptr::write(
                (tail.as_ptr() as usize + HEADER_SIZE) as *mut u64,
                (x + 8) as u64,
            );
            (*(tail.as_ptr() as *mut ObjectHeader)).set_num_slots(1 << 20);
        }
        let young = heap.lock_young_from();
        let og = heap.old_gen.lock();
        let walked = og.walk_objects();
        assert!(walked.iter().any(|&(p, _)| p as usize == x), "x is walked");
        // Re-derived at each use: the marker writes the header through its
        // own `&mut` in between.
        // SAFETY: `x` is the old-gen object carved above, alive for the test.
        let marked = || unsafe { &*(x as *const ObjectHeader) }.gc_flags() & GC_FLAG_MARKED != 0;

        let mut plain = Vec::new();
        GenerationalHeap::mark_young_to_old_refs(&young, &og, &walked[..], &[], &mut plain, None);
        assert!(plain.is_empty(), "no plan: an interior word marks nothing");
        assert!(!marked());

        let mut worklist = Vec::new();
        let mut pins = Vec::new();
        GenerationalHeap::mark_young_to_old_refs(
            &young,
            &og,
            &walked[..],
            &[],
            &mut worklist,
            Some(&mut pins),
        );
        assert!(
            pins.contains(&x) && pins.iter().all(|&p| p == x),
            "pins {pins:?}"
        );
        assert_eq!(
            worklist.iter().filter(|&&p| p as usize == x).count(),
            1,
            "pushed once for the BFS"
        );
        assert!(marked(), "marked");
        // SAFETY: as above.
        unsafe { &*(x as *const ObjectHeader) }.clear_gc_flags(GC_FLAG_MARKED);
    }
}
