#[allow(unused_imports)]
use super::*;

impl GenerationalHeap {
    /// Run one complete NON-MOVING young collection cycle (the divert path of
    /// `collect_garbage_inner`): young mark-sweep, threshold-gated in-place
    /// old sweep, and the monitor-registry remap for selective promotions.
    ///
    /// Its ONE caller is `collect_garbage_inner`'s up-front divert. (gen
    /// r4w3/young2, 2026-09-23: this doc said it was extracted so the moving
    /// path could also divert here MID-FLIGHT when its object-start walk
    /// cannot complete — the cce0079 fix. That divert no longer exists: an
    /// incomplete walk now REFUSES the whole cycle, because on that precise-root
    /// path the sweep lacks the conservative over-marking it needs (the
    /// HIB-CV-22/32/33 family, see the `!start_walk_complete` arm). Anything
    /// that must happen on every non-moving cycle — the stress-trigger epoch
    /// below, the promote-on-pressure carry in the divert — can rely on this
    /// single entry. gen r4w4/young4: the carry is gone — the divert now
    /// CONSUMES the arm and hands it here as `promote_pressure`.)
    pub fn run_non_moving_young_cycle(
        &self,
        roots: &mut [ObjectRef],
        finalizer_addrs: &[usize],
        monitors: &dyn MonitorCleanup,
        promote_pressure: bool,
    ) -> (GcResult, Vec<usize>) {
        // Fresh cycle: no old-gen reclamation has happened yet (see
        // `gc_quiescence::OLD_GEN_RECLAIMED`). The moving path resets this in
        // `collect_garbage_inner`; this path can be entered directly.
        cratonvm_gc_core::gc_quiescence::set_old_gen_reclaimed(false);
        // gcg g6/yg: a fresh cycle has no mark.
        let _ = self.take_escalation_mark();
        // gch h4/wr: nor a true-root major's watched verdict (this path can be
        // entered directly, past `collect_garbage_inner_with_pins`' clear).
        self.clear_true_root_watched_left_out();
        // `promote_pressure` (gen r4w4/young4): the promote-on-pressure arm the
        // divert consumed; see `sweep_young_non_moving`'s selective walk.
        let mut result = self.sweep_young_non_moving(roots, finalizer_addrs, promote_pressure);
        // gen r4w2/youngmark: tell the `CRATONVM_DBG_GC_STRESS` trigger that the
        // cycle `minor_gc_count` now counts was a non-moving one, whose `used`
        // is a high-water mark rather than an occupancy. See
        // `gc_stress_decision`. One relaxed store; read only under stress.
        self.young_stress_epochs[0].store(
            self.stats.minor_gc_count.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        // Selective promotion commits young -> old relocations inside
        // `sweep_young_non_moving`. Registered external-root providers can own
        // the only reference to a promoted object (collection overlay backing
        // arrays are the canonical case), and the System.gc path below may run
        // an old-space sweep in this SAME collector call. Waiting for the VM's
        // ordinary post-GC remap is therefore too late: the old marker would
        // consult a provider that still points at the forwarded young source,
        // fail to seed the promoted destination, and immediately reclaim it.
        //
        // Publish provider relocation at the young-phase commit boundary,
        // before any same-cycle old marking. The VM-level remap after this
        // function returns remains intentionally idempotent and still covers
        // every non-provider root owner.
        cratonvm_gc_core::external_roots::remap_external_roots(&result.0.pointer_map);
        // OOM-INVESTIGATE (dohead-oom, 2026-07-19): track young-arena usage
        // across cycles to find where reclaimed bytes stop coming back as
        // usable free space. Gated so normal runs pay nothing.
        if gc_flags().dbg_heap_trace {
            static CYCLE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = CYCLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (used, free_bytes, cap) = {
                let from = self.lock_young_from();
                (from.used(), from.free_list_bytes(), from.capacity())
            };
            let (old_used, old_cap) = {
                let og = self.old_gen.lock();
                (og.used(), og.capacity())
            };
            eprintln!(
                "[HEAP-TRACE] cycle={n} young_used={used} young_free_list={free_bytes} \
                 young_cap={cap} old_used={old_used} old_cap={old_cap} bytes_freed={} objects_copied={}",
                result.0.stats.bytes_freed, result.0.stats.objects_copied,
            );
        }
        // The JIT-active path cannot use the ordinary old-gen compactor:
        // conservative JIT stack/register roots cannot be rewritten when an
        // old object moves.  Previously this early return therefore skipped
        // old collection altogether.  Long allocation-heavy runs eventually
        // filled old space with dead objects promoted by the selective young
        // sweep (or spilled there by an allocation slow path), making depth
        // 20 Binary Trees OOM despite a small live tree.  Sweep old space in
        // place once it reaches the same pressure threshold as the compacting
        // path.  The shared marker retains every conservative root, while
        // the non-moving sweep only returns unreachable blocks to OldGen's
        // free lists and never invalidates a raw JIT pointer.
        // In this path a pending System.gc() must still sweep old gen,
        // even below the normal occupancy threshold. Consume the request
        // here because the moving Phase-5 check below is skipped.
        let major_requested = cratonvm_gc_core::gc_quiescence::take_major_gc_request();
        let old_capacity = self.old_gen_capacity();
        // CRATONVM_OLD_SWEEP_JIT=0 opts out of the in-place old sweep on
        // this conservative-roots path (diagnostic escape hatch / A-B
        // bisection knob for suspected live-object reclaims — the young
        // sweep survives an imperfect root set via conservative
        // over-marking and side-mark containment, but this old sweep
        // frees purely on GC_FLAG_MARKED, so any root-set gap frees a
        // LIVE promoted object). Read once per GC cycle — not hot.
        let old_sweep_enabled = gc_flags().old_sweep_jit;
        if old_sweep_enabled
            && old_capacity > 0
            && self.major_trigger_decision(&mut self.old_gen.lock(), major_requested)
        {
            // gcg g6/yg: the young sweep of this pause is done; an unrequested
            // major starts here (the `Copy` bean books the half before it).
            self.note_escalation_mark(major_requested);
            // gc-common w36-d: with the old generation's finalizer
            // resurrection. `sweep_young_non_moving` judged only the YOUNG
            // candidates; a finalizable that died old is kept here and joins
            // `result.1` BEFORE the pinned-compaction rewrite below.
            let mut old_fin_pass = OldGenFinalizerPass::new(finalizer_addrs);
            // gcj j3/rf3 (ec47, the legacy seed): the young sweep's
            // resurrection set, for a legacy-seeded major to defer.
            old_fin_pass.young_resurrected = self.young_resurrection_members(&result.0.pointer_map);
            // gcd d4/n (`gcd-d4j-stw-major-keeps-dead-old-data-through-young-nepotism`):
            // a REQUESTED major (`System.gc()`, the allocation ladder) seeds
            // young from the true roots (`TrueRootYoung`), so dead old data a
            // young survivor names is freed in this collection, not the next.
            // `CRATONVM_GC_FULL_GC_TRUE_ROOTS=0` keeps the legacy seed.
            // gcf f2/c: the requested-ness rides along on its own bit
            // (`CRATONVM_GC_REQUESTED_MAJOR_COMPACTS` needs it with the
            // true-root seed off too).
            // gcf f4/c: `CRATONVM_GC_FULL_GC_TRUE_ROOTS_ALL_MAJORS` (default
            // ON since gch h3/mj, `=0` kills) arms the seed for an occupancy
            // major too.
            let _true_roots = TrueRootMajorScope::arm_major(
                major_requested,
                full_gc_true_roots_enabled(),
                true,
            );
            // gci i13/vg (`CRATONVM_GC_VT_PARKED_GATHER_FILTER`): the parked
            // continuations the VM's split gather left out of `roots` (none of
            // their words is young, so the young sweep above needed none) are
            // roots of this old sweep. Appended after `roots`, so they sit
            // before the true-root ask's promotion tail (`promo_tail` is the
            // slice's length at the call). No copy without a staged section.
            let parked_old_only = self.take_parked_old_only_roots();
            let merged_roots: Option<Vec<ObjectRef>> = (!parked_old_only.is_empty()).then(|| {
                let mut v: Vec<ObjectRef> = Vec::with_capacity(roots.len() + parked_old_only.len());
                v.extend_from_slice(roots);
                v.extend(parked_old_only.iter().map(|&a| {
                    // SAFETY: a word of a parked thread's root snapshot (held
                    // there as a non-null `ObjectRef`); the old sweep's root
                    // loop screens it against the old-gen grid before use.
                    unsafe { ObjectRef::from_raw(a as *mut u8) }
                }));
                v
            });
            let (old_freed, old_survivors) = self.sweep_old_gen_non_moving_finalizing(
                merged_roots.as_deref().unwrap_or(&*roots),
                &result.0.pointer_map,
                &mut old_fin_pass,
            );
            // gcd d9/a: a true-root major resolved this pause's promotions
            // instead of rooting every destination, and freed the ones nothing
            // reached; their young sources (zeroed on the young free list by
            // the young sweep) are dead, so they leave the map every
            // post-collection consumer reads.
            let true_root_out = TRUE_ROOT_SWEEP_OUT.with(|c| std::mem::take(&mut *c.borrow_mut()));
            for src in &true_root_out.dead_sources {
                result.0.pointer_map.remove(src);
            }
            // gch h4/wr (`CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD`, default ON since gci i1/og and jr20 wave 9):
            // the watched young survivors the true-root trace left out are
            // dead too (kept in place only for a dead old holder's card):
            // their identity entries leave the map, and the heap answers
            // "did not survive" for them until the next collection starts.
            // Empty with the switch off.
            if !true_root_out.watched_left_out.is_empty() {
                let n = self.install_true_root_watched_left_out(
                    true_root_out.watched_left_out,
                    &mut result.0.pointer_map,
                );
                if watchref_dbg() {
                    eprintln!(
                        "[watchref] true-root major: {n} watched young survivor(s) left out by the trace, judged dead (total {})",
                        self.true_root_watched_left_out_total(),
                    );
                }
            }
            result.1.extend_from_slice(&old_fin_pass.resurrected);
            self.note_resurrection_closure(&old_fin_pass.closure);
            // This cycle DID reclaim old-gen storage: a freed block is back on
            // the free list, so an old-gen address is no longer self-evidently
            // live. `old_survivors` carries the identity entries that make
            // "absent from the map" an exact death proof for watched addresses.
            cratonvm_gc_core::gc_quiescence::set_old_gen_reclaimed(true);
            // gen r4w5/oldcompact5: a pinned compaction (opt-in,
            // `CRATONVM_GC_OLD_PINNED_COMPACT`) may have MOVED old objects,
            // including this cycle's promotion destinations. Chain every
            // `young -> destination` entry through to the final address and
            // report the finalizer list at its final addresses, exactly as the
            // moving path composes `pointer_map` with its `compact_map` — the
            // VM applies the merged map ONCE per slot. Without a move the map
            // holds identity entries only, and this is skipped.
            if old_survivors.iter().any(|(from, to)| from != to) {
                for dest in result.0.pointer_map.values_mut() {
                    if let Some(&final_addr) = old_survivors.get(dest) {
                        *dest = final_addr;
                    }
                }
                for addr in result.1.iter_mut() {
                    if let Some(&final_addr) = old_survivors.get(addr) {
                        *addr = final_addr;
                    }
                }
            }
            result.0.pointer_map.extend(old_survivors);
            result.0.stats.bytes_freed += old_freed;
            self.stats
                .bytes_freed_old
                .fetch_add(old_freed as u64, Ordering::Relaxed);
            self.stats.major_gc_count.fetch_add(1, Ordering::Relaxed);
        } else if old_capacity > 0 {
            // gen r4w6/oldpin6: no old-gen collection this pause — run the
            // resize a concurrent cycle's sweep left due, if any.
            let mut og = self.old_gen.lock();
            if Self::resize_old_gen_after_concurrent_sweep(&mut og) {
                // gen r5w3/oldgen7: and its interior half (opt-in); gen
                // r5w6/old10: retried at the next pause when damped.
                self.decommit_old_interior_after_concurrent_resize(&mut og);
            }
            // gcj j2/og2 B4: the free-ratio growth that sweep left due; the
            // capacity compare below cedes and republishes.
            self.grow_old_gen_after_concurrent_sweep(&mut og);
        }
        // gen r5w3/oldgen7 (`CRATONVM_GC_OLD_BORROW_YOUNG`): an old-gen
        // collection above that grew the generation into its headroom
        // (`OldGen::grow_after_refusal`) moved its end. This path does not
        // republish the region bounds at its end as the moving path does, and
        // the lock-free screens read them, so republish here, before mutators
        // resume — and cede the young budget the growth took. `old_capacity`
        // is the capacity before the collection; nothing else changes it.
        let old_capacity_now = self.old_gen_capacity();
        if old_capacity_now != old_capacity {
            self.cede_young_budget_to_old(old_capacity_now);
            self.refresh_region_bounds();
        }
        // BUG-V fix: the non-moving sweep still *relocates* objects via
        // selective promotion (young→old, see `selective_on` in
        // `sweep_young_non_moving`). Those relocations land in
        // `result.0.pointer_map`, and the moving path below remaps the
        // monitor registry with exactly that map at the
        // `monitors.remap_after_gc(&pointer_map)` call. This early return
        // used to skip it, so a `synchronized`-inflated object that got
        // selectively promoted kept its monitor-registry entry keyed to its
        // *old* young address while its copied mark word (at the new old-gen
        // address) still read INFLATED. The next `enter`/`lookup_inflated`
        // at the new address missed the registry → `inflate_locked`
        // returned the "mark inflated but registry entry missing" Err → the
        // `.expect(...)` panicked (monitor.rs registry/mark-word desync).
        // Re-key the registry here, identically to the moving path.
        monitors.remap_after_gc(&result.0.pointer_map);
        // DBG (CRATONVM_DBG_YOUNGSTATE): post-collection young/old arena
        // state — the bimodal-bt18 discriminator (is young allocatable
        // after this sweep, and from which structure?).
        if gc_flags().dbg_youngstate {
            let from = self.lock_young_from();
            let og = self.old_gen.lock();
            eprintln!(
                "[youngstate] post-sweep used={}/{} free_list={} largest_free={} old={}/{}",
                from.used(),
                from.capacity(),
                from.free_list_bytes(),
                from.largest_free_block(),
                og.used(),
                og.capacity(),
            );
        }
        result
    }

    pub fn collect_garbage_inner(
        &self,
        roots: &mut [ObjectRef],
        finalizer_addrs: &[usize],
        monitors: &dyn MonitorCleanup,
    ) -> (GcResult, Vec<usize>) {
        // gci i1/yg: the HotSpot age clock brackets every collection (both
        // public doors reach `collect_garbage_inner` and nothing else does);
        // both calls return at once on a heap without the clock.
        // gc wave 9 (lane gcC): the aging epoch brackets the production young
        // cycle too (no-ops on an unarmed heap; at most one of the two is
        // armed, see `arm_serial_eden_trigger`).
        self.age_clock_begin();
        self.open_young_age_epoch();
        let out = self.collect_garbage_inner_with_pins(roots, finalizer_addrs, monitors, None);
        self.close_young_age_epoch();
        self.age_clock_end();
        // gci i13/vg: a staged old-only root section belongs to this
        // collection only; a cycle that ran no major (or refused) leaves it
        // unconsumed, and it must not reach a later collection's major.
        self.parked_old_only_roots.lock().clear();
        out
    }

}
