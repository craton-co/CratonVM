#[allow(unused_imports)]
use super::*;

impl ZgcRealHeap {
    /// Compact the low end of the arena at a stop-the-world, after the mark.
    /// Phase 4 of the ZGC maturity plan.
    ///
    /// # What it does
    ///
    /// Slides every marked survivor in the small-object region down into the
    /// lowest free space, in address order; then rewrites **every reference
    /// slot in every survivor** through the resulting `from -> to` map; then
    /// drops the bump cursor to the end of the compacted region. That last
    /// step is the entire point -- a non-compacting arena's cursor is a
    /// one-way ratchet, and this is the only operation that can return the
    /// middle of the heap to one contiguous run.
    ///
    /// Returns `(objects_moved, bytes_reclaimed, pointer_map)`.
    ///
    /// # Three gates, all of which must be open
    ///
    /// 1. `CRATONVM_ZGC_RELOCATE=1` -- *intent* ([`Self::relocation_requested`]).
    /// 2. `zgc_relocation_permitted` -- *safety*. Refuses when the JIT is
    ///    enabled **and** `x64::zgc_codegen_honours_read_barrier()` is false:
    ///    compiled code that loads a reference field without routing through a
    ///    barriering accessor would be handed stale pointers into evacuated
    ///    objects. Since stage (a) of `zgc-jit-load-barrier.md` landed
    ///    (2026-08-13) that predicate answers `true` unconditionally --
    ///    `jit_getfield` / `jit_putfield_object` barrier -- so **a default JIT
    ///    run is permitted** and the per-cycle guard is the coverage proof
    ///    (`relocate_stw_admitted`), not this gate. That gate lives in
    ///    `vm_init` and is tested. *(This bullet used to say the gate refuses
    ///    whenever the JIT is enabled; see
    ///    `docs/feature-designs/zgc-architecture.md`.)*
    /// 3. A stop-the-world token, held by the caller for the whole call.
    ///
    /// # What it deliberately is not
    ///
    /// Not *concurrent* relocation. Every mutator is stopped, so no load
    /// barrier is needed to observe the move -- which is exactly why this step
    /// is reachable before barrier emission lands, and why it is an honest
    /// first cut rather than a stand-in for `zgc::relocate`.
    ///
    /// **The high end is not compacted.** Large objects bump down from
    /// capacity with their own free list; moving them needs the same slide in
    /// the opposite direction against a different free structure. Left out on
    /// purpose -- the low end is where TLAB chunks fragment, which is the
    /// measured problem.
    ///
    /// # The caller's remaining obligation
    ///
    /// The returned `PointerMap` is **non-empty**, and `VmHeap::Zgc`'s arms
    /// assert non-moving throughout. Every consumer of a raw heap address
    /// outside this heap -- JIT frame maps, monitor tables, external root
    /// providers, native side tables -- must be remapped through it, exactly as
    /// the generational and G1 paths already do. Auditing those arms is the
    /// reason this stays behind a default-off flag instead of being wired into
    /// `collect_garbage`'s normal path.
    /// # Why the live set is a PARAMETER
    ///
    /// It reads mark bits in no version of this function, and that is
    /// deliberate. The first draft filtered `registered` by `GC_FLAG_MARKED`
    /// and moved **nothing**, because the sweep clears every survivor's mark
    /// bit before returning -- so by the time a post-sweep compaction runs,
    /// the marks it wanted are gone. Taking the set explicitly makes the
    /// caller say which objects are live instead of inferring it from state
    /// another phase owns: `collect_garbage` passes the post-sweep registry
    /// (everything the sweep did not reclaim), and a test passes whatever it
    /// built.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn relocate_stw_for_test(
        &self,
        live: &[usize],
    ) -> (usize, usize, cratonvm_types::PointerMap) {
        self.relocate_stw(live)
    }

    /// Compact the arena's LARGE-OBJECT end — the half of the heap
    /// `relocate_stw`'s slide has never been able to reach.
    ///
    /// # Why a second compactor, and not a wider first one
    ///
    /// The arena has two ends (`Arena::high_cursor`): the low one bumps up and
    /// holds everything a TLAB can serve, the high one bumps DOWN from
    /// `capacity` and holds objects at or above `ZGC_LARGE_OBJECT_MIN`. The
    /// slide above compacts the low end only — `logical_pages` builds
    /// candidates over `base .. base + used_low_for_compaction()`, and
    /// `used_low_for_compaction` is the low cursor by construction.
    ///
    /// That was not an oversight so much as an unmeasured assumption, and
    /// `org.h2.test.store.TestMVStoreTool` is where it came due. Its failing
    /// request is **262 160 bytes**, above `ZGC_LARGE_OBJECT_MIN`, so it is
    /// served from the high end — and the window the fragmentation profile
    /// named as cheapest to empty began **3.9 MB above** the low cursor:
    ///
    /// ```text
    /// [zgc-target] recorded window start=1072365328 end=1072627680 width=262352
    ///              request=262160 used_low=1068498592 in_low_region=false
    /// ```
    ///
    /// So targeted compaction (`CRATONVM_ZGC_TARGETED_COMPACTION`) selected the
    /// right window and could evacuate nothing in it: `targeted_pages` read 0
    /// on every workload measured. Large-object fragmentation had no repair at
    /// all. This is that repair.
    ///
    /// # The geometry is the low slide's, mirrored
    ///
    /// Survivors are packed against `capacity` in DESCENDING address order, so
    /// an object is only ever copied into space a higher-addressed survivor has
    /// already vacated — the mirror of the low slide's ascending walk, and just
    /// as load-bearing: out-of-order copying overwrites a survivor that has not
    /// moved yet, which is silent heap corruption rather than a failed assert.
    ///
    /// There are no logical pages up here and no relocation-set selector, and
    /// neither is missing. A page is an accounting unit for "which parts of a
    /// million-object region pay for their copy"; this region holds thousands
    /// of objects at most (that is the definition of the population it serves),
    /// the copy is all-or-nothing, and the profitability question is asked once
    /// for the whole region by `gain` below instead of once per page.
    ///
    /// # What it costs when it declines
    ///
    /// One `high_free_shape()` — three field reads and a sum over a list whose
    /// length is measured in tens. `high_compaction_declined` counts the
    /// declines, so a run in which this never engaged says so rather than
    /// looking clean.
    pub fn compact_high_region(
        &self,
        arena: &mut Arena,
        live: &[usize],
        pins: &[usize],
        pairs: &mut Vec<(usize, usize)>,
    ) -> (usize, usize) {
        if !self.high_compaction_enabled.load(Ordering::Relaxed)
            || self.external_gpu.gpu_relocation_forbidden()
        {
            return (0, 0);
        }
        let base = arena.base_ptr() as usize;
        let high_lo = base + arena.high_cursor();
        let high_hi = base + arena.capacity();
        if high_lo >= high_hi {
            // The high end has never been used, which is the state every
            // workload that allocates no large object stays in forever.
            return (0, 0);
        }

        // ---- IS IT WORTH THE COPY? ------------------------------------
        //
        // The gain from merging this end's holes is bounded below by
        // `free_bytes - largest`: after the pass every free byte the slide can
        // reach is in one block, so the largest block grows by at least that
        // much. Under one large object's worth of gain there is nothing here a
        // large allocation could use that it could not already have, and the
        // pass would be pure copying — so decline, and COUNT the decline.
        let (blocks_before, free_bytes, largest) = arena.high_free_shape();
        let gain = free_bytes.saturating_sub(largest);
        if gain < ZGC_LARGE_OBJECT_MIN {
            self.counters
                .high_compaction_declined
                .fetch_add(1, Ordering::Relaxed);
            return (0, 0);
        }

        // Survivors, HIGHEST FIRST — see the header.
        let mut survivors: Vec<usize> = live
            .iter()
            .copied()
            .filter(|b| *b >= high_lo && *b < high_hi)
            .collect();
        survivors.sort_unstable_by(|a, b| b.cmp(a));

        // A pin makes its OBJECT immovable, and object granularity is right
        // here where the low slide's page granularity was right there: there
        // are no pages up here to withhold, and the population is small enough
        // that a linear probe per survivor costs nothing beside the memmove.
        // The set is the one the low slide consults — JNI critical sections
        // plus the CONSERVATIVE JIT root scan, whose entries may be interior
        // pointers or plain `long`s, so the object they name must not move even
        // though nothing can rewrite the slot they came from.
        // zs s2/sl2 (`CRATONVM_ZGC_SLIDE_PIN_BY_BASE`, default ON, `=0` kill):
        // ONE-PAST-THE-END pins its object too, the rule of
        // `resolve_interior_for_pin` (`addr <= end`) and of the low end's
        // by-base screen. A compiled loop's end cursor over an array is
        // exactly such a word; the high slide used to test `p < from + size`
        // (and filter `p < high_hi`), so it moved the array the cursor bounds.
        let end_inclusive = zgc_slide_pin_by_base_enabled();
        let mut high_pins: Vec<usize> = pins
            .iter()
            .copied()
            .filter(|p| *p >= high_lo && (*p < high_hi || (end_inclusive && *p == high_hi)))
            .collect();
        // zt t1/rl (`zgc-zs2sl2-slide-review-residuals` item 3): sorted once,
        // so each survivor's pin test is a `partition_point` instead of a
        // scan of every pin (O(survivors x pins) before). Same answer.
        high_pins.sort_unstable();

        let first_pair = pairs.len();
        // Offsets, because that is what `Arena::compact_high_to` speaks.
        let mut vacated: Vec<(usize, usize)> = Vec::new();
        let mut floor = high_lo;
        let mut dest = high_hi;
        let mut moved = 0usize;
        let mut copied = 0usize;
        let mut pinned = 0usize;
        for from in survivors {
            // zs s1/dg: `survivor_size`, so a real array past 1 << 28 elements
            // is sized (and, past `MAX_ALLOC_EXTENT`, stays put below) instead
            // of stopping this slide for every survivor under it.
            let Some(size) = self.survivor_size(from, self.header_ref(from as *mut u8)) else {
                // A header this collector cannot size cannot be moved, and its
                // EXTENT is what the vacated span below it would be measured
                // from — so nothing below it can be claimed dead either. Stop,
                // and make the packed region's current floor the authoritative
                // floor: everything below is left exactly as it was, free list
                // included. The low slide documents the same break as an
                // amplifier; here it under-reclaims instead, which is the
                // conservative direction.
                tracing::warn!(
                    target: "cratonvm::gc::guard",
                    addr = from,
                    "zgc relocate: unsizable survivor stops the HIGH slide"
                );
                floor = dest;
                break;
            };
            let size = size.max(8);
            if from.saturating_add(size) > dest {
                // An extent that runs into the region already packed above it
                // — `dest` starts at `capacity`, so this also catches one that
                // runs past the top of the arena — is impossible for a live
                // object met in descending address order, and means the header
                // is not one. Refuse it rather than let it drive a memmove:
                // the same answer the low slide gives a survivor whose extent
                // runs past the bump cursor.
                tracing::warn!(
                    target: "cratonvm::gc::guard",
                    addr = from,
                    size,
                    dest,
                    high_hi,
                    "zgc relocate: a HIGH survivor's extent overlaps the packed \
                     region -- refusing to compact past it"
                );
                floor = dest;
                break;
            }
            // An extent past `MAX_ALLOC_EXTENT` (a real array over 256 MiB,
            // sizable since zr z1/rl) stays put: the interior pin probes scan
            // only that far below an address, so a derived pointer deep inside
            // it could not have pinned it. See `MAX_ALLOC_EXTENT`.
            let immovable = size > Self::MAX_ALLOC_EXTENT
                || sorted_pin_in_extent(&high_pins, from, size, end_inclusive);
            // Pack against `dest` on the object grid. The DELTA is aligned, not
            // the address: nothing guarantees the arena's base is 8-aligned,
            // every object start is `base + 8k`, so rounding the difference is
            // what keeps the destination on the same grid — and, because it
            // rounds the move DOWN, keeps `to >= from` without a second check.
            let mut to = if immovable {
                pinned += 1;
                from
            } else {
                from + ((dest - size - from) & !7)
            };
            // COMMIT THE DESTINATION FIRST. It is free space, and free space is
            // exactly what `Arena::decommit_free_blocks` hands back to the OS —
            // on the promise that everything which re-issues it goes through
            // `hand_out`. A slide does not: it picks `to` arithmetically and
            // memmoves. So the first slide after a give-back wrote into a
            // `PROT_NONE` granule and died inside `memcpy`, which is what
            // `zgc-relocation-slides-wrote-into-decommitted-granules-FIXED-20260904.md`
            // recorded as "the fault address is always a page boundary, `rdi`
            // equal to it, fault pc inside libc".
            if to != from && !arena.commit_for_relocation(to - base, size) {
                tracing::warn!(
                    target: "cratonvm::gc::guard",
                    addr = from,
                    size,
                    dest = to,
                    "zgc relocate: the OS refused the HIGH slide's destination -- leaving the survivor in place"
                );
                pinned += 1;
                to = from;
            }
            if to != from {
                // SAFETY: `size` bytes are live at `from`; `to` is inside the
                // high region, strictly above `from`, and `to + size <= dest <=
                // high_hi`. The commit above proved `[to, to + size)` is mapped
                // read-write. The regions may overlap — `copy` is memmove, which
                // is correct in this direction.
                unsafe { std::ptr::copy(from as *const u8, to as *mut u8, size) };
                pairs.push((from, to));
                moved += 1;
                copied += size;
            }
            // Whatever lies between this object's end and the floor of the run
            // above it is dead: every survivor that was there has just been
            // packed above, and anything else was free already. Recorded per
            // object rather than once at the end, because a pinned survivor
            // splits the region in two and a single span would either swallow
            // it or abandon the free space on its far side — the memory loss
            // `Arena::compact_low_to` records having made once already.
            if to + size < dest {
                vacated.push((to + size - base, dest - base));
            }
            dest = to;
        }
        // The walk reached the bottom, so the span below the lowest survivor is
        // vacated too. (When it broke early, `floor` is the packed region's own
        // bottom and there is nothing further to claim.)
        if floor == high_lo && dest > high_lo {
            vacated.push((high_lo - base, dest - base));
        }

        // ---- NOTHING LIVE MAY REMAIN INSIDE A VACATED SPAN -------------
        //
        // `compact_high_to` zeroes every span it is handed and publishes it as
        // free. A live object still inside one would be erased AND its address
        // re-issued while the object-start registry still named it — the
        // failure the low slide's `live_ceiling` check exists to prevent, from
        // the other end.
        //
        // Derived from a second pass over `live` rather than from the loop's
        // own bookkeeping, for the reason that check gives: every argument that
        // the loop's spans miss every survivor is an argument about the
        // partition, not a check on the answer. Cheap beside the slide, and it
        // can only DROP spans — i.e. reclaim less.
        let moved_to = relocate::ZForwardIndex::from_pairs(&pairs[first_pair..]);
        let mut survivor_now: Vec<usize> = live
            .iter()
            .copied()
            .filter(|b| *b >= high_lo && *b < high_hi)
            .map(|b| moved_to.resolve(b) - base)
            .collect();
        survivor_now.sort_unstable();
        let spans_before = vacated.len();
        vacated.retain(|&(s, e)| match survivor_now.binary_search(&s) {
            // A survivor based exactly at the span's start settles it.
            Ok(_) => false,
            Err(i) => {
                // The greatest base below `s` may still extend into the span,
                // and the least base at or above `s` may start inside it.
                let below_clear = i == 0 || {
                    let b = survivor_now[i - 1];
                    let sz = self
                        .survivor_size(b + base, self.header_ref((b + base) as *mut u8))
                        .unwrap_or(usize::MAX);
                    b.saturating_add(sz) <= s
                };
                below_clear && survivor_now.get(i).is_none_or(|&b| b >= e)
            }
        });
        if vacated.len() != spans_before {
            tracing::error!(
                target: "cratonvm::gc::guard",
                dropped = spans_before - vacated.len(),
                spans = spans_before,
                moved,
                "zgc relocate: a HIGH vacated span still covered a live object — \
                 dropping it and reclaiming less"
            );
        }

        let reclaimed = arena.compact_high_to(floor - base, &vacated);
        self.counters
            .high_compaction_cycles
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .high_objects_relocated
            .fetch_add(moved, Ordering::Relaxed);
        self.counters
            .high_bytes_copied
            .fetch_add(copied, Ordering::Relaxed);
        if dbg_zgc_high() {
            let (blocks_after, bytes_after, largest_after) = arena.high_free_shape();
            eprintln!(
                "[zgc-high] region={} moved={moved} copied={copied} pinned={pinned} \
                 spans={} reclaimed={reclaimed} \
                 before=(blocks={blocks_before} bytes={free_bytes} largest={largest}) \
                 after=(blocks={blocks_after} bytes={bytes_after} largest={largest_after})",
                high_hi - high_lo,
                vacated.len(),
            );
        }
        (moved, reclaimed)
    }

    /// Refusals, then the slide: what `collect_garbage` did in one call until
    /// round 9 wave 6, kept for the tests that drive a slide over a live set
    /// they built. `collect_garbage` itself calls the two halves separately so
    /// it can skip the live-set snapshot on a refused cycle.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn relocate_stw(&self, live: &[usize]) -> (usize, usize, cratonvm_types::PointerMap) {
        // zt t33: the slide's words are good (`end_closing_heal_color`).
        self.end_closing_heal_color_for("relocate-stw");
        if let Some(reclaimed) = self.relocate_refusal() {
            return (0, reclaimed, cratonvm_types::PointerMap::default());
        }
        self.relocate_stw_admitted(live)
    }

    /// Set the worker count for THIS heap's compaction passes. `0` or `1` is
    /// serial. See `relocate_workers_for`.
    pub fn set_relocate_workers(&self, n: usize) {
        self.relocate_worker_count
            .store(Self::clamp_sweep_workers(n), Ordering::Relaxed);
    }

    /// `CRATONVM_ZGC_PAR_RELOCATE` -- the seed for `relocate_worker_count`.
    ///
    /// Unset: [`ZGC_PAR_RELOCATE_DEFAULT_WORKERS`]. `0` or an off-word (`off`,
    /// `false`, `no`, empty): serial. `1` or any other word (`on`, `true`, the
    /// `CRATONVM_GC=zgc-par-relocate` token): [`ZGC_PAR_RELOCATE_AUTO_WORKERS`].
    /// A number of 2 or more: that many workers. Clamped to the core count like
    /// the sweep's workers, because this is a stop-the-world phase and
    /// oversubscription only adds context switches inside the pause.
    pub fn relocate_workers_requested() -> usize {
        let n = match None::<String> {
            Some(v) => {
                let v = v.trim().to_ascii_lowercase();
                match v.parse::<usize>() {
                    Ok(0) => 1,
                    Ok(1) => ZGC_PAR_RELOCATE_AUTO_WORKERS,
                    Ok(n) => n,
                    Err(_) if v.is_empty() || v == "false" || v == "off" || v == "no" => 1,
                    Err(_) => ZGC_PAR_RELOCATE_AUTO_WORKERS,
                }
            }
            None => ZGC_PAR_RELOCATE_DEFAULT_WORKERS,
        };
        Self::clamp_sweep_workers(n)
    }

    /// zs s3/cr (`CRATONVM_ZGC_EVAC_QUARANTINE`): hand the spans the LAST
    /// evacuating slide withheld to the free list. Called by `collect_garbage`
    /// with the world stopped, after the mark and BEFORE the sweep, so every
    /// sweep arm treats them as memory that was already free: the complement
    /// sweep recomputes the list over them (it replaces the list, so nothing is
    /// listed twice), the per-object sweeps never visit them (no registered
    /// base lies inside). Returns the bytes released.
    ///
    /// Checked rather than trusted: a span is released only if, at this
    /// moment, no free block overlaps it and no registered base lies inside
    /// it. Either would mean something already claimed those bytes (a path
    /// that swept or allocated while the quarantine was held); listing them
    /// again would hand one span out twice, so such a span is DROPPED and
    /// counted (`evac_quarantine_stats[4]`) -- the next complement sweep
    /// recovers whatever of it is really free. Pieces smaller than one object
    /// are left to that sweep as well. zt t2/rv (zs4rv3 item 6): that bound is
    /// "the next BITMAP (complement) sweep", which runs only on a full cycle
    /// with `sweep_floor == 0`; until one runs, a dropped span and a sub-object
    /// piece are out of the free list (never handed out twice, only late).
    pub fn release_evac_quarantine(&self) -> usize {
        // zt t23/zc7 (R3, `CRATONVM_ZGC_REGION_QUARANTINE`): the region
        // relocation cycle kept since the last slide ends HERE too, with its
        // from-space, whichever sweep arm follows (zc2's deferred one
        // included: this runs before the split). Taken first, so no
        // stale-address lookup reads it once its spans can be re-issued; read
        // by the dead-at-release census below, then retired (its pairs handed
        // to `relocations` under `=1`). One acquire load when none is kept.
        // zt t32 (6a): an R4 cycle a generational slide kept outlives every
        // YOUNG mark end -- such a mark heals only the slots it visits, never
        // the old generation's -- and is released, with its quarantine, at
        // the next MAJOR mark end.
        if self.counters.rim_keep_until_major.load(Ordering::Relaxed)
            && self.counters.rim_cycle_young.load(Ordering::Relaxed)
        {
            self.counters.rim.note_kept_past_young();
            return 0;
        }
        self.counters
            .rim_keep_until_major
            .store(false, Ordering::Relaxed);
        let region_cycle = self.take_region_cycle();
        // zt t32 (R6): the plan published since the last closing pause ends
        // here too, before either sweep arm: drained, withdrawn, its regions
        // back on the free list. A no-op when none is published.
        self.release_concurrent_relocate();
        let spans = std::mem::take(&mut *self.counters.evac_quarantine.lock());
        if spans.is_empty() {
            self.retire_region_cycle(region_cycle);
            return 0;
        }
        // zt t1/rl (zs3cr item 6): the dead-at-release census, BEFORE the
        // arena lock (it reads headers and the registry only). A census, not
        // a gate: the spans are released either way.
        if zgc_dbg_quarantine_dead_check_enabled() {
            let base = self.arena.lock().base_ptr() as usize;
            let (holders, stale, screened, answered, live_stale) =
                self.quarantine_stale_slots(
                &spans,
                base,
                region_cycle.as_ref().map(|k| &*k.cycle),
                true,
            );
            let dc = &self.counters.evac_quarantine_dead_check;
            dc[0].fetch_add(1, Ordering::Relaxed);
            dc[1].fetch_add(holders, Ordering::Relaxed);
            dc[2].fetch_add(stale, Ordering::Relaxed);
            dc[3].fetch_add(screened, Ordering::Relaxed);
            dc[4].fetch_add(live_stale, Ordering::Relaxed);
            self.note_region_cycle_stale_answered(answered);
        }
        self.retire_region_cycle(region_cycle);
        let mut arena = self.arena.lock();
        let base = arena.base_ptr() as usize;
        let cursor = arena.used_low_for_compaction();
        let free = arena.free_blocks_sorted();
        let (mut released, mut bytes, mut dropped) = (0usize, 0usize, 0usize);
        for (s, e) in spans {
            let e = e.min(cursor);
            if e <= s || e - s < cratonvm_types::MIN_OBJECT_SIZE {
                continue;
            }
            let i = free.partition_point(|&(off, sz)| off + sz <= s);
            let overlaps_free = free.get(i).is_some_and(|&(off, _)| off < e);
            let holds_base = self
                .registry
                .nearest_base_within_total(base + e - 1, base + s)
                .is_some();
            let straddled = !holds_base && self.quarantine_span_straddled(base, s);
            if overlaps_free || holds_base || straddled {
                dropped += 1;
                continue;
            }
            arena.add_free_block(s, e - s);
            released += 1;
            bytes += e - s;
        }
        let st = &self.counters.evac_quarantine_stats;
        st[2].fetch_add(released, Ordering::Relaxed);
        st[3].fetch_add(bytes, Ordering::Relaxed);
        st[4].fetch_add(dropped, Ordering::Relaxed);
        bytes
    }

    /// Does an object based BELOW the held span starting at arena offset `s`
    /// (`base` is the arena base) reach into it?
    ///
    /// zt t3/rl (zs4rv3 item 4): probed no further down than the largest
    /// extent the interior pin probe trusts (`MAX_ALLOC_EXTENT`); an
    /// unsizable one fails closed. zt t6/rv3 (`zgc-zt4x3-*` item 5): an array
    /// longer than that window, whose base lies more than `MAX_ALLOC_EXTENT`
    /// below the span, is found through the huge-extent table
    /// ([`Self::huge_extent_containing`], the interior probes' own fallback),
    /// so a span inside its body is no longer released under it.
    pub fn quarantine_span_straddled(&self, base: usize, s: usize) -> bool {
        let in_window = self
            .registry
            .nearest_base_within_total(
                base + s - 1,
                (base + s).saturating_sub(Self::MAX_ALLOC_EXTENT).max(base),
            )
            .is_some_and(|b| {
                self.survivor_size(b, self.header_ref(b as *mut u8))
                    .is_none_or(|sz| b.saturating_add(sz) > base + s)
            });
        in_window || self.huge_extent_containing(base + s, false).is_some()
    }

    /// zt t1/rl (`CRATONVM_DBG_ZGC_QUARANTINE_DEAD_CHECK`; zs3cr item 6): walk
    /// the reference slots of every registered object based OUTSIDE the held
    /// `spans` (arena offsets; `base` is the arena base) and count the
    /// reference words that name an address inside one. `(holders walked,
    /// stale words, screened words)`.
    ///
    /// Why the whole registry and not only the marked set: at the release
    /// (after this cycle's mark, before its sweep) the registry also holds
    /// objects that died since the last slide. Every one of them was either a
    /// survivor of that slide (so its slots were rewritten) or allocated after
    /// it (so it could only have stored an address it read through a
    /// rewritten slot or root). So NO registered object may name a
    /// quarantined address, dead or alive, and the walk needs no mark. A
    /// primitive in a slot a class declares as a reference (the W7-84 family)
    /// can still read as a hit; a cell slot is a reference only under tag 4.
    /// zt t8/rl2 (zt1rl item 6): a hit that is not 8-aligned cannot be an
    /// object base (every base sits on the 8-byte grid), so it is that
    /// primitive and is counted apart (`screened`), not as stale; an aligned
    /// primitive still reads as a hit, and each reported one names its holder
    /// and slot so it can be checked against the class layout.
    /// O(registered slots): debug only.
    ///
    /// zt t23/zc7 (R3): the fourth answer counts the stale words that
    /// `cycle` (the region relocation cycle released with these spans)
    /// answers -- exactly the words R4's remap-in-mark would heal. 0 without
    /// a cycle. zt t31/zc11: the fifth counts the stale words whose holder
    /// this cycle's mark set (`is_marked_addr`): a live slot no barrier or
    /// marker healed.
    ///
    /// t1 Q: `report_dead` -- name dead holders' hits in the error lines too.
    /// The slide's release passes `true` (no registered object may name a held
    /// span there); R6's passes `false`, where an unvisited dead holder keeps
    /// its stale words by design and only a live one is a finding.
    pub fn quarantine_stale_slots(
        &self,
        spans: &[(usize, usize)],
        base: usize,
        cycle: Option<&region_forwarding::ZRelocationCycle>,
        report_dead: bool,
    ) -> (usize, usize, usize, usize, usize) {
        use census::ZCensusHeapView;
        let mut held: Vec<(usize, usize)> = spans
            .iter()
            .map(|&(s, e)| (base.saturating_add(s), base.saturating_add(e)))
            .filter(|&(s, e)| e > s)
            .collect();
        held.sort_unstable();
        let inside = |a: usize| -> bool {
            let i = held.partition_point(|&(_, e)| e <= a);
            held.get(i).is_some_and(|&(s, _)| s <= a)
        };
        let (mut holders, mut stale, mut screened, mut reported) =
            (0usize, 0usize, 0usize, 0usize);
        let (mut answered, mut live_stale) = (0usize, 0usize);
        for obj in self.registry.snapshot().bases() {
            if inside(obj) {
                // A base inside a held span is the release's own drop case.
                continue;
            }
            holders += 1;
            let live = self.is_marked_addr(obj);
            self.reference_slots(obj as u64, &mut |slot| {
                if slot.legacy_tag.is_some_and(|t| t != 4) {
                    return;
                }
                // t1 Q (R-1): a colored word (bit 63, the colored-store heap's
                // encoding `color | (addr - arena_base)`) names
                // `arena_base + offset`; it read as never inside before, so
                // the census was blind on a colored heap (R4 / R6).
                let raw = if vaddr::is_colored_word(slot.raw_word) {
                    self.arena_base
                        .wrapping_add(vaddr::offset_of(slot.raw_word) as usize)
                } else {
                    slot.raw_word as usize
                };
                if slot.raw_word == 0 || !inside(raw) {
                    return;
                }
                if raw % 8 != 0 {
                    screened += 1;
                    return;
                }
                stale += 1;
                if live {
                    live_stale += 1;
                }
                if cycle.is_some_and(|c| c.forwarded(raw).is_some()) {
                    answered += 1;
                }
                if reported < 16 && (live || report_dead) {
                    reported += 1;
                    tracing::error!(
                        target: "cratonvm::gc::guard",
                        holder = obj,
                        holder_live = live,
                        slot_addr = slot.slot_addr,
                        points_to = raw,
                        "zgc quarantine dead-check: a reference slot names an address \
                         inside a quarantined span at its release"
                    );
                }
            });
        }
        (holders, stale, screened, answered, live_stale)
    }

    /// The relocation passes' fan-out: [`zgc_par_chunks`] on scoped threads,
    /// or, under `CRATONVM_ZGC_RELOCATE_POOL=1` (zs s3/cr, opt-in), the same
    /// chunking and the same answers IN CHUNK ORDER on this heap's persistent
    /// [`cratonvm_gc_core::evac_pool::EvacPool`] (built on first use, `workers - 1`
    /// helpers, grown on demand), so a pass stops paying a thread spawn and
    /// join per helper. Counted in `reloc_par_passes`.
    pub fn reloc_par_chunks<T, F>(&self, items: &[usize], workers: usize, f: F) -> Vec<T>
    where
        T: Send,
        F: Fn(&[usize]) -> T + Sync,
    {
        let parallel = workers > 1 && items.len() >= 2;
        if parallel {
            self.counters.reloc_par_passes[0].fetch_add(1, Ordering::Relaxed);
        }
        if !parallel || !zgc_relocate_pool_enabled() {
            if parallel {
                let per = if self.par_bal().balance {
                    ZGC_PAR_CHUNKS_PER_WORKER
                } else {
                    1
                };
                self.counters.reloc_par_passes[1].fetch_add(
                    zgc_par_threads(items.len(), workers, per).saturating_sub(1),
                    Ordering::Relaxed,
                );
            }
            return zgc_par_chunks(self.par_bal(), items, workers, f);
        }
        let per_worker = if self.par_bal().balance {
            ZGC_PAR_CHUNKS_PER_WORKER
        } else {
            1
        };
        // zt t1/rl: `per_heap`, so this pool's dispatches stay out of the
        // process-wide `[GC] evac_pool:` census the generational heap prints
        // (zs3cr item 4); its own census is `pool=` on the `zgc-relocate`
        // line.
        let pool = self
            .counters
            .reloc_pool
            .get_or_init(|| cratonvm_gc_core::evac_pool::EvacPool::per_heap(0, "zgc-reloc"));
        self.counters.reloc_par_passes[2].fetch_add(1, Ordering::Relaxed);
        zgc_par_chunks_pooled(pool, items, workers, per_worker, self.par_bal().cpu, f)
    }

    /// How many workers one compaction pass over `live_len` survivors should
    /// use: the heap's setting, but never so many that a worker gets fewer
    /// than [`ZGC_PAR_RELOCATE_MIN_PER_WORKER`] survivors -- below that the
    /// spawn and join cost more than the pass. Always at least 1.
    pub fn relocate_workers_for(&self, live_len: usize) -> usize {
        let n = self.relocate_worker_count.load(Ordering::Relaxed);
        if n <= 1 {
            return 1;
        }
        n.min(live_len / ZGC_PAR_RELOCATE_MIN_PER_WORKER).max(1)
    }

    /// Every per-cycle refusal `relocate_stw` owes BEFORE it looks at the
    /// live set: `Some(reclaimed)` when this cycle must not slide (the cursor
    /// has already been retracted into the free tail, and the reason counted),
    /// `None` when the slide may run.
    ///
    /// Split out of `relocate_stw` (round 9 wave 6, `zgc6`) so that
    /// `collect_garbage` can ask it BEFORE materialising the post-sweep live
    /// set: a refused cycle used to pay for a registry snapshot and a
    /// `Vec` of every live base (10 M entries on `ZgcTlabStress 8 200`) and
    /// then throw both away. The checks, their order and their counters are
    /// exactly the ones that opened `relocate_stw`.
    pub fn relocate_refusal(&self) -> Option<usize> {
        self.relocate_refusal_with(true)
    }

    /// [`Self::relocate_refusal`], with the cursor retraction of a refused
    /// cycle optional: zt t32 (R6, 6b) asks it in a pause that parks its
    /// sweep, whose install owns the cursor (`Some(0)` there).
    pub fn relocate_refusal_with(&self, retract: bool) -> Option<usize> {
        // A slide rewrites registry words anywhere in the low region: no
        // owned-chunk record may survive it (`vm_tlab::OwnedVmTlabChunk`).
        // `collect_garbage` already bumped; this covers the direct callers.
        vm_tlab::invalidate_owned_vm_tlab_chunks();
        // zr z9/sl: `moved_bytes=` is per cycle; a refused cycle copies 0.
        self.counters.slide_moved_bytes[0].store(0, Ordering::Relaxed);
        // DO NOT RELOCATE WHILE A JIT FRAME IS LIVE.
        //
        // `gc_quiescence::is_active()` means some thread is inside a compiled
        // call right now, and its frame may hold object pointers in REGISTERS
        // or SPILL SLOTS. Those are not slots the collector can find, and they
        // are not slots it can rewrite. `gen_heap` states the rule plainly at
        // its own divert (`gen_heap.rs:5629`): such objects "cannot be
        // relocated (raw register/spill slots can't be rewritten)", so it runs
        // the non-moving sweep instead. G1 reads the same flag. **ZGC read it
        // ZERO times** -- 9 call sites in `gen_heap`, 6 in `g1`, none here --
        // and has been sliding objects out from under live compiled frames
        // since compaction went default-on on 2026-08-13.
        //
        // `pinned_jit_roots_snapshot()`, which this function DOES consume a
        // few lines below, is not a substitute. It is the set of addresses a
        // CONSERVATIVE STACK scan recovered; a pointer that never left a
        // register is not in it, so its page is not withheld and the object
        // slides anyway.
        //
        // Measured on `io.netty.util.ResourceLeakDetectorTest`: the faulting
        // pc is inside compiled `LeakAwareResource.close()Z`, the crash dump
        // reports `guarded compiled frames live process-wide: YES (quiescence
        // depth=101)`, and every logged stale access is a WRITE to field 0 of
        // a `DefaultResourceLeak` the last slide moved -- i.e. compiled code
        // performing `Reference.clear()` through a pre-move address.
        //
        // The cost is the same cost the other two collectors pay: a JIT-busy
        // process compacts less often. `relocation_skipped_jit` is exported so
        // it is visible rather than inferred, because on this collector
        // deferred compaction is also deferred defragmentation.
        //
        // MEASURED 2026-08-15, which the original of this comment left as an
        // open question. A deliberately JIT-saturated workload -- 4e6
        // mixed-size (64 B .. 8 KiB) allocations through one hot compiled
        // method, 512-entry rolling live window, 512 MiB heap -- then a count
        // of the 4 MiB contiguous blocks the heap can still hand out:
        //
        //   arm                    compactions  skipped_jit  worst_free_permille  4 MiB blocks
        //   JIT on (this branch)             4           64                  138            72
        //   CRATONVM_ZGC_RELOCATE=0          0            0                  239           124
        //
        // The decline fires on 64 of 68 cycles, so "a JIT-saturated run
        // compacts rarely" is confirmed -- and it costs NOTHING here. The arm
        // that relocates *never* ends with a LARGER worst-case largest free
        // block and satisfies nearly twice as many large contiguous requests.
        // Compaction is not what buys contiguity on this collector, so
        // deferring it is not what loses it. Four interleaved JIT reps
        // returned an identical 72.
        //
        // That is one workload, not a proof; the numbers are here so the next
        // person argues with a measurement rather than re-deriving the fear.
        // Correctness settles it regardless: a slide under a live compiled
        // frame corrupts the heap, and fragmentation only wastes it.
        // ---- THE REFUSAL IS PER-CYCLE, NOT PER-JIT-FRAME (2026-08-21) ------
        //
        // Everything above is right that a slide under a compiled frame whose
        // oops the collector cannot rewrite corrupts the heap. It is wrong that
        // "a compiled frame exists" is how to ask.
        //
        // `gen_heap::collect_garbage_inner` asked it that way and stopped on
        // 2026-07-26 (`arch-2026-07-26/moving-young-precise-roots`), in a
        // comment that describes this defect one collector over: *"Because
        // `gc_quiescence::is_active()` is true whenever ANY thread holds a live
        // JIT frame -- i.e. in every steady-state workload once the
        // 500-invocation JIT threshold trips -- that term made the young
        // generation stop being a copying collector the moment the JIT
        // engaged."* On THIS collector compaction is also the only
        // defragmentation there is, so the same term additionally means the
        // default configuration never defragments: measured as
        // `OutOfMemoryError: Java heap space (ByteBuffer.allocate 1048576)` with
        // 97 % of the heap free on `org.h2.test.store.TestKillProcessWhileWriting`.
        //
        // What replaces it is the collection's PER-CYCLE COVERAGE PROOF. Two
        // things had to be true before this could be consulted here, and a
        // first attempt on 2026-08-21 was landed and reverted because neither
        // was:
        //
        //   1. **The proof has to be RUN for this collector.**
        //      `memory::roots::collect_roots` computed it inside a `&&` chain
        //      whose second term was `heap.is_generational() || ...`, so on a
        //      ZGC cycle `refresh_moving_young_coverage_for_collection()` was
        //      never called and the verdict stayed at the `false` -- meaning
        //      *complete* -- that `begin_moving_young_coverage_cycle` had reset
        //      it to. The proof and the conservative-scan SUPPRESSION are now
        //      two expressions there; only the suppression is collector-gated.
        //   2. **The verifier has to be able to classify an address.** Its
        //      residency test read `JIT_REGION_BOUNDS`, which is
        //      generational-only, so under ZGC it answered `false` for every
        //      address and `moving_young_unpublished_frame_oop_present` failed
        //      closed on `YOUNG_BOUNDS_UNPUBLISHED`. It now asks
        //      `gen_heap::addr_is_movable`, and this collector publishes its
        //      arena envelope into `MOVABLE_BOUNDS` at construction -- a third
        //      table, because filling `JIT_REGION_BOUNDS` would re-enable an
        //      inline reference STORE fast path this collector must not have.
        //
        // Every conservative term of the generational decision is kept, and one
        // is added: the proof must have been ATTEMPTED.
        // `refresh_moving_young_coverage_for_collection` returns `true` without
        // proving anything when moving-young is off, so asking only "is
        // coverage incomplete?" would read a vacuous `false` as a proof -- the
        // exact mistake the first attempt made, now guarded from both sides
        // (`moving_young_enabled()` here, and the verifier's own fail-closed
        // gate on `movable_bounds_are_live()`).
        //
        // The proof stays conservative in a way worth knowing before reading a
        // `relocation_skipped_jit` count: it marks CROSS_THREAD_JIT_PEER
        // incomplete whenever any thread OTHER than the collection initiator is
        // in compiled code, so a many-threaded workload still compacts rarely.
        // That is a real limit of the proof, not of this gate.
        let compiled_frames_live = cratonvm_gc_core::gc_quiescence::compiled_frames_live();
        // Native claims have no JIT entry. Their per-pause frozen verdict
        // must still arm the relocation refusal for unrewritable raw holders.
        let conservative_holders_live =
            compiled_frames_live || cratonvm_gc_core::gc_quiescence::conservative_peers_live();
        // FIRST FAILING TERM, not the conjunction. The five terms below used
        // to be one `&&` chain whose only trace was `relocation_skipped_jit`,
        // and a count of a conjunction names nothing: see
        // [`Self::relocation_skip_reasons`] for the run that cost.
        let refusal: Option<usize> = if !zgc_relocate_under_proven_jit() {
            Some(relocation_skip_reason::SWITCH_OFF)
        } else if !cratonvm_gc_core::gc_quiescence::moving_young_enabled() {
            Some(relocation_skip_reason::MOVING_YOUNG_DISABLED)
        } else if cratonvm_gc_core::gc_quiescence::moving_young_coverage_incomplete()
            && !self.coverage_incompleteness_is_page_pinnable()
        {
            Some(relocation_skip_reason::COVERAGE_INCOMPLETE)
        } else if cratonvm_gc_core::gc_quiescence::force_non_moving_jit_roots() {
            Some(relocation_skip_reason::FORCED_NON_MOVING_ROOTS)
        } else if cratonvm_gc_core::gc_quiescence::unregistered_jit_frame_on_stack() {
            Some(relocation_skip_reason::UNREGISTERED_JIT_FRAME)
        } else {
            None
        };
        // THE SAME VERDICT, COUNTED WHERE IT IS COMPUTED.
        //
        // Every term above is an obligation, and four of them
        // (`SWITCH_OFF`, `MOVING_YOUNG_DISABLED`, `COVERAGE_INCOMPLETE`,
        // `FORCED_NON_MOVING_ROOTS`) are evaluated on every cycle and then
        // DISCARDED when no compiled frame is live, because the decision `if`
        // below -- which also holds the only `fetch_add` into
        // `relocation_skip_reasons` -- is guarded on `compiled_frames_live`.
        // So `relocation_skip_reasons[SWITCH_OFF] == 0` meant either "nobody
        // set the switch" or "the switch was set and every cycle ignored it",
        // and those are opposite facts.
        //
        // This census is bumped OUTSIDE that gate and answers only the first
        // question: which obligation failed. The difference between this array
        // and `relocation_skip_reasons`, term by term, IS the bypass count.
        //
        // Deliberately ahead of the `CRATONVM_ZGC_ASSUME_REWRITABLE` read: that
        // flag licenses the slide, it does not discharge the obligation. A
        // census that folded it in would be blind to what it exists to show.
        if let Some(r) = refusal {
            if let Some(slot) = self.counters.relocation_refusal_terms.get(r) {
                slot.fetch_add(1, Ordering::Relaxed);
            }
        }
        // ---- A CHUNK THIS COLLECTION CANNOT SEE ---------------------------
        //
        // UNCONDITIONAL, and deliberately not a term of the compiled-frame
        // chain below: a retained TLAB chunk has nothing to do with whether the
        // JIT is active. `retire_all_tlabs_at_safepoint` could not lock a cell,
        // so its owner still holds a reserved span -- and then nothing keeps
        // the compaction cursor above it, `compact_low_to` retracts past it and
        // zeroes it, `clear_low_free_list` drops the only record that it was
        // reserved, and the arena and that thread fill one span.
        //
        // This was previously argued away rather than checked. The argument
        // was: *"`tlab_retire_skipped` measured zero across every run of the
        // `ResourceLeakDetectorTest` repro, so no chunk is ever retained and a
        // refusal built on it could never fire."* That counter also counted the
        // THREE opportunistic callers (`alloc_raw`, `walk_objects`, the census
        // driver), none of which holds a safepoint and for none of which a skip
        // means anything -- so its zero was never evidence about this case.
        // `probes/OopMapPeerCoverage.java` reports 42 skips across both kinds,
        // which is exactly the reading a conflated counter cannot act on.
        if self.tlab_retire_incomplete.load(Ordering::Acquire) {
            self.counters
                .relocation_skipped_jit
                .fetch_add(1, Ordering::Relaxed);
            if let Some(slot) = self
                .counters
                .relocation_skip_reasons
                .get(relocation_skip_reason::TLAB_RETIRE_INCOMPLETE)
            {
                slot.fetch_add(1, Ordering::Relaxed);
            }
            let reclaimed = if retract {
                self.arena.lock().retract_cursor_into_free_tail()
            } else {
                0
            };
            return Some(reclaimed);
        }
        // ---- RAW JNI LOCALS ON A PEER (zt t1/rl, opt-in) -------------------
        //
        // `zgc-zr1rl-raw-jni-locals-on-a-native-peer`: a native whose locals
        // are raw addresses (`CRATONVM_JNI_INDIRECT_LOCALS=0`, or a raw escape)
        // may keep a C copy of one across the JNIEnv call or up-call it is
        // parked in. The local TABLE is remapped; the C copy is not. Like the
        // retained-chunk refusal above, unconditional: it has nothing to do
        // with compiled frames, and `ASSUME_REWRITABLE` does not license it
        // (C state is not a compiled frame). Read with the ledger-unbound
        // answer `false`, so a thread reaching no ledger does not refuse every
        // slide (z2-r2 hunk X2 (b)). Counted in both censuses: it always stops
        // the slide.
        if cratonvm_gc_core::gc_quiescence::zgc_relocate_refuses_raw_jni_locals()
            && cratonvm_gc_core::gc_quiescence::raw_jni_locals_open_or_closed()
        {
            let r = relocation_skip_reason::RAW_JNI_LOCALS;
            if let Some(slot) = self.counters.relocation_refusal_terms.get(r) {
                slot.fetch_add(1, Ordering::Relaxed);
            }
            if let Some(slot) = self.counters.relocation_skip_reasons.get(r) {
                slot.fetch_add(1, Ordering::Relaxed);
            }
            self.counters
                .relocation_skipped_jit
                .fetch_add(1, Ordering::Relaxed);
            let reclaimed = if retract {
                self.arena.lock().retract_cursor_into_free_tail()
            } else {
                0
            };
            return Some(reclaimed);
        }

        // `CRATONVM_ZGC_ASSUME_REWRITABLE=1` -- **A MEASUREMENT INSTRUMENT, and
        // unsafe to run with.** It answers the one question this family turns
        // on and which no per-obligation repair can answer: if every compiled
        // frame WERE rewritable, would compaction fix the workload at all?
        //
        // Discharging obligations one at a time cannot answer it, and two
        // attempts on record show why. `CRATONVM_XT_JIT_COVERAGE_ASSUME` took
        // the cross-thread handshake from `accepted=0 refused=1731` to
        // `accepted=1731 refused=0` and `TestCachedQueryResults` failed
        // identically, because other obligations still marked the cycle
        // incomplete. `CRATONVM_XT_HELPER_WINDOW_DISCHARGE` then removed
        // `xt-helper-window-conservative-scan` outright (12 -> absent on
        // `TestMultiThread`) and `relocation_on_proven_jit` went 1 -> 0,
        // because the refusals redistributed to
        // `compiled-frame-oop-not-published` and `cross-thread-jit-peer`.
        //
        // `coverage-proof-incomplete` is a CONJUNCTION. Only forcing the whole
        // term says whether the conjunction is worth satisfying, and that is
        // the ONLY thing this flag is for. It relocates under frames nobody
        // proved rewritable; expect corruption if the answer is no.
        let assume_rewritable = zgc_assume_rewritable();
        let frames_are_rewritable = refusal.is_none() || assume_rewritable;
        // (zt t12/rt: the opt-in blanket guard, `CRATONVM_ZGC_JIT_BLANKET_REFUSAL`,
        // which refused whenever a compiled frame was live, was retired; code 6
        // of `relocation_skip_reason` stays unassigned.)
        if conservative_holders_live && !frames_are_rewritable {
            self.counters
                .relocation_skipped_jit
                .fetch_add(1, Ordering::Relaxed);
            if let Some(r) = refusal {
                if let Some(slot) = self.counters.relocation_skip_reasons.get(r) {
                    slot.fetch_add(1, Ordering::Relaxed);
                }
                if r == relocation_skip_reason::COVERAGE_INCOMPLETE {
                    // WHICH obligation the proof failed on. `moving_young_incomplete_reason`
                    // is first-wins for the cycle, which is the one that forced it.
                    let why = cratonvm_gc_core::gc_quiescence::moving_young_incomplete_reason();
                    if let Some(slot) = self.counters.relocation_coverage_reasons.get(why) {
                        slot.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            let reclaimed = if retract {
                self.arena.lock().retract_cursor_into_free_tail()
            } else {
                0
            };
            return Some(reclaimed);
        }
        if compiled_frames_live {
            self.counters
                .relocation_on_proven_jit
                .fetch_add(1, Ordering::Relaxed);
        }
        // WHICH of the two licences this cycle used. `relocation_on_proven_jit`
        // counts "relocated with a compiled frame live" and cannot separate a
        // cycle whose proof PASSED from one whose proof failed on a reason page
        // pinning covers -- and those are different claims about the collector.
        // AND THE PIN SET HAS TO HAVE LICENSED IT. The incompleteness alone is
        // not the claim this counter makes: on a `!compiled_frames_live` cycle
        // the whole chain above is bypassed, so a cycle can reach here with
        // `coverage_incompleteness_is_page_pinnable()` answering `false` --
        // the pins licensed nothing and the cycle slid because there was no
        // compiled frame, not because pinning covered the gap. Counting those
        // made `relocation_on_page_pins` a count of "coverage was incomplete",
        // which `moving_young_coverage_incomplete()` already says.
        if cratonvm_gc_core::gc_quiescence::moving_young_coverage_incomplete()
            && self.coverage_incompleteness_is_page_pinnable()
        {
            self.counters
                .relocation_on_page_pins
                .fetch_add(1, Ordering::Relaxed);
        }
        None
    }

    /// The slide itself, for a cycle [`Self::relocate_refusal`] admitted.
    /// Call it only straight after that answered `None`, inside the same
    /// stop-the-world.
    pub fn relocate_stw_admitted(&self, live: &[usize]) -> (usize, usize, cratonvm_types::PointerMap) {
        self.relocate_stw_admitted_hinted(live, &ZSweepSizeHints::default())
    }

    /// [`Self::relocate_stw_admitted`] with the sweep's survivor size cells
    /// (zr z7/pc, `CRATONVM_ZGC_SWEEP_SIZE_HINTS`): `logical_pages` takes a
    /// survivor's cell from `hints` when the hint names that exact base, and
    /// reads its header otherwise. Empty hints are the plain function.
    pub fn relocate_stw_admitted_hinted(
        &self,
        live: &[usize],
        hints: &ZSweepSizeHints,
    ) -> (usize, usize, cratonvm_types::PointerMap) {
        self.relocate_stw_admitted_scoped(live, hints, None)
    }

    /// t1 wave 6 lane Y4 (zt3gn S2.4 slice 1, `CRATONVM_ZGC_GEN_YOUNG_EVACUATE`):
    /// build THIS young cycle's nursery scope, after the sweep, at the slide
    /// point, under the stop-the-world token. See `young_evacuate` for the
    /// soundness argument; every refusal returns a `YE_REFUSE_*` code and
    /// nothing has been written.
    ///
    /// `aged_bitmap_cycle` is the caller's `old_bits_young && young_aged_sweep`:
    /// only then is the old-start bitmap exact and the registry free of dead
    /// young objects below the floor, which is what lets "young survivor" be
    /// read as `registered & !old` by words.
    ///
    /// Cost: one backwards bit scan (the straddler), a word walk of `[lo,
    /// cursor)` for old starts, the live list of `[lo, cursor)`, a word walk of
    /// `[base, lo)` and of the large-object end for young starts, and one
    /// pass over the cards. O(heap/512) words plus O(young live + cards); no
    /// old object's header is read.
    pub fn young_evac_scope(
        &self,
        aged_bitmap_cycle: bool,
    ) -> Result<young_evacuate::ZYoungEvacScope, usize> {
        use young_evacuate as ye;
        if !aged_bitmap_cycle {
            return Err(ye::YE_REFUSE_OLD_BITS);
        }
        let Some(old) = self.counters.gen_old_bits.get() else {
            return Err(ye::YE_REFUSE_OLD_BITS);
        };
        let Some(reg) = self.registry.bitmap() else {
            return Err(ye::YE_REFUSE_REGISTRY);
        };
        if self.registry.has_spill()
            || old.has_spill()
            || reg.word_base(0) != old.word_base(0)
            || reg.word_count() != old.word_count()
        {
            return Err(ye::YE_REFUSE_REGISTRY);
        }
        if self.counters.gen_element_cards_on.load(Ordering::Relaxed) {
            return Err(ye::YE_REFUSE_ELEMENT_CARDS);
        }
        if zgc_slide_evacuate_enabled() || region_forwarding::zgc_region_relocate_enabled() {
            return Err(ye::YE_REFUSE_EVACUATING_SLIDE);
        }
        if self
            .counters
            .young_evac
            .remset_incomplete
            .load(Ordering::Relaxed)
        {
            return Err(ye::YE_REFUSE_REMSET_INCOMPLETE);
        }
        let page = Self::Z_LOGICAL_PAGE_BYTES;
        let (base, low_end, high_start, arena_end) = {
            let arena = self.arena.lock();
            let base = arena.base_ptr() as usize;
            (
                base,
                base + arena.used_low_for_compaction(),
                base + arena.high_cursor(),
                base + arena.capacity(),
            )
        };
        let floor = self.gen_young_floor.load(Ordering::Relaxed);
        let Some(lo) = ye::scope_low_bound(floor, base, low_end, page) else {
            return Err(ye::YE_REFUSE_FLOOR);
        };
        let clock = zgc_dbg_relocate_phases_enabled().then(std::time::Instant::now);
        let lap = |t: Option<std::time::Instant>| t.map_or(0, |t| t.elapsed().as_micros());
        let mut blocked: Vec<u64> = Vec::new();
        // The object straddling `lo` from below is in no live list the slide
        // sees, so the pages its tail covers are withheld.
        if lo > base {
            match self.registry.nearest_base_within(lo - 1, base) {
                None => return Err(ye::YE_REFUSE_REGISTRY),
                Some(None) => {}
                Some(Some(b)) => match self.live_extent(b, self.header_ref(b as *mut u8)) {
                    Some(size) => {
                        ye::push_straddler_pages(b, b.saturating_add(size), lo, base, page, &mut blocked)
                    }
                    None => return Err(ye::YE_REFUSE_STRADDLER),
                },
            }
        }
        let word_base = |w: usize| reg.word_base(w);
        let start_word = |w: usize| reg.word_at(w);
        let old_word = |w: usize| old.word_at(w);
        // Pages holding an old object (promoted in place, or tenured by this
        // very sweep) are never selected: an old object's referrers are not
        // carded, so it must not move on a scoped slide.
        let (w0, w1) = reg.word_range(lo, low_end);
        ye::old_start_pages(
            w0, w1, &word_base, &start_word, &old_word, lo, low_end, base, page, &mut blocked,
        );
        ye::finish_blocked(&mut blocked);
        let t_blocked = lap(clock);
        let live = self.registry.bases_in_span(lo, low_end);
        let t_live = lap(clock);
        // Young survivors outside the scope: below `lo`, and at the large-object
        // end (never moved here, but they may name a moved object).
        let mut young: Vec<usize> = Vec::new();
        // Only blocks that may hold a young base are read; a block found
        // young-free is forgotten under the same conditions as the registry's
        // empty blocks (the sweep above was the last writer of this pause).
        let forget = self.registry_forget_allowed();
        reg.young_bases_in(old, base, lo, forget, &mut young);
        let hs = high_start.max(low_end);
        if hs < arena_end {
            reg.young_bases_in(old, hs, arena_end, forget, &mut young);
        }
        // Every carded object (the CURRENT buffer: this cycle's retained cards
        // plus the cards its sweep wrote at promotion). A card that names no
        // registered base names no object and is skipped.
        let t_young = lap(clock);
        let mut carded: Vec<usize> = Vec::new();
        for set in self.remembered.snapshot() {
            set.iterate(|off| {
                if let Some(a) = set.slot_address(off) {
                    let a = a as usize;
                    if self.registry.contains(a) {
                        carded.push(a);
                    }
                }
            });
        }
        let t_carded = lap(clock);
        let (n_young, n_carded) = (young.len(), carded.len());
        let extra = ye::merge_extra(young, carded, lo, low_end);
        if clock.is_some() {
            eprintln!(
                "[zgc-young-evac-scope-parts] blocked_us={t_blocked} live_us={} young_us={} \
                 carded_us={} merge_us={} young={n_young} carded={n_carded} lo_off={} \
                 low_end_off={} high_off={} arena_off={}",
                t_live - t_blocked,
                t_young - t_live,
                t_carded - t_young,
                lap(clock) - t_carded,
                lo - base,
                low_end - base,
                high_start - base,
                arena_end - base,
            );
        }
        Ok(ye::ZYoungEvacScope {
            lo,
            lo_page: ye::page_of(lo, base, page),
            low_end,
            blocked_pages: blocked,
            live,
            extra,
        })
    }

    /// The slide, optionally NURSERY-SCOPED (t1 wave 6 lane Y4). With
    /// `scope` set, `live` must be `scope.live`, and the slide (a) selects only
    /// pages `scope.admits_page`, (b) runs no evacuating arm and does not
    /// compact the large-object end (whose survivors are not in `live`), (c)
    /// rewrites `scope.extra` after its own rewrite of `live`, and (d) has the
    /// debug verifier (`CRATONVM_DBG_ZGC_VERIFY_SLIDE`) walk the WHOLE post-move
    /// registry, so a missing card shows as `slide_verify_missed_rewrites`.
    pub fn relocate_stw_admitted_scoped(
        &self,
        live: &[usize],
        hints: &ZSweepSizeHints,
        scope: Option<&young_evacuate::ZYoungEvacScope>,
    ) -> (usize, usize, cratonvm_types::PointerMap) {
        // A RETAINED TLAB CHUNK IS NOW A REFUSAL ABOVE, not a ruling-out. The
        // paragraph that used to stand here said it "could never fire" on the
        // strength of a counter that conflated the safepoint caller with three
        // opportunistic ones. Kept below as history, because the shape of the
        // hazard it describes is right and only the evidence was wrong.
        //
        // A RETAINED TLAB CHUNK WAS THE OBVIOUS SUSPECT HERE, AND IT WAS RULED
        // OUT. `retire_all_tlabs` skips a cell it cannot `try_lock`, and on a
        // compacting heap that would be unsound rather than merely wasteful:
        // nothing keeps the compaction cursor above a chunk the collector
        // cannot see, so `compact_low_to` would retract past it, zero it, and
        // `clear_low_free_list` would drop the only record it was reserved --
        // after which the arena and the chunk's owner would fill one span.
        //
        // `tlab_retire_skipped` measured **zero** across every run of the
        // `ResourceLeakDetectorTest` repro, so no chunk is ever retained and a
        // refusal built on it could never fire. The counter is exported rather
        // than the refusal written: it is what turned the story into a
        // ruled-out one, and a nonzero there is how someone re-opens it.
        // The slide's from->to ledger: a plain pair list, ascending by
        // `from` at the low end. (zs s1/sl: this paragraph used to say the
        // ledger was `relocate::ZRelocationRecord`; it is not, and that type
        // has no production caller. The `PointerMap`, the forward index and
        // `counters.relocations` are all built from these pairs.) Only real
        // moves are recorded, never an identity entry.
        let mut pairs: Vec<(usize, usize)> = Vec::new();
        let mut moved = 0usize;
        let mut reclaimed = 0usize;
        // Captured out of the arena scope for the post-slide verifier, which
        // runs after the guard is dropped.
        let mut arena_lo = 0usize;
        let mut arena_hi = 0usize;
        // The span `compact_low_to` is about to zero, captured for
        // `stamp_forwarding_words`. Empty when no low slide ran.
        let mut forwardable_tail = 0usize..0usize;
        // zr z2/r2: where the slide's time goes, one line per slide under
        // `CRATONVM_DBG_ZGC_RELOCATE_PHASES=1` (no clock is read otherwise).
        let mut phases = ZSlidePhaseClock::new(zgc_dbg_relocate_phases_enabled());
        // zr z2/r2: a panic after the first memmove is a fail-stop, not an
        // unwind into a half-moved heap. See `ZSlideFailStop`.
        let mut fail_stop = ZSlideFailStop::default();
        // zr z2/r2 (`CRATONVM_ZGC_FUSED_SLIDE_SCREEN`): the `live_ceiling`
        // screen rides on the reference rewrite, so the low region's headers
        // are read once after the slide instead of twice. Filled only on the
        // fused arm: the rewrite's answer, the forward index it used, and the
        // large-object end's result (that end is compacted BEFORE the low
        // cursor drops on this arm, so the rewrite sees every pair).
        let fused = zgc_fused_slide_screen_enabled();
        // zr z4/un: `CRATONVM_ZGC_SLIDE_BUFFER_TRIM` (default ON, `=0` kill).
        let slide_trim = self.counters.fp.slide_trim_on.load(Ordering::Relaxed);
        let mut fused_tally: Option<ZRewriteTally> = None;
        let mut fused_fwd: Option<relocate::ZForwardIndex> = None;
        // zt t23/zc7: the region cycle the evacuating arm drained
        // (`CRATONVM_ZGC_REGION_RELOCATE=1`), which the forward index asks
        // first and the registry rebuild reads; its installs are the first
        // `slide_cycle_pairs` entries of `pairs` (the low end's, before the
        // large-object end appends). `region_quarantine` (R3,
        // `CRATONVM_ZGC_REGION_QUARANTINE`) keeps it past this pause.
        let mut slide_cycle: Option<std::sync::Arc<region_forwarding::ZRelocationCycle>> = None;
        let mut slide_cycle_pairs = 0usize;
        let mut region_quarantine: Option<region_forwarding::ZRegionQuarantine> = None;
        let mut high_done: Option<(usize, usize)> = None;
        // t1 wave 6 handback (Y4): a scoped slide that selected no page
        // (`YE_DECLINED_NOTHING_SELECTED`) -- counted at the early return.
        let mut scoped_nothing_selected = false;
        // zr z7/pc (`CRATONVM_ZGC_RELOCATE_SIZE_TABLE`): one `size_cell` per
        // entry of `live`, filled by `logical_pages`; empty on the kill arm,
        // where every pass reads the header as before.
        let size_table_on = zgc_relocate_size_table_enabled();
        let mut sizes: Vec<u32> = Vec::new();
        // zr z9/sl: the copy volume, both ends (`moved_bytes=`). The high end
        // books its own into `high_bytes_copied`; the delta is this slide's.
        let high_copied_before = self.counters.high_bytes_copied.load(Ordering::Relaxed);
        let mut low_moved_bytes = 0usize;
        // zr z9/sl (`CRATONVM_ZGC_SELECTIVE_RELOCATE`): per logical page, the
        // free bytes that lie in holes too small for the allocator's
        // contiguous requests. Filled only on the selective arm; the cost gate
        // reads it there instead of the pages' whole garbage.
        let mut sel_frag: Option<Vec<usize>> = None;

        {
            let mut arena = self.arena.lock();
            let base = arena.base_ptr() as usize;
            let low_end = base + arena.used_low_for_compaction();
            arena_lo = base;
            arena_hi = base + arena.capacity();

            // ---- PRE-SLIDE CENSUS (diagnostic, `CRATONVM_DBG_ZGC_CORPSE`) --
            //
            // `rewrite_target_walkable_sized` reports offenders AFTER the slide,
            // which cannot separate "this slide broke them" from "they arrived
            // broken". The same question asked here, before a single byte
            // moves, does separate them -- and it is the first question to ask,
            // because the two answers point at opposite halves of the
            // collector.
            //
            // Behind the corpse gate rather than unconditional: it is a full
            // pass over the live set with an `alloc_size` each, which is the
            // same order as the slide itself but buys nothing in a healthy run.
            if zgc_corpse_enabled() {
                let mut bad = 0usize;
                let mut first: Option<(usize, u32, u32)> = None;
                for &b in live {
                    if b < base || b >= low_end {
                        continue;
                    }
                    let h = self.header_ref(b as *mut u8);
                    if Self::alloc_size(h).is_none_or(|sz| b.saturating_add(sz) > low_end) {
                        bad += 1;
                        if first.is_none() {
                            first = Some((b, h.class_id.as_u32(), h.num_slots()));
                        }
                    }
                }
                if bad > 0 {
                    let (addr, class_id, num_slots) = first.unwrap_or((0, 0, 0));
                    tracing::error!(
                        target: "cratonvm::gc::guard",
                        unwalkable_before_slide = bad,
                        live = live.len(),
                        first_addr = addr,
                        first_class_id = class_id,
                        first_num_slots = num_slots,
                        "zgc relocate: the live set was ALREADY unwalkable on entry -- \
                         whatever corrupted these bases, it was not this slide"
                    );
                }
            }

            // Survivors in ADDRESS order. The slide requires it: an object may
            // only be copied into space a lower-addressed survivor has already
            // vacated. Out-of-order copying overwrites a survivor that has not
            // moved yet -- silent heap corruption, not a failed assert.
            // ---- Which pages are worth evacuating -------------------------
            //
            // `forwarding::ZRelocationSet::select` is the real ZGC selector
            // and this is its first production caller. It ranks the logical
            // pages by profitability -- copy cost is live bytes, benefit is
            // `extent - live` -- refuses pages above `max_live_occupancy`
            // (0.25 by default: copy one byte to reclaim at least three), and
            // stops at `max_evacuation_bytes`.
            //
            // Without it this function slid the WHOLE low region every cycle,
            // which copies a dense, wholly-live page for no reclaim at all.
            // With it, a page that is 90% live is left to decay.
            // ---- The generational scope -----------------------------------
            //
            // `generation::ZPromotionPolicy` ages every logical page one cycle
            // and splits young from old; `ZGenerationScope::from_pages` then
            // names the byte ranges a young cycle may touch. Both are driven
            // from real `page::ZPageReal` VIEWS over the arena grid -- see
            // `logical_pages` for why a view is not a page.
            //
            // NO `ZGenerationScope` IS BUILT HERE, and no `old_page_ids` is
            // published.
            //
            // Both were removed on 2026-09-03 because nothing read either one.
            // The scope was described as "advisory for now" -- this cycle
            // evacuates from whichever pages the relocation-set selector picks,
            // young or old -- and in the year since, its only consumer was the
            // `scope_pages` field of the `tracing::debug!` below it. A
            // `ZGenerationScope` that no marker is ever handed does not scope
            // anything; it filters a page list into a second page list and
            // drops it, and a reader who finds it here reasonably concludes the
            // STW slide is generation-aware, which it is not. The real minor
            // cycle builds its own scope over its own young page set --
            // `generation::ZGenerationalHeap::minor_cycle` -- and that one is
            // handed to a marker. `counters.old_page_ids` had no reader at all;
            // even the card barrier's gate stopped asking it (see
            // `has_old_objects`).
            //
            // What survives is the part with an effect: the page ages
            // themselves, and the remembered set every old page gets.
            // Workers for this cycle's whole-live-set passes (round 9 wave 7,
            // `zgc7`); 1 = serial. See `ZGC_PAR_RELOCATE_DEFAULT_WORKERS`.
            let workers = self.relocate_workers_for(live.len());
            // zr z9/sl + zs s1/sl: read once, ahead of the page census, so the
            // census itself can book the selective arm's fragmented garbage
            // (`CRATONVM_ZGC_SELECTIVE_FUSED_FRAG`). `None` on the default
            // arm: the census is then byte-for-byte the pre-zs pass.
            // t1 wave 6 handback (Y4): NOT on a scoped slide. The selective
            // cycle gate (default ON as `=2` since zs s2) declines a slide
            // whose garbage already sits in holes the allocator can use --
            // and a nursery's garbage is exactly that: one long run per dead
            // burst. It declined every scoped slide of the unit test (scope
            // built, 3 live, 0 moved, no refusal counted). A scoped slide
            // exists to EMPTY the nursery, not to buy contiguity, so the
            // admitted pages are its relocation set (S2.4: as in a HotSpot
            // young collection).
            let selective = if scope.is_some() {
                None
            } else {
                zgc_selective_relocate_config()
            };
            let fused_hole_min = selective
                .filter(|_| zgc_selective_fused_frag_enabled())
                .map(|cfg| cfg.hole_min);
            let (views, mut fused_frag) = self.logical_pages(
                live,
                base,
                low_end,
                workers,
                size_table_on.then_some(&mut sizes),
                hints,
                fused_hole_min,
            );
            phases.mark("logical");
            let promo = generation::ZPromotionPolicy::default();
            let (young_ids, old_ids) = self.age_pages_and_split(views.len(), &promo);
            if scope.is_some() {
                phases.mark("ye_age");
            }
            // Every old page gets a remembered set, so the store barrier has
            // somewhere to record an old-to-young edge before the next cycle.
            // Context-local ids: `old_ids` indexes the logical grid this heap
            // derives by address arithmetic over its single arena, not
            // `ZPageReal::id()`. See `card_object` for why the declaration
            // matters even though nothing else registers on this table today.
            // ...WITH THEIR BASE ADDRESSES. *Applies
            // `handoff-n-census-engagement-and-namespaced-base.md`.* All three
            // of this heap's registrations used to be base-less, so
            // `ZRememberedSetTable::iterate_slot_addresses` would report every
            // old page in its `pages_without_a_base` return value and yield no
            // roots at all -- the base exists precisely to remove the
            // consumer-side `page_id -> base` map whose missing entry is a
            // silently dropped root.
            //
            // The `_in` form is mandatory: `register_old_page_with_base` (no
            // suffix) hard-codes `ZPageIdNamespace::Allocator`, and offering
            // that to a latch already holding `ContextLocal` is the conflict
            // arm -- `error!` plus `debug_assert!(false)`, a panic in every
            // debug and test build on the first promoted page.
            //
            // NOT on a nursery-scoped slide. A young cycle drops every set that
            // holds no card (`young_extra_roots`) and the card barrier
            // registers its page on the first store (`card_object`), so a
            // scoped slide re-registering every aged page allocated one bitmap
            // per page for the next cycle to drop: 24 of 28 ms a slide on
            // `Zr1GnGenShape` (`t1y4_a_scoped_slide_registers_no_uncarded_page`).
            if scope.is_none() {
                for id in &old_ids {
                    self.remembered.register_old_page_with_base_in(
                        remembered::ZPageIdNamespace::ContextLocal,
                        *id,
                        (base + *id as usize * Self::Z_LOGICAL_PAGE_BYTES) as u64,
                        Self::Z_LOGICAL_PAGE_BYTES,
                    );
                }
            }
            if scope.is_some() {
                phases.mark("ye_rset");
            }
            tracing::debug!(
                target: "zgc",
                young = young_ids.len(),
                old = old_ids.len(),
                "zgc generational split over the logical grid"
            );

            // `adapters::page_candidates` rather than a local map. That module
            // exists so there is EXACTLY ONE conversion between a
            // `page::ZPageReal` and a `forwarding::PageCandidate`; hand-rolling
            // a second one here is the thing its header calls a symptom, and it
            // is how `capacity_bytes` comes to mean the page SPAN in one place
            // and the bump EXTENT in another -- an ambiguity that inverts the
            // selector's profitability ranking.
            let candidates = adapters::page_candidates(&views);
            // t1 wave 6 lane Y4: a nursery-scoped slide offers the selector
            // only the pages at or above the scope's floor page that hold no
            // old object -- every other page keeps its objects in place, and
            // the pages below the floor (which read EMPTY here, their
            // survivors not being in `live`) are never destinations.
            let candidates = match scope {
                Some(s) => {
                    debug_assert_eq!(s.low_end, low_end, "the scope was cut at this cursor");
                    debug_assert!(s.lo >= base && s.lo < low_end, "the scope's floor page");
                    let mut kept = candidates;
                    kept.retain(|c| s.admits_page(c.page_id));
                    kept
                }
                None => candidates,
            };
            let mut policy = forwarding::ZRelocationPolicy::default();
            if scope.is_some() {
                phases.mark("ye_cand");
            }
            // (zt t12/rt: the opt-in targeted compaction that set
            // `policy.target_pages` from the last allocation failure's window,
            // `CRATONVM_ZGC_TARGETED_COMPACTION`, engaged zero times on every
            // workload and was retired; `target_pages` stays `None` here.)
            // THE EVACUATION BUDGET, and the one place it is assignable.
            //
            // Unset, this is byte-for-byte `ZFWD_DEFAULT_MAX_EVACUATION_BYTES`
            // -- the value `ZRelocationPolicy::default()` already put there --
            // so no existing run moves. The knob exists so the A/B the budget
            // needs is a SAME-BINARY pair of arms rather than two builds, which
            // is what `CRATONVM_ZGC_PAR_RELOCATE`'s flip was measured with.
            policy.max_evacuation_bytes = zgc_relocate_budget_bytes();
            // zr z2/cp: WIDEN THE OCCUPANCY CUTOFF UNDER PRESSURE. The default
            // cutoff (0.25 live) evacuates only pages that are three-quarters
            // garbage, so a heap whose pages are 30-90 % live is never
            // compacted, and a request that needs CONTIGUITY (an `ArrayList`
            // growing its `Object[]`) fails on a heap that is 40-60 % free in
            // small holes: `ProxyBoxingOomeProbe` OOMEd at 39 % of HotSpot's
            // live set, `GchMj3OccupancyMajorWeakProbe --compatible` at 90 %
            // occupancy. See `relocate_pressure_occupancy`.
            // Consumed: one refused request widens one slide.
            let cycle_followed_failure = self
                .counters
                .cycle_after_alloc_failure
                .swap(false, Ordering::Relaxed);
            // Low-end contiguous headroom: un-bumped middle headroom plus the
            // largest single low free-list block (disjoint from the middle and
            // excluding tiny mosaic holes). A mosaic has small headroom AND
            // tiny holes, so it still reads tight; a roomy heap with a large
            // coalesced free-list span alongside bump headroom does not
            // false-trigger `Tight`.
            let bump_free_permille = arena
                .low_bump_headroom()
                .saturating_add(arena.largest_low_free_block())
                .min(arena.remaining())
                .saturating_mul(1000)
                / arena.capacity().max(1);
            let pressure_tier = relocate_pressure_tier(
                zgc_relocate_pressure_select_mode(),
                cycle_followed_failure,
                bump_free_permille,
            );
            if let Some(tier) = pressure_tier {
                policy.max_live_occupancy = relocate_pressure_occupancy(tier);
            }
            // ---- zr z9/sl: KEEP A PAGE WHOSE HOLES ARE ALREADY USABLE ------
            //
            // `CRATONVM_ZGC_SELECTIVE_RELOCATE` (default OFF, opt-in). The
            // occupancy cutoff above is HotSpot's question ("is this page
            // sparse?"), and on HotSpot it is the only way a sparse page's
            // garbage ever comes back: ZGC has no free list. This collector
            // HAS one -- the sweep already put every dead byte on it -- so
            // evacuating a sparse page buys only CONTIGUITY, and pays for it
            // with the reference rewrite over EVERY live object (not over the
            // moved set: nothing else can name the slots that point into a
            // moved page). A page whose free bytes are already in holes of at
            // least `hole_min` gains nothing from the move that a TLAB chunk or
            // a large request could not already have. So on this arm a page
            // with survivors is a candidate only when at least `frag_pct` % of
            // its extent is FRAGMENTED garbage (holes under `hole_min`), and
            // the cost gate prices the slide by that fragmented garbage.
            //
            // Never after an allocation failure: that tier is the
            // full-compaction fallback (a dense heap with scattered holes must
            // still satisfy a large request), and it runs the unfiltered
            // selector exactly as before. Targeted pages and wholly empty pages
            // (destinations, nothing to copy) are never dropped.
            // (`selective` is read above, before the page census.)
            // zr z11/rv9: the LATCH, not the pressure tier. With
            // `CRATONVM_ZGC_RELOCATE_PRESSURE_SELECT=0` the tier is `None` even
            // after a refused request, and reading the tier here kept the
            // filter on for the failure collection: the long-hole pages stayed,
            // and the request that needed their contiguity failed again (an
            // OOME the default selector does not throw). The fallback is this
            // arm's own promise, so it must not depend on another switch.
            // `CRATONVM_ZGC_SELECTIVE_BYPASS_ON_LATCH=0` restores the z9 read.
            let after_failure = if zgc_selective_bypass_on_latch_enabled() {
                cycle_followed_failure
            } else {
                pressure_tier == Some(RelocatePressure::AllocFailure)
            };
            if selective.is_some() && after_failure {
                self.counters.selective_relocate[3].fetch_add(1, Ordering::Relaxed);
            }
            // zs s1/sl: the cycle gate's "no slide" answer, applied after the
            // pin screen (the one writer of `selected` before it).
            let mut sel_cycle_declined = false;
            // zs s3/cr (`CRATONVM_ZGC_SELECTIVE_GATE_ON_SELECTION`, default ON
            // since zt t20/fl1, `=0` kill): the `=2` gate's inputs, carried past the selector and the pin
            // screen so the gate prices the pages the slide will actually
            // move. `(per-page fragmented garbage, low live, the old input)`.
            let gate_on_selection = true;
            let mut gate_deferred: Option<(Vec<usize>, usize, usize)> = None;
            let filtered: Option<Vec<forwarding::PageCandidate>> = match selective {
                Some(cfg) if !after_failure => {
                    let frag = match fused_frag.take() {
                        Some(f) => {
                            self.counters.selective_relocate[7].fetch_add(1, Ordering::Relaxed);
                            f
                        }
                        None => self.fragmented_garbage_per_page(
                            live,
                            &sizes,
                            base,
                            low_end,
                            cfg.hole_min,
                            workers,
                        ),
                    };
                    if cfg.mode == ZSelectiveMode::CycleGate {
                        // One decision for the cycle; never a page filter.
                        // A targeted window is an allocation failure's own
                        // request: the default selector serves it.
                        let low_live: usize = candidates.iter().map(|c| c.live_bytes).sum();
                        let frag_in_reach: usize = candidates
                            .iter()
                            .filter(|c| {
                                c.live_bytes > 0
                                    && c.garbage_bytes() > 0
                                    && c.live_occupancy() < policy.max_live_occupancy
                            })
                            .map(|c| frag.get(c.page_id as usize).copied().unwrap_or(0))
                            .sum();
                        // zs s2/sl2: nor a cycle that followed a refused
                        // request. `after_failure` already keeps such a cycle
                        // out of this arm, but under
                        // `CRATONVM_ZGC_SELECTIVE_BYPASS_ON_LATCH=0` with
                        // `CRATONVM_ZGC_RELOCATE_PRESSURE_SELECT=0` it reads
                        // the tier, which is `None`, and the gate could then
                        // decline the very collection the failure asked for
                        // (an OOME HotSpot does not throw). The latch is read
                        // here directly: the gate's promise must not depend
                        // on two other switches.
                        let forced_admit = policy.target_pages.is_some() || cycle_followed_failure;
                        if gate_on_selection && !forced_admit {
                            // zs s3/cr: decided below, after the pin screen,
                            // on `selected` (see `gate_deferred`).
                            gate_deferred = Some((frag, low_live, frag_in_reach));
                            None
                        } else {
                            let verdict = if forced_admit {
                                SelectiveCycle::Admit
                            } else {
                                selective_cycle_gate(frag_in_reach, low_live, bump_free_permille)
                            };
                            self.counters.selective_relocate[selective_cycle_slot(verdict)]
                                .fetch_add(1, Ordering::Relaxed);
                            sel_cycle_declined = verdict == SelectiveCycle::Decline;
                            None
                        }
                    } else {
                        let targeted = |id: u64| {
                            policy
                                .target_pages
                                .is_some_and(|(lo, hi)| id >= lo && id <= hi)
                        };
                        let mut kept_in_place = 0usize;
                        let out: Vec<forwarding::PageCandidate> = candidates
                            .iter()
                            .filter(|c| {
                                let f = frag.get(c.page_id as usize).copied().unwrap_or(0);
                                let keep = selective_page_admits(
                                    c.live_bytes,
                                    c.capacity_bytes,
                                    f,
                                    cfg.frag_pct,
                                    targeted(c.page_id),
                                );
                                if !keep
                                    && c.garbage_bytes() > 0
                                    && c.live_occupancy() < policy.max_live_occupancy
                                {
                                    kept_in_place += 1;
                                }
                                keep
                            })
                            .copied()
                            .collect();
                        self.counters.selective_relocate[0].fetch_add(1, Ordering::Relaxed);
                        self.counters.selective_relocate[1]
                            .fetch_add(kept_in_place, Ordering::Relaxed);
                        sel_frag = Some(frag);
                        Some(out)
                    }
                }
                _ => None,
            };
            let reloc_set = forwarding::ZRelocationSet::select(
                filtered.as_deref().unwrap_or(&candidates),
                &policy,
            );
            if scope.is_some() {
                phases.mark("ye_select");
            }
            // zs s2/sl2 (`CRATONVM_ZGC_RELOCATION_SET_BOOKS_FINAL`, default
            // ON, `=0` kill): the relocation-set telemetry (JFR
            // `jdk.ZRelocationSet`, `targeted_pages_selected`, the pressure
            // tier's selected live bytes) books the set the slide RELOCATES,
            // after the pin screen, the selective cycle gate and the cost gate
            // dropped pages -- below, beside `phases.mark("select")`. It used
            // to book the selector's answer here, so a cycle the cost gate
            // declined still reported a relocation set (HotSpot books what it
            // relocates). The `=0` arm books here, as before.
            let books_final = zgc_relocation_set_books_final_enabled();
            if !books_final {
                // zr z4/jf: `jdk.ZRelocationSet` / `ZRelocationSetGroup` facts.
                self.counters.jfr.note_relocation_set(
                    jfr_facts::ZJfrRelocationSet::from_selection(&candidates, &reloc_set),
                );
                // zr z3/lt: per heap (was a process static inside `select`).
                self.counters
                    .local
                    .targeted_pages_selected
                    .fetch_add(reloc_set.targeted_selected(), Ordering::Relaxed);
            }
            if let Some(tier) = pressure_tier {
                // The tier's cycle count stays here: the selector DID run
                // widened on this cycle, whatever was dropped after it.
                self.counters.pressure_select[tier as usize].fetch_add(1, Ordering::Relaxed);
                if !books_final {
                    self.counters.pressure_select[2]
                        .fetch_add(reloc_set.total_live_bytes(), Ordering::Relaxed);
                }
            }
            // DID THE BUDGET BIND? `select` is a prefix rule, so a cycle that
            // stopped early and a cycle that evacuated everything are otherwise
            // indistinguishable from outside -- same `compaction_cycles`, same
            // shape of `objects_relocated`, no skip reason, because the refusal
            // census is about whether the slide RAN. `deferred_for_budget()`
            // had no caller outside `forwarding.rs`'s own tests until here.
            {
                let deferred = reloc_set.deferred_for_budget();
                if deferred != 0 {
                    self.counters
                        .relocation_pages_deferred
                        .fetch_add(deferred, Ordering::Relaxed);
                    self.counters
                        .relocation_budget_truncated_cycles
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            // HOW FAR PAST THE EVACUATION BUDGET THE TARGET EXEMPTION WENT.
            // Accumulated here and printed on the `zgc-relocate` line, because
            // the selection and the log are ~10 000 lines apart. Zero unless a
            // targeted page exceeded `max_evacuation_bytes` -- see the counter.
            {
                let overrun = reloc_set.targeted_overrun_bytes();
                if overrun != 0 {
                    self.counters
                        .relocation_targeted_overrun_bytes
                        .fetch_add(overrun, Ordering::Relaxed);
                }
            }
            // `FxHashSet`, not the SipHash `std` set (round 9 wave 5, `zgc5`):
            // it is probed once or twice per LIVE OBJECT below (the survivor
            // and obstacle filters, `highest_pinned_end`), and SipHash was
            // ~15 % of this function on `ZgcTlabStress 8 200`. Only
            // membership, `remove`, `min` and a collect-then-sort read it, so
            // the iteration order change is invisible.
            let mut selected: FxHashSet<u64> =
                reloc_set.pages().iter().map(|p| p.page_id).collect();
            // t1 wave 8 lane ZT3GN (`CRATONVM_ZGC_GEN_YOUNG_EVACUATE_ALL_PAGES`,
            // default ON inside the opt-in scoped slide): the nursery IS the
            // relocation set -- every admitted page, no occupancy cutoff, no
            // budget. The pin screen below still withholds pages.
            if scope.is_some() && self.counters.young_evac.all_pages.load(Ordering::Relaxed) {
                selected = candidates.iter().map(|c| c.page_id).collect();
            }
            // A pinned object makes its whole PAGE immovable.
            //
            // Page granularity, deliberately, and it is what G1 does with
            // `pin_region_for_addr`: the slide's placement probe reasons about
            // pages, so an object-granular pin would need the probe to route
            // around individual survivors inside a page it is otherwise
            // compacting — more machinery, and every extra rule in that probe
            // is a chance to overwrite something live. Dropping the page costs
            // one page's worth of reclaim for the duration of a critical
            // section, which is bounded by the section itself.
            // CONSERVATIVE JIT ROOTS PIN THEIR PAGE TOO.
            //
            // `gc_quiescence::pinned_jit_roots_snapshot()` is the VM's set of
            // addresses recovered by a CONSERVATIVE scan of JIT frames. The
            // root array is over-approximate by construction — `is_heap_addr`
            // is a range check, so an interior pointer or a plain `long` can
            // present as a root — which means the collector cannot rewrite the
            // slot it came from (it may not be a slot) and therefore must not
            // MOVE the object it names.
            //
            // G1 learned this on 2026-08-11: it was evacuating 6-8 of them per
            // pause and the remap then rewrote a Java `long`. It excludes those
            // regions from its collection set (`g1.rs:2364`, `:8390`). ZGC had
            // **no consumer of that snapshot at all** — correct while it never
            // moved anything, and wrong from the day compaction shipped
            // (2026-08-13).
            //
            // The failure is not a wild pointer, which is what makes it hard to
            // read: `compact_low_to` ZEROES the vacated span on purpose, so the
            // conservative root ends up pointing at a valid, all-zero header.
            // The reader then sees `num_slots=0` and walks off the end of a
            // zero-length object — the `zgc real: field index OOB index=N
            // num_slots=0` signature.
            // t1 wave 10 lane ZT3GN: on a scoped slide only, split `select_us`
            // (`CRATONVM_DBG_ZGC_RELOCATE_PHASES=1`) into `selector_us` (ages,
            // remembered-set registration, the selector), `pins_us` (the pin
            // screen) and the rest (`select_us`: the gates and the books).
            if scope.is_some() {
                phases.mark("selector");
            }
            let mut pins = self.critical_pin_addrs();
            pins.extend(cratonvm_gc_core::gc_quiescence::pinned_jit_roots_snapshot_honouring_movable(
                zgc_movable_pins_honoured(),
            ));
            // A VM TLAB whose owner could not retire (blocked in native, or
            // frozen in compiled code) still bumps into its reserved tail
            // when it resumes. The tail holds no registered base, so the sweep
            // is blind to it by construction; the SLIDE is not, and a page it
            // vacates is zeroed and re-issued. Pin both ends of every published
            // tail so its pages leave the relocation set, and see
            // `jit_tlab_skip_floor` below for the cursor. `zgc/vm_tlab.rs`.
            for (tail_start, tail_end) in self.jit_tlab_skip_regions() {
                pins.push(tail_start);
                pins.push(tail_end - 1);
            }
            if !pins.is_empty() {
                let page_span = Self::Z_LOGICAL_PAGE_BYTES;
                let mut dropped = 0usize;
                // zs s2/sl2 (`CRATONVM_ZGC_SLIDE_PIN_BY_BASE`, default ON, `=0`
                // kill): ALSO drop the page of the OBJECT the pin lies in.
                // Page membership is decided by an object's BASE, and a pin
                // word may be interior (the conservative JIT scan is a range
                // check: a derived cursor or a plain `long` qualifies). An
                // interior pin on page k+1 into an object based on page k used
                // to drop page k+1 only; page k stayed selected and the
                // object MOVED under a word nothing can rewrite. The high end
                // already resolves by containment (`compact_high_region`'s
                // `high_pins`), and the page-evac context resolves interiors
                // in `begin_page_evac_cycle`; this is the low end's form. The
                // raw word's own page is still dropped (kept as before).
                let by_base = zgc_slide_pin_by_base_enabled();
                // t1 wave 10 lane ZT3GN
                // (`CRATONVM_ZGC_GEN_YOUNG_EVACUATE_SCOPED_PINS`, default ON
                // inside the opt-in scoped slide): every admitted page is at
                // or above `scope.lo` (a page boundary), so a word below it
                // can name neither a selectable page nor an object based on
                // one (a base is at or below the word); and an interior word
                // above it is resolved against `scope.live` (= `live`, the
                // registered bases in `[lo, low_end)`) by binary search --
                // a base below `lo` is a straddler, its pages already blocked.
                let scoped_pins = scope
                    .filter(|_| self.counters.young_evac.scoped_pins.load(Ordering::Relaxed));
                let mut pins_skipped = 0usize;
                for &addr in &pins {
                    if let Some(s) = scoped_pins {
                        if addr < s.lo {
                            pins_skipped += 1;
                            continue;
                        }
                    }
                    if addr >= base && addr < low_end {
                        if selected.remove(&(((addr - base) / page_span) as u64)) {
                            dropped += 1;
                        }
                    }
                    if by_base && addr >= base && addr <= low_end {
                        // zs s3/rv2: a word is ALSO one past the end of the
                        // object below it. `resolve_interior_for_pin(addr)`
                        // answers the object BASED at `addr` when one is (a
                        // live neighbour starting where the pinned object ends
                        // is the normal case in a dense region), so the object
                        // the word is the end cursor of was never found and
                        // moved under it; and a word equal to `low_end` (the
                        // topmost object's end) was never screened. Resolving
                        // `addr - 1` too names that object, as the high end's
                        // `*p == from + size` test does. Over-pins at worst.
                        let probes = [
                            (addr < low_end).then_some(addr),
                            (addr > base).then(|| addr - 1),
                        ];
                        for probe in probes.into_iter().flatten() {
                            let found = match scoped_pins {
                                Some(_) => young_evacuate::scoped_pin_base(probe, live, |b| {
                                    Self::alloc_size(self.header_ref(b as *mut u8))
                                }),
                                None => self
                                    .resolve_interior_for_pin(probe)
                                    .map(|obj| obj.as_ptr() as usize),
                            };
                            if let Some(ob) = found {
                                if ob >= base
                                    && ob < low_end
                                    && selected.remove(&(((ob - base) / page_span) as u64))
                                {
                                    dropped += 1;
                                    self.counters
                                        .slide_pins_dropped_by_base
                                        .fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                }
                if pins_skipped > 0 {
                    self.counters
                        .young_evac
                        .pins_skipped
                        .fetch_add(pins_skipped, Ordering::Relaxed);
                }
                if dropped > 0 {
                    tracing::debug!(
                        target: "zgc",
                        dropped,
                        "zgc relocate: pages withheld from the relocation set because \
                         a JNI critical section pins an object on them"
                    );
                }
            }
            // zt t23/zc5 (R1, `CRATONVM_DBG_ZGC_RELOC_SELECT_CENSUS`): the
            // slide's selection after the pin screen, before the cycle gates,
            // for the census to compare. One relaxed load when no census is
            // pending.
            if scope.is_some() {
                phases.mark("pins");
            }
            self.reloc_select_census_note_slide(&selected, views.len());
            // zr z9/sl: a selective selection that names no page with a
            // survivor has nothing to copy. The slide would still pay the
            // partition pass over the whole live set to find that out; decline
            // instead (the free tail is retracted below as on any declined
            // cycle). Empty pages are destinations only, and their bytes are
            // already free blocks. Ahead of the cost gate (which would decline
            // the same selection, its fragmented garbage being 0) so the
            // reason is counted as this arm's.
            // zs s3/cr (`CRATONVM_ZGC_SELECTIVE_GATE_ON_SELECTION`): the
            // `=2` gate priced on the pages the slide is about to move -- the
            // selector's budget-bounded PREFIX after the pin screen -- rather
            // than on every page under the occupancy cutoff. On a heap whose
            // candidate live bytes exceed `max_evacuation_bytes` the old input
            // could admit a slide for fragmentation sitting on pages past the
            // prefix (or on pinned pages), and the slide then moved long-hole
            // pages that buy nothing while paying the whole-live-set rewrite.
            // Same gate function, same counters; `[1]` books the cycles the
            // two inputs disagree on in that direction.
            if let Some((frag, low_live, frag_in_reach)) = gate_deferred.take() {
                let frag_selected = cycle_gate_frag_on_selection(&candidates, &selected, &frag);
                let verdict = selective_cycle_gate(frag_selected, low_live, bump_free_permille);
                let old = selective_cycle_gate(frag_in_reach, low_live, bump_free_permille);
                self.counters.selective_relocate[selective_cycle_slot(verdict)]
                    .fetch_add(1, Ordering::Relaxed);
                self.counters.selective_gate_on_selection[0].fetch_add(1, Ordering::Relaxed);
                if old == SelectiveCycle::Admit && verdict == SelectiveCycle::Decline {
                    self.counters.selective_gate_on_selection[1].fetch_add(1, Ordering::Relaxed);
                }
                sel_cycle_declined = verdict == SelectiveCycle::Decline;
            }
            if sel_cycle_declined {
                // zs s1/sl: the cycle gate's decline. Not counted again:
                // `selective_relocate[5]` booked it at the decision.
                selected.clear();
            }
            // zs s2/sl2 (`CRATONVM_ZGC_TARGET_EXEMPTION_AFTER_PINS`, default ON,
            // `=0` kill): the two gates below exempt a TARGETED cycle (an
            // allocation failure named a window). They used to test the policy
            // (`target_pages.is_some()`), so a cycle whose targeted pages the
            // pin screen had all dropped was still exempt, and slid its
            // untargeted pages without the cost gate. The exemption now holds
            // only while a targeted page is still selected.
            let targeted_cycle = targeted_cycle_after_pins(
                policy.target_pages,
                &selected,
                zgc_target_exemption_after_pins_enabled(),
            );
            if policy.target_pages.is_some() && !targeted_cycle {
                self.counters
                    .slide_targets_pin_dropped
                    .fetch_add(1, Ordering::Relaxed);
            }
            if sel_frag.is_some()
                && !selected.is_empty()
                && !targeted_cycle
                && !candidates
                    .iter()
                    .any(|c| c.live_bytes > 0 && selected.contains(&c.page_id))
            {
                self.counters.selective_relocate[2].fetch_add(1, Ordering::Relaxed);
                selected.clear();
            }
            // ---- IS THE SLIDE WORTH A WHOLE-LIVE-SET PASS? ------------------
            //
            // `CRATONVM_ZGC_RELOCATE_COST_GATE` (round 9 wave 6, `zgc6`;
            // default ON since wave 9, `zgc9`, `=0` turns it off -- see
            // `zgc_relocate_cost_gate_enabled` for the measurements). The
            // slide's cost is NOT the copy: every compacting
            // cycle ends in the reference rewrite, which walks every slot of
            // every live object whatever moved. Its benefit is bounded by the
            // garbage on the selected pages, and that garbage is ALREADY on the
            // free list (the sweep put it there); the slide only makes it
            // contiguous. Measured on `ZgcTlabStress 8 200` (w5b): 5-6 cycles
            // each moving 17-65 K of 2.6-10 M survivors, 250-950 ms apiece,
            // most reporting `reclaimed=0` -- ~3 s of an ~13 s run, which is
            // the whole gap to `CRATONVM_ZGC_RELOCATE=0`.
            //
            // So, like HotSpot's per-page live-ratio cost model, decline the LOW
            // slide when the selected pages' garbage is under
            // 1/`ZGC_RELOCATE_COST_GATE_RATIO` of the low region's live bytes --
            // but never when an allocation failure asked for a window
            // (`target_pages`), and never on a tight heap (less than
            // `ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE` free), where contiguity is what
            // the next large request needs. A declined cycle is the selector's
            // own empty answer below: the cursor still retracts into the free
            // tail and the large-object end is still compacted.
            // zr z2/cp: never after a hard allocation failure. The gate prices
            // a slide against a heap with room to spare; a cycle that follows
            // a refused request is the case the slide exists for, and its
            // `free_permille` leg would read the post-sweep tail, not the
            // contiguity the failed request lacked.
            //
            // zt t14/pr2 retired the zs s1/sl opt-in that also exempted a cycle
            // the pressure tier widened for a TIGHT bump headroom: on its
            // deciding row (`Zr2CpCapacity mosaic -Xmx256m`) the gate declined
            // nothing (`cost_gated=0` on every cycle), so the exemption had no
            // cycle to change.
            if !selected.is_empty()
                && !targeted_cycle
                && pressure_tier != Some(RelocatePressure::AllocFailure)
                && zgc_relocate_cost_gate_enabled()
                // t1 wave 6 handback (Y4): nor the cost gate -- a scoped
                // slide's copy is O(young live) by construction.
                && scope.is_none()
            {
                let low_live: usize = candidates.iter().map(|c| c.live_bytes).sum();
                // zr z9/sl: on the selective arm the benefit is the garbage
                // the slide makes contiguous (fragmented holes), not what the
                // free list already holds in usable blocks.
                let selected_garbage: usize = candidates
                    .iter()
                    .filter(|c| selected.contains(&c.page_id))
                    .map(|c| match sel_frag.as_ref() {
                        Some(frag) => frag.get(c.page_id as usize).copied().unwrap_or(0),
                        None => c.garbage_bytes(),
                    })
                    .sum();
                let free_permille =
                    arena.remaining().saturating_mul(1000) / arena.capacity().max(1);
                if relocate_cost_gate_declines(selected_garbage, low_live, free_permille) {
                    self.counters
                        .relocation_cost_gated
                        .fetch_add(1, Ordering::Relaxed);
                    selected.clear();
                }
            }
            // ---- zt t27/pc2: A SLIDE THAT CANNOT MOVE ANYTHING IS DECLINED --
            //
            // `CRATONVM_ZGC_SLIDE_NO_MOVE_DECLINE` (default ON, `=0` kill).
            // `h7_OOM1_KV` (`zgc-zt25hunt7b-*` B2): 62 of 65 cycles selected
            // pages, ran the partition pass, the slide loop and the
            // rewrite/screen pass over all ~893 k survivors, and moved nothing
            // (2.78 s of slide). `slide_moves_nothing` proves "no selected
            // survivor has a window below it" from the size table this cycle
            // already filled, in one header-free walk that stops at the first
            // survivor that might move; when it holds, the selection is
            // cleared here and the cycle takes the empty-selection arm below.
            //
            // What the cleared cycle no longer does, each checked against the
            // moved-nothing path it replaces:
            // * page ages, the remembered-set registration, the pressure tier,
            //   the failure latch's consumption and the selection census all
            //   ran above, as before;
            // * the relocation-set books (`books_final`, below) record the
            //   empty set this cycle relocates -- HotSpot books what it
            //   relocates; the moved-nothing slide booked pages it moved
            //   nothing from;
            // * `compact_low_to` with nothing moved dropped every free block
            //   overlapping `[slide_floor, dest)` (holes on unselected pages
            //   between selected ones included) until the next sweep, and
            //   re-published the selected pages' free spans above `dest`;
            //   the empty arm keeps the sweep's free list as it is -- the same
            //   bytes or more, never fewer -- and retracts the tail;
            // * the large-object end is compacted on both arms.
            // Not on an evacuating arm (`CRATONVM_ZGC_SLIDE_EVACUATE`, the
            // region relocation): those copy into the bump tail and need no
            // window below a survivor.
            //
            // zt t35/zrr (CR-E4): "an evacuating arm" means one whose gate
            // ADMITS this cycle. An arm that is requested but whose gate will
            // refuse (no tail headroom, a pressure tier, a failure latch) runs
            // the in-place slide, and that slide moving nothing dropped the free
            // blocks below `dest` exactly as above: on `Zr7CmFixedWork` a
            // 159 MiB free block went at the first refused cycle and the low
            // cursor never came down again (D 37 MiB, EV 199 MiB). Same gate as
            // the arm's own, read on the same inputs.
            // t1 wave 6 lane Y4: never on a scoped slide (its builder refuses
            // them too; read again here so the two cannot disagree).
            let evac_requested_here = scope.is_none()
                && (zgc_slide_evacuate_enabled() || region_forwarding::zgc_region_relocate_enabled());
            let evac_admitted_here = evac_requested_here
                && slide_evacuate_gate_admits(
                    candidates
                        .iter()
                        .filter(|c| selected.contains(&c.page_id))
                        .map(|c| c.live_bytes)
                        .sum(),
                    cycle_followed_failure,
                    pressure_tier.is_none(),
                    self.publish_vacated_enabled.load(Ordering::Relaxed),
                    arena.low_bump_headroom(),
                    arena.to_space_is_open(),
                );
            if !selected.is_empty()
                && !evac_admitted_here
                && selected.iter().all(|p| (*p as usize) < views.len())
            {
                // Every selected id is a page of this grid (checked above), so
                // the dense membership answers exactly what `selected` does.
                let mut membership = vec![false; views.len()];
                for &p in &selected {
                    membership[p as usize] = true;
                }
                if slide_moves_nothing(
                    live,
                    &sizes,
                    base,
                    low_end,
                    Self::Z_LOGICAL_PAGE_BYTES,
                    &membership,
                ) {
                    self.counters
                        .slide_no_move_declined
                        .fetch_add(1, Ordering::Relaxed);
                    // The arm's refusal, booked as the gate below would have.
                    if evac_requested_here {
                        self.counters.slide_evacuate[1].fetch_add(1, Ordering::Relaxed);
                        if region_forwarding::zgc_region_relocate_enabled() {
                            self.note_region_relocate_fallback();
                        }
                    }
                    selected.clear();
                }
            }
            if books_final {
                // zs s2/sl2: the set the slide relocates (see `books_final`).
                // `selected` has no writer past this point.
                let relocated_set = if selected.len() == reloc_set.len() {
                    None
                } else {
                    Some(reloc_set.restricted_to(|id| selected.contains(&id), policy.target_pages))
                };
                let booked = relocated_set.as_ref().unwrap_or(&reloc_set);
                self.counters.jfr.note_relocation_set(
                    jfr_facts::ZJfrRelocationSet::from_selection(&candidates, booked),
                );
                self.counters
                    .local
                    .targeted_pages_selected
                    .fetch_add(booked.targeted_selected(), Ordering::Relaxed);
                if pressure_tier.is_some() {
                    self.counters.pressure_select[2]
                        .fetch_add(booked.total_live_bytes(), Ordering::Relaxed);
                }
            }
            phases.mark("select");
            // Dense membership for the per-live-object probes below (the
            // survivor/obstacle partition and the slide's placement probe).
            // Built AFTER the pin screen and the cost gate, which are the only
            // writers of `selected`, so it answers exactly what
            // `selected.contains` answers -- without a hash per live object.
            let selected_dense: Vec<bool> = {
                let len = selected
                    .iter()
                    .map(|p| *p as usize + 1)
                    .max()
                    .unwrap_or(0)
                    .max(views.len());
                let mut v = vec![false; len];
                for &p in &selected {
                    v[p as usize] = true;
                }
                v
            };
            let is_selected = |pg: u64| -> bool {
                usize::try_from(pg)
                    .ok()
                    .and_then(|i| selected_dense.get(i).copied())
                    .unwrap_or(false)
            };
            if selected.is_empty() {
                scoped_nothing_selected = scope.is_some();
                // Nothing profitable to move AT THE LOW END. Not a failure --
                // it is the selector doing its job on a heap whose pages are
                // all dense.
                //
                // A SKIP, not a return, since 2026-08-29: the large-object end
                // is compacted below and may move objects whose slots this
                // function still has to rewrite. Returning here meant a heap
                // whose low end was dense could never have its high end
                // repaired, which is exactly the shape `TestMVStoreTool`
                // fails in.
                reclaimed = arena.retract_cursor_into_free_tail();
            } else {
                let page_of = |addr: usize| ((addr - base) / Self::Z_LOGICAL_PAGE_BYTES) as u64;
                // ONE PASS over the low-region live set partitions it three ways
                // (round 9 wave 6, `zgc6`): the survivors on selected pages, the
                // OBSTACLES and `highest_pinned_end`, each documented where it is
                // used below. They used to be three separate filters over the
                // whole live set, two of them reading every unselected
                // survivor's header -- on `ZgcTlabStress 8 200` that is three
                // walks of 10 M objects to move a few ten-thousand.
                // `highest_pinned_end` is computed BEFORE the slide now; it
                // reads only unselected survivors, which the slide never moves
                // and never writes into (the placement probe below keeps every
                // destination inside selected pages and clear of obstacles), so
                // the value is the one the old post-slide pass read.
                //
                // Split across `workers` (round 9 wave 7, `zgc7`): each chunk
                // builds its own three answers and they are concatenated (the
                // two lists are sorted below anyway) and maxed. With one worker
                // it is the single loop it was.
                let sizes_ref: &[u32] = &sizes;
                // zs s1/sl (`CRATONVM_ZGC_SLIDE_SIZE_FROM_TABLE`, default ON,
                // `=0` kill): a selected survivor carries its size cell into
                // the slide loop, which then sizes it from the cell instead of
                // re-reading its header (one of the four reads of a moved
                // object's header, `zgc-zr7pc-...-four-times`). Same answer:
                // the cell IS `size_cell(header)` of this base before any byte
                // moved, `0` (header read) where the table is off or the
                // object unsizable, and the slide never writes over a survivor
                // it has not visited yet.
                let slide_cells = zgc_slide_size_from_table_enabled();
                let unsizable_obstacle = zgc_slide_unsizable_obstacle_enabled();
                let partitioned = self.reloc_par_chunks(live, workers, |chunk: &[usize]| {
                    let mut survivors: Vec<(usize, u32)> = Vec::new();
                    let mut obstacles: Vec<(usize, usize)> = Vec::new();
                    let mut highest_pinned_end = base;
                    // zr z7/pc: this chunk's slice of the size table (empty
                    // on the kill arm -- every lookup then reads the header).
                    let cells = sizes_ref
                        .get(zgc_chunk_offset(live, chunk)..)
                        .unwrap_or(&[]);
                    for (i, b) in chunk.iter().copied().enumerate() {
                        if b < base || b >= low_end {
                            continue;
                        }
                        // Survivors ON THE SELECTED PAGES ONLY. An object on an
                        // unselected page must not move, so the slide's destination
                        // cursor has to start above the highest unselected survivor
                        // -- see below.
                        if is_selected(page_of(b)) {
                            let cell = if slide_cells {
                                cells.get(i).copied().unwrap_or(0)
                            } else {
                                0
                            };
                            survivors.push((b, cell));
                            continue;
                        }
                        let sz = self.size_via_cell(cells.get(i).copied().unwrap_or(0), b);
                        // An OBSTACLE -- see the note where `obstacles` is sorted.
                        if let Some(sz) = sz {
                            if page_of(b) != page_of(b + sz - 1) {
                                obstacles.push((b, b + sz));
                            }
                        } else if unsizable_obstacle {
                            // zs s1/sl: an UNSIZABLE survivor on an unselected
                            // page has no known end, and the census booked it
                            // as 0 live bytes, so the pages its body may cover
                            // read as empty and are selected as destinations.
                            // The post-slide `live_ceiling` screen fails closed
                            // on it, but only after the memmove. Bound it the
                            // way that screen does: by `low_end`, so no
                            // destination at or above its base is chosen. The
                            // selected-page case already stops the slide (the
                            // break in the loop below).
                            obstacles.push((b, low_end));
                        }
                        // `highest_pinned_end` -- see the note where it is used.
                        let size = sz.unwrap_or(0);
                        let end = if size > low_end.saturating_sub(b) {
                            tracing::warn!(
                                target: "cratonvm::gc::guard",
                                addr = b,
                                size,
                                low_end,
                                "zgc relocate: a pinned survivor's extent runs past the \
                                 bump cursor -- refusing to reclaim this cycle rather than \
                                 trust the header"
                            );
                            low_end
                        } else {
                            b + size
                        };
                        if end > highest_pinned_end {
                            highest_pinned_end = end;
                        }
                    }
                    (survivors, obstacles, highest_pinned_end)
                });
                let mut survivors: Vec<(usize, u32)> = Vec::new();
                let mut obstacles: Vec<(usize, usize)> = Vec::new();
                let mut highest_pinned_end = base;
                for (part_survivors, part_obstacles, part_end) in partitioned {
                    survivors.extend(part_survivors);
                    obstacles.extend(part_obstacles);
                    highest_pinned_end = highest_pinned_end.max(part_end);
                }
                survivors.sort_unstable();
                phases.mark("partition");

                // The slide may only use space below the first page it is allowed
                // to disturb. Sliding into an unselected page would overwrite
                // survivors this cycle promised not to touch.
                // From the FILTERED set, not `reloc_set`: a pinned page dropped
                // above may have been the lowest, and starting the slide at a page
                // that is no longer selected would place survivors on top of the
                // very object the pin exists to hold still.
                let first_selected_page = *selected.iter().min().expect("non-empty, checked above");
                let slide_floor = base + first_selected_page as usize * Self::Z_LOGICAL_PAGE_BYTES;

                // OBSTACLES: live objects that will not move but occupy bytes of a
                // page that IS selected.
                //
                // Page membership is decided by an object's BASE, so an object
                // based in an unselected page and extending across the boundary
                // keeps its tail inside the selected page above -- and it is not in
                // `survivors`, so it never moves. "This page is selected" and
                // "these bytes are free" are therefore different claims, and the
                // slide had only the first one.
                //
                // The probe aimed straight at the hazard: when a span touched an
                // unselected page it restarted at `base + (blocked + 1) * PAGE` --
                // the first byte of the next page, which is exactly where a
                // straddler from the previous page lies. `slide_floor` has the same
                // shape. Measured by
                // `compaction_never_writes_into_an_object_it_did_not_move`.
                //
                // The list is tiny -- only an object crossing a boundary out of an
                // unselected page can qualify, at most one per boundary -- so a
                // sorted `Vec` and a short scan cost nothing beside the slide.
                // Collected by the partition pass above.
                obstacles.sort_unstable();
                // End of an obstacle overlapping `[lo, hi)`, if any.
                let blocked_until = |lo: usize, hi: usize| -> Option<usize> {
                    obstacles
                        .iter()
                        .take_while(|(ob, _)| *ob < hi)
                        .find(|(_, oe)| *oe > lo)
                        .map(|(_, oe)| *oe)
                };

                // The floor itself can land inside a straddler's tail.
                let mut dest = slide_floor;
                if let Some(end) = blocked_until(dest, dest + 1) {
                    dest = end;
                }
                // ---- zs s2/sl2: EVACUATE INSTEAD OF SLIDING (opt-in) ---------
                //
                // `CRATONVM_ZGC_SLIDE_EVACUATE=1`. The selected pages' survivors
                // are copied into the low BUMP TAIL (the arena's to-space
                // window, `Arena::alloc_to_space`: bump-only above the cursor,
                // never a free-list hole, so never inside a selected page) and
                // every selected page's bytes outside what stayed are handed to
                // the free list whole. No placement probe, no overlapping
                // memmove, no ordering constraint between survivors: a copy's
                // destination is fresh memory. That is ZGC's own relocation
                // model (evacuate a page, free it whole) and the stepping stone
                // of the concurrent-relocation plan
                // (`zgc-zs1sl-proposal-concurrent-relocation-with-load-barriers`,
                // stage 1, re-scoped to the arena): a from-page that is not
                // written by its own evacuation is a from-page that a later
                // stage can keep quarantined while a barrier heals stale reads.
                //
                // Only where the tail can take the whole set (headroom at least
                // the selected live bytes plus an eighth plus one large-object
                // minimum), never after an allocation failure and never on a
                // pressure tier (a full heap needs the slide's in-place
                // contiguity, it has no tail to evacuate into), and only with
                // the vacated-span publication on (`publish_vacated`): without
                // it the evacuated pages' bytes would be lost. Everything after
                // the copy -- the screen, the rewrite, the registry rebuild,
                // the forwarding table, the `PointerMap` -- is the slide's own,
                // fed the same pairs.
                // zt t23/zc5 (R2, `CRATONVM_ZGC_REGION_RELOCATE=1`, opt-in): the
                // same arm with its copy loop replaced by a drain of a region
                // relocation cycle (`region_relocate_evacuate`); it asks for
                // the arm by itself and passes the same gate below.
                let region_relocate =
                    scope.is_none() && region_forwarding::zgc_region_relocate_enabled();
                // zt t23/zc7 (R3): acts only on the region arm.
                if region_relocate {
                    region_quarantine = region_forwarding::zgc_region_quarantine_mode();
                }
                let evac_requested =
                    scope.is_none() && (zgc_slide_evacuate_enabled() || region_relocate);
                let evacuate = evac_requested
                    && slide_evacuate_gate_admits(
                        candidates
                            .iter()
                            .filter(|c| is_selected(c.page_id))
                            .map(|c| c.live_bytes)
                            .sum(),
                        cycle_followed_failure,
                        pressure_tier.is_none(),
                        self.publish_vacated_enabled.load(Ordering::Relaxed),
                        arena.low_bump_headroom(),
                        arena.to_space_is_open(),
                    );
                // zt t2/rv (`zgc-zs4rv3-wave-s3-review-residuals-CLOSED-20261019.md`
                // item 5): `[0]` is booked below, once the window opened; a
                // refused window is a to-space stop (`[2]`) only, not also an
                // evacuation.
                if evac_requested && !evacuate {
                    self.counters.slide_evacuate[1].fetch_add(1, Ordering::Relaxed);
                    if region_relocate {
                        self.note_region_relocate_fallback();
                    }
                }
                // Extents of selected-page survivors the evacuation left in
                // place (unsizable: to `low_end`, fail-closed like the screen).
                let mut evac_stays: Vec<(usize, usize)> = Vec::new();
                // `(from, size)` of every survivor the evacuation copied out,
                // ascending by `from` (the loop walks `survivors` in order).
                let mut evac_moves: Vec<(usize, usize)> = Vec::new();
                fail_stop.arm();
                // zr z4/un (`CRATONVM_ZGC_SLIDE_BUFFER_TRIM`): size the pair
                // list once. Grown by doubling it ended at up to 2x the moves,
                // and each growth briefly held the old and the new buffer.
                if slide_trim {
                    pairs.reserve(survivors.len());
                }
                if evacuate {
                    let opened = arena.open_to_space().is_some();
                    if opened {
                        self.counters.slide_evacuate[0].fetch_add(1, Ordering::Relaxed);
                    } else {
                        // zs s3/cr: a refused window (one already open) moves
                        // nothing; booked as a to-space stop, not left to read
                        // as an evacuation that found nothing to copy.
                        self.counters.slide_evacuate[2].fetch_add(1, Ordering::Relaxed);
                    }
                    // Once halted (no window, an unsizable header, the tail
                    // exhausted) every later survivor stays where it is. An
                    // unsizable one bounds everything above it fail-closed (the
                    // screen does the same), so evacuating past it would only
                    // strand the copies' old bytes on pages that cannot be freed.
                    let mut halted = !opened;
                    if region_relocate {
                        // zt t23/zc5 (R2): the loop below, through the cycle:
                        // same order, sizes, destinations and stay list.
                        let (drain, cycle) = self.region_relocate_evacuate(
                            &mut arena, base, low_end, &survivors, halted,
                        );
                        // zt t23/zc7: `pairs` is empty here (nothing but this
                        // arm fills it before the large-object end), so the
                        // cycle's installs are its first `slide_cycle_pairs`.
                        debug_assert!(pairs.is_empty(), "the region drain fills `pairs` first");
                        if cycle.is_some() {
                            slide_cycle_pairs = drain.pairs.len();
                        }
                        slide_cycle = cycle;
                        moved += drain.pairs.len();
                        low_moved_bytes += drain.moved_bytes;
                        pairs.extend(drain.pairs);
                        evac_moves = drain.moves;
                        evac_stays = drain.stays;
                    }
                    for (from, cell) in survivors.iter().copied().filter(|_| !region_relocate) {
                        let size = self.size_via_cell(cell, from);
                        if halted {
                            evac_stays.push((from, size.map_or(low_end, |sz| from + sz)));
                            continue;
                        }
                        let Some(size) = size else {
                            tracing::warn!(
                                target: "cratonvm::gc::guard",
                                addr = from,
                                "zgc relocate: unsizable survivor stops the evacuation"
                            );
                            evac_stays.push((from, low_end));
                            halted = true;
                            continue;
                        };
                        if size > Self::MAX_ALLOC_EXTENT {
                            evac_stays.push((from, from + size));
                            continue;
                        }
                        let Some(to) = arena.alloc_to_space(size, 8).map(|p| p as usize) else {
                            // The headroom check makes this unreachable short of
                            // a racing reservation; stopping is the safe answer
                            // (the survivors left stay, their pages stay).
                            self.counters.slide_evacuate[2].fetch_add(1, Ordering::Relaxed);
                            evac_stays.push((from, from + size));
                            halted = true;
                            continue;
                        };
                        debug_assert!(to >= low_end, "to-space lies above the old cursor");
                        // SAFETY: `size` bytes are live at `from` (a selected
                        // survivor, sized above); `to` was just handed out by the
                        // to-space window for `size` bytes (committed by
                        // `hand_out`) and lies at or above the old low cursor,
                        // which every byte of the relocation set is below, so the
                        // ranges cannot overlap.
                        unsafe {
                            std::ptr::copy_nonoverlapping(from as *const u8, to as *mut u8, size)
                        };
                        pairs.push((from, to));
                        evac_moves.push((from, size));
                        moved += 1;
                        low_moved_bytes += size;
                        if moved == 1 && zgc_dbg_slide_panic_enabled() {
                            panic!("CRATONVM_DBG_ZGC_SLIDE_PANIC: injected after the first copy");
                        }
                    }
                    // zs s4/rv3: close only the window THIS slide opened; a
                    // refused open means another owner's window is up, and
                    // closing it would drop the floor guarding its
                    // destinations (latent: no other opener is wired today).
                    if opened {
                        let _ = arena.close_to_space();
                    }
                    self.counters.slide_evacuate[3].fetch_add(low_moved_bytes, Ordering::Relaxed);
                }
                for (from, cell) in survivors.into_iter().filter(|_| !evacuate) {
                    let Some(size) = self.size_via_cell(cell, from) else {
                        // A header this collector cannot size cannot be moved, and
                        // nothing above it may move either or the slide would run
                        // over it. Stop here rather than guess.
                        //
                        // THIS BREAK IS AN AMPLIFIER, and the live-ceiling check
                        // after the loop is what disarms it. Abandoning the loop
                        // leaves `dest` at `from`, so every selected-page survivor
                        // ABOVE this one -- all of which are alive and none of
                        // which have moved -- is above the cursor `compact_low_to`
                        // is about to retract to. Their bytes get zeroed and
                        // handed back to the bump allocator while the object-start
                        // registry still names them, and the next allocation
                        // writes over a contiguous RUN of live objects. That is
                        // how one unsizable header becomes "492 of 27858 survivors
                        // could not be walked" a cycle later, with the offending
                        // headers decoding as whatever String the allocator put
                        // there. See the reopened
                        // `zgc-rewrite-pass-walks-off-a-reference-array` page.
                        //
                        // Deliberately NOT fixed by setting `dest = low_end` here.
                        // That would be a second mechanism for the same property,
                        // and the two would drift on exactly the case that matters.
                        // The check on the answer belongs in one place, after every
                        // exit path from this loop.
                        tracing::warn!(
                            target: "cratonvm::gc::guard",
                            addr = from,
                            "zgc relocate: unsizable survivor stops the slide"
                        );
                        dest = from;
                        break;
                    };
                    // THE DESTINATION MUST LIE ENTIRELY INSIDE SELECTED PAGES.
                    //
                    // `ZRelocationSet::select` ranks by descending garbage ratio
                    // and takes a prefix, so the selected ids are an arbitrary,
                    // NON-CONTIGUOUS set -- every page at or above
                    // `max_live_occupancy` is skipped. Until 2026-08-14 this loop
                    // marched one cursor up from `slide_floor` and placed every
                    // survivor consecutively, so as soon as the selected pages'
                    // live bytes exceeded the gap below the first dense page, this
                    // memmove copied survivors straight over the live objects ON
                    // it. Silent heap corruption; it reached the outside world as
                    // `compaction must not raise the cursor: 15348137664 > ...`
                    // (a clobbered header read back as a 1.9-billion-element
                    // array) and, on one run, as a bare SIGSEGV.
                    //
                    // The comment eight lines above the old code said the cursor
                    // "has to start above the highest unselected survivor". That
                    // was the right rule and the code implemented something else.
                    //
                    // Skipping rather than clamping keeps the selector's choice:
                    // pages above a dense one are still compacted, into the next
                    // selected page, which is what makes a non-contiguous
                    // selection worth having at all.
                    let span = size.max(1);
                    let mut probe = dest;
                    let mut chosen: Option<usize> = None;
                    // Past the interior-probe window: never moved (zr z1/rl,
                    // see `MAX_ALLOC_EXTENT`). `chosen = None` is the existing
                    // stay-put arm, cursor continued above it.
                    if size > Self::MAX_ALLOC_EXTENT {
                        probe = from;
                    }
                    while probe < from {
                        // zs s2/sl2: rounded on the OBJECT grid (`base + 8k`),
                        // the high slide's rule, not on absolute addresses.
                        // Every object start is `base + 8k` (`Arena::alloc`
                        // rounds offsets) and nothing asserts the arena base
                        // itself is 8-aligned (its `Layout` is align 1). On
                        // an 8-aligned base the two rules give the same
                        // address, so no reachable run changes; on an
                        // unaligned one the absolute rule placed survivors
                        // off the grid every walker strides.
                        let cand = base + ((probe - base + 7) & !7);
                        // Only a strictly-downward move is worth anything, and an
                        // upward one would overwrite a survivor not yet visited.
                        if cand >= from || cand + span > low_end {
                            break;
                        }
                        let first_page = page_of(cand);
                        let last_page = page_of(cand + span - 1);
                        match (first_page..=last_page).find(|pg| !is_selected(*pg)) {
                            // The span would touch an unselected page: restart the
                            // probe at the page after it. Strictly increasing, so
                            // this terminates in at most one pass over the grid.
                            Some(blocked) => {
                                probe = base + (blocked as usize + 1) * Self::Z_LOGICAL_PAGE_BYTES;
                            }
                            // Every page of the span is selected -- which still
                            // does not mean the bytes are free. Re-check against the
                            // straddlers that reach into these pages from below,
                            // and restart above the one in the way. Also strictly
                            // increasing (an obstacle's end exceeds `cand`), so the
                            // loop still terminates.
                            None => match blocked_until(cand, cand + span) {
                                Some(end) => probe = end,
                                None => {
                                    chosen = Some(cand);
                                    break;
                                }
                            },
                        }
                    }
                    match chosen {
                        // The match GUARD commits the destination, which is
                        // dev's independently-arrived-at form of the same fix
                        // this branch built (`ensure_committed_span`, dropped in
                        // the merge as the duplicate it became). Keeping the
                        // evidence, because it is what turned this from a
                        // suspicion into a diagnosis:
                        //
                        // "Inside the arena" is not "mapped". The arena commits
                        // granules on demand and hands them back via
                        // `decommit_unbumped_middle` / `decommit_free_blocks`
                        // while their addresses stay reserved, so a destination
                        // screened only by page and liveness can name memory
                        // returned to the OS.
                        //
                        // Observed, not inferred: a fault-time witness
                        // (`reservation::recent_decommit_covering`) reports the faulting address
                        // INSIDE a decommitted granule, the access is a WRITE,
                        // `offset_into_span` is 0x0 in every crash, and the
                        // frame is this one. See
                        // the retired
                        // `bug-box-unbox-intrinsic-segv-under-relocation-20260902`
                        // write-up, and the root-cause record it points at.
                        Some(to) if arena.commit_for_relocation(to - base, size) => {
                            debug_assert!(to < from, "the slide must never move an object UP");
                            // SAFETY: `size` bytes are live at `from`, `to` is
                            // inside the arena and strictly below `from`, the
                            // guard above proved `[to, to + size)` is mapped
                            // read-write, and the regions may overlap -- `copy`
                            // is memmove, correct in that direction.
                            unsafe { std::ptr::copy(from as *const u8, to as *mut u8, size) };
                            pairs.push((from, to));
                            moved += 1;
                            low_moved_bytes += size;
                            dest = to + size;
                            // `CRATONVM_DBG_ZGC_SLIDE_PANIC=1` (zr z2/r2): the
                            // positive control for `ZSlideFailStop` -- a panic
                            // after the first memmove, which must abort.
                            if moved == 1 && zgc_dbg_slide_panic_enabled() {
                                panic!("CRATONVM_DBG_ZGC_SLIDE_PANIC: injected after the first memmove");
                            }
                        }
                        // The destination is free space the give-back returned
                        // to the OS and the OS would not take back. Same answer
                        // as "nowhere below it": the object stays put. See
                        // `Arena::commit_for_relocation` for why a slide has to
                        // ask at all.
                        Some(from_stay) => {
                            tracing::warn!(
                                target: "cratonvm::gc::guard",
                                addr = from,
                                size,
                                dest = from_stay,
                                "zgc relocate: the OS refused the LOW slide's destination -- leaving the survivor in place"
                            );
                            dest = from + size;
                        }
                        // Nowhere below it inside a selected page: it stays put,
                        // and the cursor continues above it so a later survivor
                        // cannot be placed on top of it.
                        None => dest = from + size,
                    }
                }
                // The cursor may only drop to the compacted end if nothing that
                // STAYED PUT lives above it.
                //
                // Only UNSELECTED survivors count here. An object on a selected
                // page has already been slid down, so its entry in `live` names an
                // address it no longer occupies -- taking the maximum over the
                // whole live set pins the cursor at the pre-compaction top and
                // reclaims exactly nothing, which is what the first draft did.
                //
                // `highest_pinned_end` itself is computed by the partition pass
                // before the slide (round 9 wave 6). What it guards against: an
                // extent that runs past the bump cursor is impossible for a live
                // object and means the header is not one -- which is how the
                // 2026-08-14 corruption presented: a clobbered header sized at
                // 15,348,137,664 bytes on a 168 MB arena, carried into
                // `compact_low_to` as a cursor 91x the heap. It is refused there
                // rather than allowed to set the cursor: pinning at `low_end`
                // reclaims nothing this cycle, which is the conservative answer
                // and strictly better than either trusting the number or
                // panicking -- `alloc_size` has no arena to check against, and
                // this is the caller that does.
                // ---- NOTHING LIVE MAY END ABOVE THE NEW CURSOR ---------------
                //
                // `compact_low_to` zeroes `[new_cursor, cursor)` and hands the span
                // straight back to the bump allocator. A live object still up there
                // is therefore erased AND its address re-issued: the registry keeps
                // its base, a mutator writes a fresh object over it, and the next
                // slide's rewrite pass reads a "header" made of somebody else's
                // payload. That is precisely the failure
                // `rewrite_target_walkable_sized` reports -- a contiguous run of
                // registered bases whose class ids and slot counts decode as ASCII,
                // because what is actually there now is string data.
                //
                // `dest` and `highest_pinned_end` above are each derived from a
                // DIFFERENT subset of `live` -- selected-page survivors and
                // unselected ones -- and every argument that their maximum covers
                // the whole live set is an argument about the partition, not a
                // check on the answer. This is the check on the answer: one pass
                // over `live`, resolved through the slide's own from->to pairs, of
                // the one property the allocator is about to depend on.
                //
                // Cheap next to the slide (one hash probe and one `alloc_size` per
                // survivor, against a memmove per survivor), and it can only
                // RAISE the cursor -- i.e. reclaim less. Losing a cycle's reclaim
                // is a cost; handing out occupied memory is heap corruption whose
                // symptom surfaces cycles later in an unrelated subsystem.
                phases.mark("slide");
                // zt t27/pc2: a non-empty selection that moved nothing (only
                // low pairs exist yet; the large-object end appends later).
                if !evacuate && pairs.is_empty() {
                    self.counters
                        .slide_no_move_missed
                        .fetch_add(1, Ordering::Relaxed);
                }
                // zr z4/un (`CRATONVM_ZGC_SLIDE_BUFFER_TRIM`): only the
                // separate-screen arm reads this index. The fused arm (the
                // default) builds its own over both ends below, so building
                // this one there was a sort of every pair and ~20 bytes a move
                // held for nothing beside the fused arm's copy.
                // zt t23/zc7: every forward index of this slide comes from
                // `ZForwardIndex::for_slide`, which asks the region cycle first
                // when the region arm drained one (the same answers).
                let moved_to: Option<relocate::ZForwardIndex> = if fused && slide_trim {
                    None
                } else {
                    Some(relocate::ZForwardIndex::for_slide(
                        &pairs,
                        slide_cycle.as_ref(),
                    ))
                };
                let moved_to_bytes = moved_to.as_ref().map_or(0, zgc_fwd_index_bytes);
                self.counters.fp.note_slide_buffers(
                    pairs.capacity() * std::mem::size_of::<(usize, usize)>(),
                    moved_to_bytes,
                    0,
                );
                // The VACATED-SPAN screen (see "WHAT THE SLIDE EMPTIED" below)
                // rides on this same pass (round 9 wave 6, `zgc6`). It used to
                // be a second walk of the live set that also MATERIALISED it:
                // two `Vec<(usize, usize)>` runs, their merge and a prefix-max
                // `Vec` -- about 400 MB of fresh allocation per cycle at 10 M
                // survivors -- to answer "does any live extent overlap this
                // page's span" for a few selected pages. Each candidate span is
                // now marked as it is met. The spans are pre-clipped at the page
                // top only; the clip at the new cursor is applied once the
                // cursor is known, and gives the same answer, because every
                // extent this pass sees ends at or below `live_ceiling`, which
                // the cursor is never below. `(lo, page_hi, blocked)`,
                // ascending and disjoint.
                let publish_vacated = self.publish_vacated_enabled.load(Ordering::Relaxed);
                let mut vacate_candidates: Vec<(usize, usize, bool)> = Vec::new();
                // zs s3/cr: the free holes below the old cursor (absolute,
                // ascending), kept for the quarantine, which withholds only the
                // bytes the evacuation vacated (the holes were free already and
                // stay on the free list). Empty off the evacuating arm.
                let mut evac_holes: Vec<(usize, usize)> = Vec::new();
                if evacuate {
                    // zs s2/sl2: every selected page, clipped at the old cursor,
                    // minus what stayed in it (straddlers from unselected pages,
                    // survivors the evacuation left), widened over the free
                    // holes that touch it (dead already; `compact_low_to`
                    // drops a kept block that overlaps a span, so a hole half
                    // inside would otherwise lose its other half). The screen
                    // below still checks every span against every live extent.
                    let mut pages: Vec<u64> = selected.iter().copied().collect();
                    pages.sort_unstable();
                    let mut blockers: Vec<(usize, usize)> = obstacles.clone();
                    blockers.extend(evac_stays.iter().copied());
                    evac_holes = arena
                        .free_blocks_sorted()
                        .into_iter()
                        .map(|(off, sz)| (base + off, base + off + sz))
                        .filter(|&(s, _)| s < low_end)
                        .collect();
                    vacate_candidates = evac_vacate_spans(
                        &pages,
                        base,
                        Self::Z_LOGICAL_PAGE_BYTES,
                        low_end,
                        &blockers,
                        &evac_holes,
                    );
                } else if publish_vacated {
                    let mut pages: Vec<u64> = selected.iter().copied().collect();
                    pages.sort_unstable();
                    for pg in pages {
                        let page_lo = base + pg as usize * Self::Z_LOGICAL_PAGE_BYTES;
                        let lo = page_lo.max(dest);
                        let page_hi = page_lo + Self::Z_LOGICAL_PAGE_BYTES;
                        if page_hi > lo {
                            vacate_candidates.push((lo, page_hi, false));
                        }
                    }
                }
                // No span starts below this, so an extent ending at or below it
                // (every survivor packed under `dest`, and every unselected one
                // beneath the slide) needs no search at all.
                let vacate_floor = vacate_candidates.first().map_or(usize::MAX, |c| c.0);
                let mut stranded = 0usize;
                // Split across `workers` (round 9 wave 7, `zgc7`). Each chunk
                // screens its own COPY of the candidate spans and reports its
                // own ceiling; a span is blocked if any chunk blocked it, and
                // the ceiling is the maximum -- both exactly the serial answers,
                // since "some extent overlaps" and "the highest end" do not
                // depend on the order the extents are met in.
                //
                // THE FUSED ARM (zr z2/r2, `CRATONVM_ZGC_FUSED_SLIDE_SCREEN`,
                // default ON, `=0` is the separate pass below). The screen reads
                // one header per low-region survivor at its post-slide address,
                // and the reference rewrite reads THE SAME header at THE SAME
                // address a moment later (its walkability check), so on this arm
                // the screen rides on the rewrite and the low region is read
                // once instead of twice. That needs every pair before the
                // rewrite, so the large-object end is compacted HERE, before the
                // low cursor drops: the two ends share no arena state but the
                // free-byte total (`compact_low_to` keeps and publishes LOW
                // blocks only, `compact_high_to` HIGH blocks only), and the
                // rewrite stores only into live objects, all of which end at or
                // below the cursor this screen is about to prove. The screen
                // keeps its independence for every survivor with reference
                // slots: the header it reads is still the one at `now`, after
                // the slide, not a size the partition computed. (zs s2/sl2: NOT
                // for a NOREF survivor -- `rewrite_reference_slots_of` sizes
                // those from the pre-slide size cell, possibly a sweep hint,
                // and reads no header; `CRATONVM_ZGC_RELOCATE_SIZE_TABLE=0`
                // restores the post-slide read for them.)
                let screened: Vec<(usize, Vec<(usize, usize, bool)>)> = if fused {
                    // t1 wave 6 lane Y4: a scoped slide's `live` holds no
                    // large-object survivor, so that end must not be compacted.
                    let high = if scope.is_some() {
                        (0, 0)
                    } else {
                        self.compact_high_region(&mut arena, live, &pins, &mut pairs)
                    };
                    high_done = Some(high);
                    phases.mark("high");
                    let fwd_all = relocate::ZForwardIndex::for_slide(&pairs, slide_cycle.as_ref());
                    self.counters.fp.note_slide_buffers(
                        pairs.capacity() * std::mem::size_of::<(usize, usize)>(),
                        moved_to_bytes + zgc_fwd_index_bytes(&fwd_all),
                        0,
                    );
                    let src = ZSourcePages::new(
                        fwd_all.entries(),
                        arena_lo,
                        arena_hi,
                        Self::Z_LOGICAL_PAGE_BYTES,
                    )
                    .with_spans(&fwd_all.cycle_source_spans());
                    let screen = ZSlideScreen {
                        base,
                        low_end,
                        vacate_floor,
                        candidates: &vacate_candidates,
                        // Nothing moved at either end: there is nothing to
                        // rewrite, only the screen to run.
                        rewrite: !pairs.is_empty(),
                    };
                    // zt t31/zc11 (R4): the skip color, before the rewrite.
                    self.remap_in_mark_begin_slide(
                        region_quarantine.is_some() && slide_cycle.is_some(),
                    );
                    let (fwd_ref, src_ref, screen_ref) = (&fwd_all, &src, &screen);
                    let sizes_ref: &[u32] = &sizes;
                    let size_from_cell = zgc_rewrite_size_from_cell_enabled();
                    let parts = self.reloc_par_chunks(live, workers, |chunk: &[usize]| {
                        self.rewrite_reference_slots_of(
                            chunk,
                            sizes_ref
                                .get(zgc_chunk_offset(live, chunk)..)
                                .unwrap_or(&[]),
                            arena_lo,
                            arena_hi,
                            fwd_ref,
                            src_ref,
                            Some(screen_ref),
                            size_from_cell,
                        )
                    });
                    let mut tally = ZRewriteTally::default();
                    let mut out = Vec::with_capacity(parts.len());
                    for (t, part) in parts {
                        tally = tally.add(t);
                        if let Some(part) = part {
                            out.push((part.live_ceiling, part.spans));
                        }
                    }
                    if screen.rewrite {
                        fused_tally = Some(tally);
                    }
                    fused_fwd = Some(fwd_all);
                    phases.mark("rewrite+screen");
                    out
                } else {
                    let moved_to = moved_to
                        .as_ref()
                        .expect("the separate-screen arm always builds its index");
                    let screened = self.reloc_par_chunks(live, workers, |chunk: &[usize]| {
                        let mut spans = vacate_candidates.clone();
                        let mut live_ceiling = base;
                        for &b in chunk {
                            if b < base || b >= low_end {
                                continue;
                            }
                            // Only a survivor on a SELECTED page can be a slide source
                            // (the slide walked `survivors` and nothing else), so the
                            // forwarding search is skipped for the rest -- the answer
                            // for them is `b` either way.
                            let now = if is_selected(page_of(b)) {
                                moved_to.resolve(b)
                            } else {
                                b
                            };
                            // An unsizable header cannot be bounded, so it cannot be
                            // proven dead either. Refuse to reclaim past `low_end` rather
                            // than guess -- the same answer `highest_pinned_end` gives an
                            // extent that runs past the cursor, and for the same reason.
                            // For the span screen the same bound is fail-closed: it
                            // overlaps everything above `now` and refuses every span.
                            let end = match self.survivor_size(now, self.header_ref(now as *mut u8))
                            {
                                Some(sz) => now.saturating_add(sz).min(low_end),
                                None => low_end,
                            };
                            if end > live_ceiling {
                                live_ceiling = end;
                            }
                            if end > vacate_floor {
                                block_overlapping_spans(&mut spans, now, end);
                            }
                        }
                        (live_ceiling, spans)
                    });
                    phases.mark("screen");
                    screened
                };
                let mut live_ceiling = base;
                for (part_ceiling, part_spans) in screened {
                    live_ceiling = live_ceiling.max(part_ceiling);
                    for (span, part) in vacate_candidates.iter_mut().zip(part_spans) {
                        span.2 |= part.2;
                    }
                }
                // zs s3/cr (`CRATONVM_DBG_ZGC_EVAC_BLOCK_SPANS=1`, debug): the
                // POSITIVE CONTROL of the per-object free below. That net runs
                // only where the screen blocks a span `evac_vacate_spans` built
                // (the two disagree), which no healthy heap produces; this
                // makes every evacuated span blocked, so each moved survivor's
                // old extent is freed on its own and `evac=` fifth field counts
                // them. Reclaims less than the default (holes between
                // survivors are not re-published), never more.
                if evacuate && zgc_dbg_evac_block_spans_enabled() {
                    for span in vacate_candidates.iter_mut() {
                        span.2 = true;
                    }
                    self.counters.evac_quarantine_stats[5].fetch_add(1, Ordering::Relaxed);
                }
                // ...and never below the end of a published VM TLAB tail: the
                // span between `new_cursor` and `low_end` is zeroed and handed
                // back to the arena, which is exactly the memory its owner will
                // bump into when it resumes. `zgc/vm_tlab.rs`.
                let tail_floor = self.jit_tlab_skip_floor(base, low_end);
                let proposed = dest.max(highest_pinned_end).max(tail_floor);
                // t1 wave 8 lane ZT3GN: on a scoped young slide, what held the
                // cursor up (`CRATONVM_DBG_ZGC_RELOCATE_PHASES=1` only).
                if scope.is_some() && zgc_dbg_relocate_phases_enabled() {
                    eprintln!(
                        "[zgc-young-evac-cursor] low_end_off={} dest_off={} pinned_end_off={} \
                         tail_floor_off={} live_ceiling_off={} moved={moved} live={}",
                        low_end - base,
                        dest.saturating_sub(base),
                        highest_pinned_end.saturating_sub(base),
                        tail_floor.saturating_sub(base),
                        live_ceiling.saturating_sub(base),
                        live.len(),
                    );
                }
                // zs s2/sl2: an evacuating slide does not lower the cursor (its
                // copies are above the old one); the reclaim is the vacated
                // spans. `live_ceiling` then holds the evacuated survivors'
                // extents clamped at `low_end`, which is not a stranding.
                if !evacuate && live_ceiling > proposed {
                    stranded = live
                        .iter()
                        .filter(|b| **b >= proposed && **b < low_end)
                        .count();
                    tracing::error!(
                        target: "cratonvm::gc::guard",
                        proposed_cursor = proposed - base,
                        raised_to = live_ceiling - base,
                        stranded_live_objects = stranded,
                        low_end = low_end - base,
                        moved,
                        survivors = live.len(),
                        "zgc relocate: the compaction cursor would have been left BELOW live \
                         objects -- their bytes would have been zeroed and re-issued while the \
                         object-start registry still named them. Raising it and reclaiming less."
                    );
                }
                let new_cursor = if evacuate {
                    arena.used_low_for_compaction()
                } else {
                    proposed.max(live_ceiling) - base
                };
                // The bytes this slide actually wrote: it places survivors from
                // `slide_floor` upwards and stops at `dest`. Everything outside
                // that window is where it was before, so a free block there is
                // still free -- see `Arena::compact_low_to` for what dropping them
                // wholesale cost. (zs s2/sl2: an evacuation wrote nothing below
                // the old cursor.)
                let touched = if evacuate {
                    0..0
                } else {
                    slide_floor.saturating_sub(base)..dest.saturating_sub(base)
                };

                // ---- WHAT THE SLIDE EMPTIED, AND WHY IT HAD TO BE SAID ----
                //
                // Reclaim used to be the cursor drop and nothing else, and that
                // is only the whole answer when the cursor can reach `dest`.
                // One survivor on an unselected dense page above the compacted
                // region pins `new_cursor` above it, and then the bytes the
                // slide just emptied are neither below the cursor nor on the
                // free list. **No later sweep can find them either**: the sweep
                // walks the object-start REGISTRY, which this slide is about to
                // rebuild with the survivors' NEW bases, so the old ones stop
                // existing as far as every other subsystem is concerned.
                //
                // Measured on `TestMVStoreTool` at `--Xmx 1g`: four compaction
                // cycles, 885 793 objects relocated, and the largest free block
                // at the failing 262 160-byte request is 8 184 bytes. The
                // slide's own output is page-granular and contiguous -- exactly
                // the shape that request needs -- and it was being discarded.
                //
                // A SELECTED page above `dest` is dead by construction: every
                // survivor based on it was packed below `dest` (a survivor the
                // probe could not place sets `dest` past its own end, so it is
                // below too). What can still be alive up there is an OBSTACLE:
                // an object based on an unselected page whose tail straddles
                // into a selected one, which never moves.
                //
                // Rather than argue that the page partition excludes them, the
                // spans are SCREENED against the live set at its post-slide
                // addresses -- inside the `live_ceiling` pass, one binary
                // search per survivor that reaches above `dest` (round 9 wave 6;
                // it was a separate pass with a prefix maximum until then).
                // That is a check on the answer, and it is the same
                // shape as the `live_ceiling` check above and for the same
                // reason: every argument that the partition is exhaustive is an
                // argument about the partition, not about what is in the span.
                // It can only DROP a span, i.e. reclaim less.
                let mut vacated: Vec<(usize, usize)> = Vec::new();
                // zs s3/cr (`CRATONVM_ZGC_EVAC_QUARANTINE=1`, opt-in): on the
                // evacuating arm the vacated bytes go HERE instead of
                // `vacated` -- zeroed now, withheld from the free list, and
                // released at the next collection's sweep. Stage 2 of the
                // concurrent-relocation plan: a from-span is not reissued
                // during the mutator interval after its evacuation.
                // zt t23/zc7 (R3, `CRATONVM_ZGC_REGION_QUARANTINE`): the
                // region arm's from-space is held by this same machinery.
                let quarantine =
                    evacuate && (zgc_evac_quarantine_enabled() || region_quarantine.is_some());
                let mut withheld: Vec<(usize, usize)> = Vec::new();
                if publish_vacated {
                    let cursor_addr = base + new_cursor;
                    // The screen itself ran in the `live_ceiling` pass above:
                    // a span is `blocked` iff some low-region survivor's extent
                    // at its post-slide address, `[now, end)`, overlaps it.
                    // Test: `zgc::vacated_screen_tests`.
                    for &(lo, page_hi, blocked) in &vacate_candidates {
                        let hi = page_hi.min(cursor_addr);
                        if hi <= lo {
                            continue;
                        }
                        if blocked {
                            // zs s2/sl2: on the evacuating arm the survivors
                            // copied OUT of a blocked span left their old
                            // extents behind, dead, and the registry rebuild is
                            // about to forget them: free each one on its own so
                            // the bytes are not stranded. Sound because live
                            // objects never overlap -- except possibly with an
                            // object whose header cannot be sized, so nothing at
                            // or above the lowest fail-closed extent (an
                            // unsizable survivor's, which runs to `low_end`) is
                            // freed this way: the arm halts at a selected one,
                            // and an unselected one bounds this loop.
                            if evacuate {
                                let unknown_floor = obstacles
                                    .iter()
                                    .chain(evac_stays.iter())
                                    .filter(|&&(_, e)| e >= low_end)
                                    .map(|&(s, _)| s)
                                    .min()
                                    .unwrap_or(usize::MAX);
                                let first = evac_moves.partition_point(|m| m.0 < lo);
                                for &(from, size) in &evac_moves[first..] {
                                    if from >= hi || from + size > unknown_floor {
                                        break;
                                    }
                                    let e = (from + size).min(hi);
                                    if quarantine {
                                        withheld.push((from - base, e - base));
                                    } else {
                                        vacated.push((from - base, e - base));
                                    }
                                    self.counters.slide_evacuate[4].fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            continue; // something live is in there
                        }
                        if quarantine {
                            // The span minus the free holes it absorbed: those
                            // were free before this slide and stay listed.
                            for (s, e) in subtract_sorted_spans(lo, hi, &evac_holes) {
                                withheld.push((s - base, e - base));
                            }
                            continue;
                        }
                        vacated.push((lo - base, hi - base));
                    }
                    let bytes: usize = vacated.iter().map(|(s, e)| e - s).sum();
                    self.counters
                        .vacated_spans_published
                        .fetch_add(vacated.len(), Ordering::Relaxed);
                    self.counters
                        .vacated_bytes_published
                        .fetch_add(bytes, Ordering::Relaxed);
                }
                // WHERE A FORWARDING WORD CAN LIVE. `compact_low_to` zeroes
                // `[new_cursor, cursor)` and the slide packed every survivor
                // BELOW `new_cursor`, so that span is provably empty afterwards
                // -- which makes it the one part of the vacated region where a
                // record can be stamped without writing into a live object. See
                // `stamp_forwarding_words`.
                // t1 wave 8 lane ZT3GN: split the old `cursor` phase into the
                // vacated-span screen above and `compact_low_to` below.
                phases.mark("vacate");
                forwardable_tail = (base + new_cursor)..(base + arena.used_low_for_compaction());
                // t1 wave 10 lane ZT3GN
                // (`CRATONVM_ZGC_GEN_YOUNG_EVACUATE_LOCAL_INSTALL`, default ON
                // inside the opt-in scoped slide): edit only the free blocks
                // reaching the scope floor instead of rebuilding the whole low
                // list; `None` (premise not met) falls back to the old door.
                // `..._DEFER_VACATED_ZERO`: leave the vacated bytes for the
                // hand-out to zero, where the sweep's dead-header skip (the
                // same soundness audit) is on.
                let ye = &self.counters.young_evac;
                let local = scope
                    .filter(|_| ye.local_install.load(Ordering::Relaxed))
                    .and_then(|s| {
                        let zero_vacated = !(ye.defer_vacated_zero.load(Ordering::Relaxed)
                            && sweep::zgc_sweep_skip_dead_headers_enabled()
                            && !zgc_corpse_enabled());
                        let r = arena.compact_nursery_to(
                            new_cursor,
                            s.lo - base,
                            touched.clone(),
                            &vacated,
                            zero_vacated,
                        )?;
                        ye.local_installs.fetch_add(1, Ordering::Relaxed);
                        if !zero_vacated {
                            let bytes: usize = vacated
                                .iter()
                                .map(|&(a, e)| e.min(new_cursor).saturating_sub(a))
                                .sum();
                            ye.deferred_zero_bytes.fetch_add(bytes, Ordering::Relaxed);
                        }
                        Some(r)
                    });
                reclaimed = match local {
                    Some(r) => r,
                    None => arena.compact_low_to(new_cursor, touched, &vacated),
                };
                if !withheld.is_empty() {
                    // Zeroed now (the reason `compact_low_to` zeroes a vacated
                    // span: a corpse header a linear walker could meet), and
                    // recorded for `release_evac_quarantine`.
                    arena.zero_withheld_low_spans(&withheld);
                    let bytes: usize = withheld.iter().map(|(s, e)| e - s).sum();
                    self.counters.evac_quarantine_stats[0]
                        .fetch_add(withheld.len(), Ordering::Relaxed);
                    self.counters.evac_quarantine_stats[1].fetch_add(bytes, Ordering::Relaxed);
                    self.counters.evac_quarantine.lock().extend(withheld);
                }
                phases.mark("cursor");
            }

            // ---- THE LARGE-OBJECT END ---------------------------------
            //
            // Everything above compacts the LOW end only. The arena has two,
            // and the requests that were failing at 97 % free came from the
            // other one -- see `compact_high_region`, which is where the
            // measurement that says so lives. Its pairs join `pairs`, so the
            // rewrite pass, the registry rebuild and the returned
            // `PointerMap` below cover both ends with no second mechanism.
            let (high_moved, high_reclaimed) = match high_done.take() {
                Some(done) => done,
                None if scope.is_some() => (0, 0),
                None => {
                    fail_stop.arm();
                    let done = self.compact_high_region(&mut arena, live, &pins, &mut pairs);
                    phases.mark("high");
                    done
                }
            };
            moved += high_moved;
            reclaimed += high_reclaimed;
            // One batched publish after the slide, not one per object: the
            // record is read by the rewrite pass below, which must see the
            // WHOLE map or it resolves half the graph against a half-built one.
            // CRATONVM_DBG_ZGC_CORPSE -- remember what was at each vacated
            // address. Read AFTER the move, at `to`, where the header now
            // lives. (zs s2/sl2: this said "before `compact_low_to` has zeroed
            // anything at `from`"; `compact_low_to` ran above, and the
            // ledger never reads `from` -- it records the class and size at
            // the destination, keyed by the vacated address.)
            if zgc_corpse_enabled() {
                let cycle = self.counters.corpse_cycle.fetch_add(1, Ordering::Relaxed);
                let mut ledger = self.counters.corpse_ledger.lock();
                for (from, to) in &pairs {
                    let h = self.header_ref(*to as *mut u8);
                    let size = Self::alloc_size(h).unwrap_or(0);
                    ledger.insert(*from, (*to, h.class_id.as_u32(), size, cycle));
                }
                // Bound it. Dropping the OLDEST cycles keeps the recent
                // history, which is where a same-cycle stale read lives; a
                // very old corpse still reports through the `None` arm as
                // "not a recent relocation source", which is the same
                // conclusion the entry would have supported.
                const MAX: usize = 400_000;
                if ledger.len() > MAX {
                    let cutoff = cycle.saturating_sub(8);
                    ledger.retain(|_, (_, _, _, c)| *c >= cutoff);
                }
            }
        }

        {
            let high_copied = self
                .counters
                .high_bytes_copied
                .load(Ordering::Relaxed)
                .saturating_sub(high_copied_before);
            let total = low_moved_bytes + high_copied;
            self.counters.slide_moved_bytes[0].store(total, Ordering::Relaxed);
            self.counters.slide_moved_bytes[1].fetch_add(total, Ordering::Relaxed);
        }
        if moved == 0 {
            // t1 wave 6 handback (Y4): a scoped slide that moves nothing is
            // no longer silent: it books a decline code on
            // `young_evac_refusals` / `young_evac_last_refusal`.
            if scope.is_some() {
                self.counters.young_evac.note_refusal(if scoped_nothing_selected {
                    young_evacuate::YE_DECLINED_NOTHING_SELECTED
                } else {
                    young_evacuate::YE_DECLINED_NOTHING_MOVED
                });
            }
            self.remap_in_mark_end_slide();
            phases.print(live.len(), 0, fused);
            return (0, reclaimed, cratonvm_types::PointerMap::default());
        }

        // ---- Rewrite every reference slot in every surviving object -------
        //
        // AFTER the whole slide, not during it: a slot in an already-moved
        // object may point at an object that has not moved yet, so rewriting
        // as we go resolves half the graph against a half-built map.
        // ONE table for the three readers that used to have three: this, the
        // rewrite loop below, and the walkability/verify pair. Built once, sorted,
        // answered by a range check and a binary search -- see `ZForwardIndex`.
        let fwd = fused_fwd
            .take()
            .unwrap_or_else(|| relocate::ZForwardIndex::for_slide(&pairs, slide_cycle.as_ref()));
        // WHICH LOGICAL PAGES HOLD A SLIDE SOURCE (round 9 wave 6, `zgc6`).
        // The rewrite below asks `fwd` about every non-null reference slot of
        // every live object -- a binary search each, over sources that sit on a
        // handful of selected pages (plus the large-object end). A per-page
        // bit answers "cannot be a source" for the rest with one load, so the
        // search runs only where an answer can be `Some`. Exact: every source
        // marks its page, and if any source were outside the arena the filter
        // stands down entirely rather than guess.
        //
        // A concrete type since zr z2/r2 (`ZSourcePages`), not a `&dyn Fn`: the
        // rewrite asks it once per non-null slot, and through a trait object
        // that was an indirect call per slot.
        // zs s2/sl2: on the fused arm the rewrite already ran (with its own
        // filter over both ends), so this second filter is read only by the
        // debug verifier -- or kept by `CRATONVM_ZGC_SLIDE_BUFFER_TRIM=0`.
        // Built only where one of them will read it (a pass over every pair
        // per slide otherwise); the empty filter is never consulted.
        // t1 wave 6 lane Y4: a scoped slide's extra rewrite below reads it.
        let src_needed = fused_tally.is_none()
            || !slide_trim
            || zgc_verify_slide_enabled()
            || scope.is_some();
        let src = if src_needed {
            ZSourcePages::new(
                fwd.entries(),
                arena_lo,
                arena_hi,
                Self::Z_LOGICAL_PAGE_BYTES,
            )
            .with_spans(&fwd.cycle_source_spans())
        } else {
            ZSourcePages::new(&[], arena_lo, arena_lo, Self::Z_LOGICAL_PAGE_BYTES)
        };
        // Everything the rewrite is about to walk must still LOOK like the
        // object the slide thought it was moving.
        //
        // Each survivor is walked at `fwd.resolve(b)` — it keeps the ORIGINAL
        // address for any live object the relocation record does not list.
        // That is correct only while an unlisted object is one that genuinely
        // did not move AND whose memory nothing wrote over. If either half
        // fails, this loop hands `reference_slots` an address whose contents
        // are now some other object, and the walk strides a length that was
        // never this object's.
        //
        // Screening here rather than trusting the header is what turns a
        // SIGSEGV INSIDE THE COLLECTOR — whose stack names only the collector,
        // so the crash site is worthless as evidence — into a list of
        // offenders with the one fact that discriminates: whether this base is
        // an address the slide just vacated. `fwd.contains_from(base)` says
        // the object was overwritten; `!contains` says the header was already
        // wrong before the slide, which is a different bug entirely.
        //
        // Unconditional, not behind a debug flag: the cost is one registry
        // probe and one `alloc_size` per survivor, against a walk of every one
        // of its slots, and the failure it prevents is memory corruption.
        //
        // ---- SPLIT ACROSS WORKERS (round 9 wave 7, `zgc7`) -------------------
        //
        // The rewrite is the largest of the whole-live-set passes (~45 % of
        // this function on `ZgcTlabStress 8 200`), and it is a pure function
        // of each survivor: read its own slots, look each word up in the
        // read-only `fwd`, store into its own slots. Chunks of the live set
        // are disjoint objects, so they are disjoint slot sets -- no two
        // workers ever store to one word, and the store was already a relaxed
        // atomic (see `rewrite_reference_slots_of`). Each slot is still
        // rewritten EXACTLY ONCE, which matters: a slide's destinations are
        // often other objects' sources, so a second pass over a slot would
        // chase it one hop too far.
        //
        // The post-slide addresses (`live_now`) are no longer materialised
        // for this pass -- each worker resolves as it goes -- only for the
        // debug verifier below, which is the one other reader.
        //
        // On the fused arm (zr z2/r2) this pass already ran, inside the
        // `live_ceiling` screen, over every pair of both ends.
        let rewrite_workers = self.relocate_workers_for(live.len());
        let rewrite_tally = match fused_tally.take() {
            Some(tally) => tally,
            None => {
                // zt t31/zc11 (R4): the skip color, before the rewrite.
                self.remap_in_mark_begin_slide(region_quarantine.is_some() && slide_cycle.is_some());
                let (fwd_ref, src_ref) = (&fwd, &src);
                let sizes_ref: &[u32] = &sizes;
                let size_from_cell = zgc_rewrite_size_from_cell_enabled();
                let tally = self
                    .reloc_par_chunks(live, rewrite_workers, |chunk: &[usize]| {
                        self.rewrite_reference_slots_of(
                            chunk,
                            sizes_ref
                                .get(zgc_chunk_offset(live, chunk)..)
                                .unwrap_or(&[]),
                            arena_lo,
                            arena_hi,
                            fwd_ref,
                            src_ref,
                            None,
                            size_from_cell,
                        )
                        .0
                    })
                    .into_iter()
                    .fold(ZRewriteTally::default(), |a, b| a.add(b));
                phases.mark("rewrite");
                tally
            }
        };
        // t1 wave 6 lane Y4: a scoped slide's referrers OUTSIDE its live list
        // -- young survivors elsewhere and carded objects (`scope.extra`,
        // disjoint from `live`, so every slot is still rewritten exactly once).
        // None of them moved (they are not on a selected page), so each is
        // walked at its own address.
        let rewrite_tally = match scope {
            Some(s) if !s.extra.is_empty() => {
                let (fwd_ref, src_ref) = (&fwd, &src);
                let extra_workers = self.relocate_workers_for(s.extra.len());
                let extra_tally = self
                    .reloc_par_chunks(&s.extra, extra_workers, |chunk: &[usize]| {
                        self.rewrite_reference_slots_of(
                            chunk, &[], arena_lo, arena_hi, fwd_ref, src_ref, None, false,
                        )
                        .0
                    })
                    .into_iter()
                    .fold(ZRewriteTally::default(), |a, b| a.add(b));
                self.counters.young_evac.note_extra_rewritten(s.extra.len());
                phases.mark("rewrite-extra");
                rewrite_tally.add(extra_tally)
            }
            _ => rewrite_tally,
        };
        let unwalkable = rewrite_tally.unwalkable;
        self.counters
            .rewrite_primitive_cells_skipped
            .fetch_add(rewrite_tally.primitive_cells_skipped, Ordering::Relaxed);
        self.counters
            .relocate_size_table_skips
            .fetch_add(rewrite_tally.table_skipped, Ordering::Relaxed);
        // `CRATONVM_DBG_ZGC_REWRITE` (zr z1/rl): one line per slide, the
        // engagement figures of the pass that dominates its cost.
        if zgc_dbg_rewrite_enabled() {
            eprintln!(
                "[zgc-rewrite] survivors={} workers={} pairs={} slots_rewritten={} primitive_cells_skipped={} unwalkable={} slots_walked={} fused={} table_skipped={} census_hinted={} cell_sized={}",
                live.len(),
                rewrite_workers,
                pairs.len(),
                rewrite_tally.rewritten,
                rewrite_tally.primitive_cells_skipped,
                unwalkable,
                rewrite_tally.slots_walked,
                fused,
                rewrite_tally.table_skipped,
                self.counters.sweep_size_hint_hits.load(Ordering::Relaxed),
                rewrite_tally.cell_sized,
            );
        }
        // zr z4/un (`CRATONVM_ZGC_SLIDE_BUFFER_TRIM`): the forward index and
        // the source-page filter have no reader past the rewrite but the
        // debug verifier. Dropped here rather than at the end of the function,
        // so they are not alive beside the forwarding table's growth
        // (`publish_relocations`) and the cycle's `PointerMap` below.
        let fwd_bytes = zgc_fwd_index_bytes(&fwd);
        self.counters.fp.note_slide_buffers(
            pairs.capacity() * std::mem::size_of::<(usize, usize)>(),
            fwd_bytes,
            0,
        );
        let verify_tables = (!slide_trim || zgc_verify_slide_enabled()).then_some((fwd, src));
        let kept_index_bytes = if verify_tables.is_some() {
            fwd_bytes
        } else {
            0
        };

        // ---- Rebuild the object-start registry ----------------------------
        //
        // zs s2/sl2: TWO passes, every `from` removed before any `to` is
        // inserted. The single interleaved pass (`remove(from); insert(to)`
        // per pair) was correct only because of the pair ORDER: a `to` can be
        // another pair's `from` (an object slides into a predecessor's old
        // base), and that is safe only when the pair owning the `from` is
        // processed first -- true for the low end (ascending) and the high end
        // (descending), asserted nowhere, and wrong the day a parallel slide
        // or the page-evac arm appends pairs in another order (the later
        // `remove` would erase the live base just inserted). Two passes give
        // the same set for every order. Same work: one remove and one insert
        // per pair.
        //
        // zt t23/zc7: with a region cycle, its installs are re-registered
        // from the cycle's own tables (the same set as `pairs`' first
        // `slide_cycle_pairs`, which the cycle vouched for pair by pair), and
        // only the rest from `pairs`. Order-free, as above.
        let cycle_part = slide_cycle.as_deref();
        let rest_from = if cycle_part.is_some() {
            debug_assert!(pairs[..slide_cycle_pairs]
                .iter()
                .all(|&(f, t)| cycle_part.is_some_and(|c| c.forwarded(f) == Some(t))));
            slide_cycle_pairs
        } else {
            0
        };
        if let Some(c) = cycle_part {
            c.for_each_pair(|from, _| self.registry.remove(from));
        }
        for (from, _) in &pairs[rest_from..] {
            self.registry.remove(*from);
        }
        if let Some(c) = cycle_part {
            c.for_each_pair(|_, to| self.registry.insert(to));
        }
        for (_, to) in &pairs[rest_from..] {
            // No audit here: the registry is mid-rebuild, so it still holds
            // every survivor's PRE-slide base while `to` is a post-slide one.
            // Every probe would report a conflict against an address the next
            // iterations are about to remove. The post-slide extent census,
            // which runs once the rebuild is complete, is the check for this
            // site -- and it comes back clean, which is why the two mutator
            // sites above are the ones instrumented.
            self.registry.insert(*to);
        }

        // A REAL FORWARDING WORD, wherever one fits. See
        // `stamp_forwarding_words`; the table below is what covers the rest.
        // (zt t23/zc7: on the evacuating arm, the region cycle's included, the
        // stampable tail is empty -- the cursor does not drop -- so this
        // stamps nothing there.)
        self.stamp_forwarding_words(&pairs, &forwardable_tail);

        // zt t23/zc7 (R3, `CRATONVM_ZGC_REGION_QUARANTINE`): the region cycle
        // outlives this pause. It answers `forwarded_after_slide` for the
        // mutator interval (its from-space is quarantined above, so no vacated
        // base is re-issued before the release), so the pairs it vouches for
        // are NOT published into `relocations` below; the large-object end's
        // still are. Kept BEFORE the publish: a cycle it replaces is handed
        // over under an older epoch than this slide's pairs. Released at the
        // next mark end (`release_evac_quarantine`).
        let mut published_from = 0usize;
        if let (Some(mode), Some(cycle)) = (region_quarantine, slide_cycle.as_ref()) {
            if slide_cycle_pairs > 0 {
                self.keep_region_cycle(std::sync::Arc::clone(cycle), mode, slide_cycle_pairs);
                published_from = slide_cycle_pairs;
            }
        }
        // zt t31/zc11 (R4): the rewrite is done; a slot it skipped is answered
        // by the cycle just kept.
        self.remap_in_mark_end_slide();

        // Publish this slide's moves so the forwarding barrier has something to
        // read. See `ZgcRealHeap::relocations`.
        self.publish_relocations(&pairs[published_from..]);
        // `CRATONVM_DBG_VACATED_FRAMES`: the same pairs with the class each
        // object has at its DESTINATION, kept for the whole run so a use of the
        // vacated address can still be judged after the allocator has handed
        // the space out again. See `gc_quiescence::MOVED_HISTORY`.
        if cratonvm_gc_core::gc_quiescence::vacated_frames_enabled() {
            let classed: Vec<(usize, usize, u32)> = pairs
                .iter()
                .map(|(from, to)| {
                    (
                        *from,
                        *to,
                        self.header_ref(*to as *mut u8).class_id.as_u32(),
                    )
                })
                .collect();
            cratonvm_gc_core::gc_quiescence::record_moved_history(&classed);
        }

        if unwalkable > 0 {
            tracing::error!(
                target: "cratonvm::gc::guard",
                unwalkable,
                survivors = live.len(),
                "zgc relocate: {unwalkable} of {} survivor(s) could not be walked by the \
                 rewrite pass and were SKIPPED -- any reference they hold to a moved \
                 object is now dangling. See the per-object lines above for whether the \
                 slide vacated their address.",
                live.len(),
            );
        }
        // The same survey the sweep runs, asked here instead: the registry has
        // just been rebuilt from this slide's `from -> to` pairs and no mutator
        // has resumed, so an overlap reported HERE was created by this slide.
        if zgc_corpse_enabled() {
            let after: Vec<usize> = self.registry.snapshot().bases();
            self.survey_registry_extents(&after, "post-slide");
            // The state the mutators are about to be handed, so the next
            // cycle's survey can say what changed rather than what is wrong.
            let mut sizes = self.counters.slide_exit_sizes.lock();
            sizes.clear();
            sizes.reserve(after.len());
            for &b in &after {
                let sz = Self::alloc_size(self.header_ref(b as *mut u8)).unwrap_or(0);
                sizes.insert(b, sz);
            }
        }
        // The debug verifier is the one reader left of the post-slide
        // addresses, so they are materialised only when it will run (it
        // returns straight away otherwise).
        if zgc_verify_slide_enabled() {
            if let Some((fwd, src)) = verify_tables.as_ref() {
                // t1 wave 6 lane Y4: a scoped slide is verified over the WHOLE
                // post-move registry (already rebuilt above), so an old holder
                // the cards did not name shows as a missed rewrite.
                let live_now: Vec<usize> = if scope.is_some() {
                    self.registry.snapshot().bases()
                } else {
                    live.iter()
                        .map(|b| {
                            if src.may_be_source(*b) {
                                fwd.resolve(*b)
                            } else {
                                *b
                            }
                        })
                        .collect()
                };
                self.verify_no_dangling_slots_after_slide(&live_now, arena_lo, arena_hi, fwd);
            }
        }
        drop(verify_tables);

        // The map is the cycle OUTPUT (monitors, the reference processor, the
        // VM.s root write-back). Built from the pairs directly rather than
        // cloned out of a second table nothing else reads any more.
        //
        // zr z4/un (`CRATONVM_ZGC_SLIDE_BUFFER_TRIM`): sized once (the shards
        // otherwise grow by doubling, each growth holding the old table beside
        // the new one) and filled by draining `pairs`, which is freed before
        // the map leaves this function.
        let pair_bytes = pairs.capacity() * std::mem::size_of::<(usize, usize)>();
        // zt t1/rl (`zgc-zr4un-proposal-pointer-map-as-a-sorted-slice`): hand the pairs
        // themselves over as the map's sorted form -- no hash insert per move
        // and 16 bytes a move instead of a second table. The consumers only
        // look up; a mutation converts the map back (`PointerMap::thaw`).
        let pointer_map = cratonvm_types::PointerMap::from_pairs_sorted(pairs);
        let map_bytes = pointer_map.heap_bytes();
        self.counters
            .fp
            .note_slide_buffers(pair_bytes, kept_index_bytes, map_bytes);
        phases.mark("publish");
        phases.print(live.len(), moved, fused);
        (moved, reclaimed, pointer_map)
    }

    /// The reference-rewrite pass of [`Self::relocate_stw_admitted`] over one
    /// chunk of the PRE-slide live set: resolve each base through `fwd`, screen
    /// it with [`Self::rewrite_target_walkable_sized`], then point every reference
    /// slot that names a slide source at its destination. Returns the chunk's
    /// [`ZRewriteTally`] (survivors skipped as unwalkable among its counts) and,
    /// with `screen`, the chunk's part of the screen (zs s2/sl2: this said
    /// "returns how many survivors were skipped", a count it stopped being).
    ///
    /// Split out of `relocate_stw_admitted` (round 9 wave 7, `zgc7`) so the
    /// serial arm and the parallel arm run this one body; see
    /// `zgc_par_chunks`. Must be called at most once per survivor per slide:
    /// a rewritten slot names a destination, which may itself be another
    /// object's source.
    ///
    /// `screen` (zr z2/r2, the fused arm): also run the low slide's
    /// `live_ceiling` / vacated-span screen over the chunk, from the header the
    /// walkability check reads anyway, and return the chunk's part of it. With
    /// `screen.rewrite == false` only the screen runs. `src` is the per-page
    /// source filter, a concrete type so the per-slot test inlines.
    pub fn rewrite_reference_slots_of(
        &self,
        live: &[usize],
        cells: &[u32],
        arena_lo: usize,
        arena_hi: usize,
        fwd: &relocate::ZForwardIndex,
        src: &ZSourcePages,
        screen: Option<&ZSlideScreen<'_>>,
        size_from_cell: bool,
    ) -> (ZRewriteTally, Option<ZScreenPart>) {
        let mut unwalkable = 0usize;
        let mut primitive_cells_skipped = 0usize;
        let mut rewritten = 0usize;
        let mut slots_walked = 0usize;
        let mut table_skipped = 0usize;
        let reference_cells_only = self.counters.local.rewrite_reference_cells_only();
        // zs s2/sl2: `CRATONVM_ZGC_REWRITE_SIZE_FROM_CELL`. zs s3/cr: read
        // ONCE by the caller, on the collecting thread, and passed in. It was
        // read here, per chunk, i.e. on the worker threads, where a test's
        // per-thread flag override does not reach: a parallel rewrite under
        // the opt-in sized only the caller's own chunks from the cell.
        // Production reads the process environment either way.
        let mut cell_sized = 0usize;
        let rewrite = screen.is_none_or(|s| s.rewrite);
        let mut part = screen.map(|s| ZScreenPart {
            live_ceiling: s.base,
            spans: s.candidates.to_vec(),
        });
        // One buffer for the chunk, cleared per survivor: the old per-object
        // `Vec::new()` allocated on every survivor that had a slot to rewrite.
        let mut rewrites: Vec<(u64, u64)> = Vec::new();
        // zt t20/zb3: on a colored-store heap the rewrite writes a bare-word
        // slot (compact field, array element) as the good-colored word, the
        // encoding a mutator store writes there; a legacy cell stays plain
        // (see `set_field_no_card`). Read once per chunk; off it is one load.
        let colored = self.load_barrier_colored_stores();
        let mut gc_colored = 0usize;
        // zt t31/zc11 (R4, `remap_in_mark`): this slide's skip color (`0`:
        // rewrite every slot). Read once per chunk.
        let rim_skip = if colored { self.counters.rim.skip_color() } else { 0 };
        let mut rim_skipped = 0usize;
        for (i, &b) in live.iter().enumerate() {
            // zr z7/pc (`CRATONVM_ZGC_RELOCATE_SIZE_TABLE`): a survivor whose
            // pre-slide cell says "no reference slots" has nothing to rewrite
            // and a size that moved with it, so it is served without reading
            // its header: skipped outright when no screen covers it, and only
            // its extent fed to the screen otherwise. Every other shape keeps
            // the header read below -- including the walkability screen, which
            // must see the POST-slide header of any object whose slots are
            // about to be walked.
            let cell = cells.get(i).copied().unwrap_or(0);
            let noref = cell & ZSIZE_CELL_NOREF != 0;
            if noref && !screen.is_some_and(|s| b >= s.base && b < s.low_end) {
                table_skipped += 1;
                continue;
            }
            let obj = if src.may_be_source(b) {
                fwd.resolve(b)
            } else {
                b
            };
            // ONE header read serves the walkability check and the screen.
            // Off-arena survivors are not checked (see
            // `rewrite_target_walkable_sized`), and never reach the screen.
            let in_arena = obj >= arena_lo && obj < arena_hi;
            let size = if noref {
                Some((cell & !ZSIZE_CELL_NOREF) as usize)
            } else if in_arena && size_from_cell && cell != 0 {
                // zs s2/sl2 (`CRATONVM_ZGC_REWRITE_SIZE_FROM_CELL=1`, opt-in):
                // the pre-slide size cell of an object WITH reference slots,
                // instead of `survivor_size` on the post-slide header (an
                // `alloc_size` each: a class-layout lookup for an instance).
                // `reference_slots_with` below still decodes the header to
                // walk it. What the opt-in gives up is the walkability
                // screen's independent read: a header torn between the census
                // and here is no longer sized from itself.
                cell_sized += 1;
                Some(cell as usize)
            } else if in_arena {
                // zs s1/dg: `survivor_size`, so the fused screen bounds a real
                // array past 1 << 28 elements by its extent rather than
                // fail-closed at `low_end`, and the walkability check below
                // admits it.
                self.survivor_size(obj, self.header_ref(obj as *mut u8))
            } else {
                None
            };
            if let (Some(s), Some(p)) = (screen, part.as_mut()) {
                if b >= s.base && b < s.low_end {
                    // Exactly the separate pass's rule: an unsizable header is
                    // bounded by `low_end`, fail-closed.
                    let end = match size {
                        Some(sz) => obj.saturating_add(sz).min(s.low_end),
                        None => s.low_end,
                    };
                    if end > p.live_ceiling {
                        p.live_ceiling = end;
                    }
                    if end > s.vacate_floor {
                        block_overlapping_spans(&mut p.spans, obj, end);
                    }
                }
            }
            if !rewrite {
                continue;
            }
            if noref {
                table_skipped += 1;
                continue;
            }
            if in_arena && !self.rewrite_target_walkable_sized(obj, size, arena_hi, fwd) {
                unwalkable += 1;
                continue;
            }
            rewrites.clear();
            {
                self.reference_slots_with(obj as u64, &mut |slot: census::ZSlotObservation| {
                    let raw = slot.raw_word as usize;
                    if raw == 0 {
                        return;
                    }
                    slots_walked += 1;
                    if !src.may_be_source(raw) {
                        return;
                    }
                    if let Some(to) = fwd.get(raw) {
                        // A PRIMITIVE CELL IS NOT A REFERENCE (zr z1/rl).
                        // `reference_slots` reports EVERY legacy 16-byte cell,
                        // tag and all (the census counts the primitive ones),
                        // and this pass used to rewrite any word equal to a
                        // moved base -- so a Java `long` / `double` field
                        // whose bits happened to equal a vacated address was
                        // silently changed by a collection. The marker traces
                        // a legacy cell only under `VTAG_OBJECT`
                        // (`visit_strong_refs_at`), so this is the same rule
                        // on both sides. `CRATONVM_ZGC_REWRITE_REFERENCE_CELLS_ONLY=0`
                        // restores the old rewrite.
                        if reference_cells_only && !slot.is_reference_slot() {
                            primitive_cells_skipped += 1;
                            return;
                        }
                        // zt t31/zc11 (R4): a colored slot that still carries
                        // the closing mark's color and names an object the
                        // kept cycle moved is left for the load barrier or the
                        // next mark to heal (`remap_in_mark`).
                        if rim_skip != 0
                            && slot.legacy_tag.is_none()
                            && Self::remap_in_mark_skips(slot.slot_addr, rim_skip, raw, fwd)
                        {
                            rim_skipped += 1;
                            return;
                        }
                        // zt t20/zb3: `raw` was decoded by `read_ref_slot`
                        // (a colored slot reads as its address), so the
                        // forwarding lookup above saw an address either way.
                        let word = if colored && slot.legacy_tag.is_none() {
                            let w = self.gc_colored_word(to);
                            gc_colored += usize::from(vaddr::is_colored_word(w));
                            w
                        } else {
                            to as u64
                        };
                        rewrites.push((slot.slot_addr, word));
                    }
                });
            }
            rewritten += rewrites.len();
            for &(slot_addr, to) in &rewrites {
                // A RELAXED ATOMIC store, not `std::ptr::write`.
                //
                // WHY THE OLD JUSTIFICATION HAD TO GO. This line's previous
                // SAFETY note ended "and the world is stopped". That is true
                // today and it is scheduled to stop being true: it is exactly
                // the property step 7 of the ordered sequence on
                // [`census::ZSlotShape::word_is_atomically_accessed_today`]
                // removes, when flipping `RELOCATION_REQUESTED` makes
                // relocation concurrent. A justification that expires on the
                // very change it has to survive is worse than none, because
                // whoever makes that change has no reason to come back here.
                //
                // THE NEW JUSTIFICATION. This is the word the ZGC load barrier
                // SELF-HEALS: `zgc/barrier.rs`'s `load_barrier_slow` does
                // `slot.compare_exchange(observed, healed, AcqRel, Acquire)`
                // on an `&AtomicU64` view of this same address --
                // [`ZgcRealHeap::load_barrier_slot`] takes that view of a
                // `slot_addr` produced by this same `reference_slots` walk. A
                // PLAIN write racing an ATOMIC read-modify-write on one
                // location is a data race, and a data race is undefined
                // behaviour in the Rust abstract machine whatever x86-64 does
                // with an aligned qword. The hazard is not the instruction the
                // backend emits: it is that the compiler is entitled to treat
                // a plain access as unshared, and may therefore duplicate,
                // widen, sink, hoist or invent it. `Relaxed` withdraws that
                // entitlement and buys nothing else, which is the whole of
                // what a CAS neighbour needs. Same argument and same spelling
                // as the atomicity note over
                // `cratonvm_types::narrow_oop::read_ref_slot` /
                // `write_ref_slot` (`types/src/narrow_oop.rs:353`, `:370`) --
                // deliberately not a second style.
                //
                // NOT STRONGER THAN `Relaxed`, for that note's four reasons in
                // brief: the defect is the non-atomicity and not the ordering;
                // the release/acquire edge belongs to the heal's own CAS and a
                // slot writer participates in neither half of it; the
                // Java-visible publication edges come from
                // `set_field_volatile`'s bracketing `SeqCst` fences and from
                // `write_compact_field`'s caller-supplied `Ordering`, never
                // from a raw slot writer; and on x86-64 a `Relaxed` store of an
                // aligned qword lowers to the same single `mov` the plain write
                // did (`str` on aarch64), so this cannot move a benchmark and
                // is safe to land AHEAD of the arming rather than with it.
                //
                // WIDTH -- 8 bytes unconditionally is correct HERE and only
                // here. `reference_slots` reads compact fields and array
                // elements through `narrow_oop::read_ref_slot`, which is FOUR
                // bytes wide under compressed oops, so an 8-byte store would
                // be four bytes of the neighbouring field. It cannot arise
                // inside a `ZgcRealHeap`: ZGC and compressed oops are mutually
                // exclusive, and the exclusion is enforced rather than
                // aspirational. `vm/src/vm/vm_init.rs` refuses
                // `-XX:+UseCompressedOops` for any backend but `Generational`
                // and prints the refusal on stderr, and `zgc/vaddr.rs`'s
                // "Interaction with compressed oops" section gives the reason
                // it must STAY refused: a coloured word is `Z_COLORED_TAG` at
                // bit 63, a colour at bits 42-46 and a 42-bit offset, which
                // does not fit in 32 bits under any encoding -- the same fact
                // that makes `VmHeap::load_ref_slot_barriered` refuse the
                // narrow arm outright. So `narrow_oops_enabled()` is false for
                // every slot this walk reports, and `to` is a full machine
                // address that could not be narrowed anyway. A narrow arm here
                // would be unreachable code asserting the opposite of the
                // invariant, which is why there is not one.
                //
                // SAFETY: `slot_addr` is the address of a reference WORD inside
                // a live object, as reported by `reference_slots`, whose own
                // contract ([`census::ZSlotObservation::slot_addr`]) is
                // `slot_addr % 8 == 0` for every shape it emits; the object was
                // screened by `rewrite_target_walkable_sized` a few lines above.
                // Natural alignment is the one thing a relaxed atomic requires
                // that a plain access does not -- and `ptr::write` required it
                // too, so nothing that was sound becomes unsound.
                //
                // zt t20/zb3: `to` is the WORD to store -- the destination
                // address, or on a colored-store heap its good-colored word
                // for a bare-word slot (built in the walk above).
                unsafe { (&*(slot_addr as *const AtomicU64)).store(to, Ordering::Relaxed) };
            }
        }
        if gc_colored != 0 && zgc_load_barrier_census_level() != 0 {
            self.counters
                .lb_gc_colored
                .fetch_add(gc_colored, Ordering::Relaxed);
        }
        self.counters.rim.note_skipped(rim_skipped);
        (
            ZRewriteTally {
                unwalkable,
                rewritten,
                primitive_cells_skipped,
                slots_walked,
                table_skipped,
                cell_sized,
            },
            part,
        )
    }

    /// Check a registry insertion against what the registry already holds.
    ///
    /// `addr` is the base about to be registered and `size` the bytes reserved
    /// for it (0 when the caller does not know, which disables the second
    /// check). Two distinct failures look identical by the time the extent
    /// census meets them a cycle later, and this separates them:
    ///
    /// * **interior insert** -- `addr` is already inside a registered object.
    ///   Someone is registering an interior address, most likely a reference
    ///   slot, as though it were an allocation.
    /// * **stale entry swallowed** -- a registered base already sits inside
    ///   `[addr, addr + size)`. The allocation is innocent; an OLD entry was
    ///   never removed, its memory was recycled, and the fresh object now
    ///   contains a base the registry still believes in.
    ///
    /// The second is the one to expect if the sweep's `registry.remove` or the
    /// slide's rebuild can miss an address, and it is invisible to any check
    /// that only asks "am I inside somebody".
    ///
    /// Diagnostic only, behind `CRATONVM_DBG_ZGC_CORPSE`, capped. It never
    /// vetoes: a refusal here would drop a live object's base and turn a
    /// bookkeeping bug into a use-after-free.
    pub fn audit_registry_insert(&self, addr: usize, size: usize, site: &'static str) {
        if !zgc_corpse_enabled() {
            return;
        }
        // (0) IS THIS ADDRESS ALREADY A REGISTERED BASE?
        //
        // The strongest thing the allocator can get wrong, and the one neither
        // check below can see: both are strict about `addr`, so an address
        // issued twice slips between them. A set bit here means the arena has
        // handed out memory that the registry still believes holds a live
        // object, and the caller is about to write a header over it.
        if self.registry.contains(addr) {
            let h = self.header_ref(addr as *mut u8);
            let sz = Self::alloc_size(h).unwrap_or(0);
            self.report_insert_conflict("double-issue", site, addr, size, addr, sz);
        }
        // (a) Is `addr` inside an object that is already registered?
        if let Some(prev) = self
            .registry
            .nearest_base_at_or_below(addr.saturating_sub(1))
        {
            if prev < addr {
                let ph = self.header_ref(prev as *mut u8);
                if let Some(psz) = Self::alloc_size(ph) {
                    if prev + psz > addr {
                        self.report_insert_conflict("interior-insert", site, addr, size, prev, psz);
                    }
                }
            }
        }
        // (b) Is a registered base already inside the span this object will
        //     occupy? `nearest_base_at_or_below(end - 1)` is the greatest such
        //     base, so one probe answers it.
        if size > 1 {
            if let Some(inside) = self.registry.nearest_base_at_or_below(addr + size - 1) {
                if inside > addr {
                    let ih = self.header_ref(inside as *mut u8);
                    let isz = Self::alloc_size(ih).unwrap_or(0);
                    self.report_insert_conflict(
                        "stale-entry-swallowed",
                        site,
                        addr,
                        size,
                        inside,
                        isz,
                    );
                }
            }
        }
    }

    /// One line per registry-insert conflict, capped so a storm cannot itself
    /// become the hang. Carries the raw words at the conflicting base, because
    /// an implausible header field here is usually half an arena pointer.
    pub fn report_insert_conflict(
        &self,
        what: &'static str,
        site: &'static str,
        addr: usize,
        size: usize,
        other: usize,
        other_size: usize,
    ) {
        let n = self.counters.corpse_reports.fetch_add(1, Ordering::Relaxed);
        if n >= 24 {
            return;
        }
        let oh = self.header_ref(other as *mut u8);
        // SAFETY: `other` is a registered base inside the arena.
        let w: [u64; 2] = unsafe { std::ptr::read_unaligned(other as *const [u64; 2]) };
        tracing::error!(
            target: "cratonvm::gc::guard",
            what,
            site,
            addr,
            size,
            other,
            other_size,
            other_ends_at = other + other_size,
            delta = addr.abs_diff(other),
            other_kind = ?oh.kind(),
            other_class_id = oh.class_id.as_u32(),
            other_num_slots = oh.num_slots(),
            other_w0 = format!("{:#018x}", w[0]),
            other_w1 = format!("{:#018x}", w[1]),
            "zgc registry insert: this insertion and an entry the registry already \
             holds describe overlapping memory"
        );
    }

    /// Survey the object-start registry for extents that run into the next
    /// registered base, and report the first few.
    ///
    /// Every registered object's extent must end at or below the next
    /// registered base. Two things depend on it: the sweep zeroes
    /// `alloc_size(header)` bytes from a dead object's base and hands the same
    /// span to `Arena::add_free_block`, and the slide memmoves the same count.
    /// An extent that is too long therefore destroys the NEXT object and hands
    /// its address back to the allocator while the registry still names it --
    /// which is the reopened `zgc-rewrite-pass-walks-off-a-reference-array`
    /// failure with no unsizable header needed anywhere in the story.
    ///
    /// `where_` names the call site so the two surveys can be told apart in a
    /// log: `"pre-sweep"` inherits whatever the last cycle left, `"post-slide"`
    /// is the same question asked before the mutators resume.
    ///
    /// Returns the number of overlaps. Diagnostic only -- it never vetoes, and
    /// it is called only under `CRATONVM_DBG_ZGC_CORPSE`.
    /// Decode the object that would sit `delta` bytes below `victim`, and say
    /// whether it explains the victim.
    ///
    /// # The question this answers
    ///
    /// The extent census already prints the raw words below an offending base.
    /// It never said what they MEAN, and the fifth pass of
    /// `zgc-rewrite-pass-walks-off-a-reference-array-20260815.md` decoded them
    /// once, by hand, and found "a plausible header for an object the registry
    /// does not contain" at `victim - 16`.
    ///
    /// That distinction reframes the defect. The corrupting value is a MANAGED
    /// HEAP POINTER at offset 0 of a live object, and an ordinary reference
    /// store into `O` at index `i` writes at `O + 16 + 16i` — so for one to land
    /// on offset 0 of the victim, the writer's base must sit exactly 16 bytes
    /// below it. Two readings fit the same bytes:
    ///
    /// * **a stray writer** — nothing plausible sits below the victim, and
    ///   something wrote through a pointer it should not have had;
    /// * **an overlapping allocation** — a plausible, UNREGISTERED object at
    ///   `victim - 16` whose extent covers the victim, in which case the
    ///   "corruption" is just its field 0 and no writer is misbehaving at all.
    ///
    /// The second explains the most puzzling number on that page: `zgc access
    /// audit` reads ZERO on runs that overlap. Every audit there asks "is the
    /// RECEIVER a registered base?", and under the second reading the store is
    /// an ordinary store into whatever the writer believes it owns.
    ///
    /// Returns `(plausible, registered, covers_victim, class_id, size)`.
    /// `covers_victim` is the load-bearing one: a plausible unregistered object
    /// whose extent reaches the victim is the second reading, confirmed.
    pub fn decode_neighbour_below(
        &self,
        victim: usize,
        delta: usize,
    ) -> (bool, bool, bool, u32, usize) {
        let Some(cand) = victim.checked_sub(delta) else {
            return (false, false, false, 0, 0);
        };
        if cand < self.arena_base {
            return (false, false, false, 0, 0);
        }
        // SAFETY: `cand` is inside the arena and the world is stopped for the
        // census. Reading a header out of arbitrary payload bytes is exactly
        // what this is for -- `alloc_size` is the plausibility filter.
        let h = self.header_ref(cand as *mut u8);
        let size = Self::alloc_size(h);
        let registered = self.registry.contains(cand);
        (
            size.is_some(),
            registered,
            size.is_some_and(|sz| cand.saturating_add(sz) > victim),
            h.class_id.as_u32(),
            size.unwrap_or(0),
        )
    }

    pub fn survey_registry_extents(&self, all: &[usize], where_: &'static str) -> usize {
        let mut bases: Vec<usize> = all.to_vec();
        bases.sort_unstable();
        let mut overlaps = 0usize;
        let mut unsizable_now = 0usize;
        for w in bases.windows(2) {
            let (b, next) = (w[0], w[1]);
            let h = self.header_ref(b as *mut u8);
            match Self::alloc_size(h) {
                None => unsizable_now += 1,
                Some(sz) => {
                    if b.saturating_add(sz) > next {
                        overlaps += 1;
                        if overlaps <= 4 {
                            let nh = self.header_ref(next as *mut u8);
                            // Was this base a relocation SOURCE the ledger
                            // still remembers? "yes" means the slide moved the
                            // object away and the registry kept its old base;
                            // "no" means the base is one nothing moved, so its
                            // header was overwritten in place.
                            let vacated = if zgc_corpse_enabled() {
                                self.counters.corpse_ledger.lock().get(&b).map(|e| e.3)
                            } else {
                                None
                            };
                            // What this base looked like when the last slide
                            // handed the heap back to the mutators.
                            let (seen_then, size_then) = {
                                let sizes = self.counters.slide_exit_sizes.lock();
                                match sizes.get(&b) {
                                    Some(&s) => (true, s),
                                    None => (false, 0),
                                }
                            };
                            // The raw words at the base, and at the two 8-byte
                            // steps below it. A header that was overwritten in
                            // place has plausible neighbours; a registry entry
                            // that never named an object sits in the middle of
                            // somebody's payload, and the words below it say
                            // whose.
                            // SAFETY: `b` is a registered base inside the
                            // arena, and the world is stopped.
                            let w: [u64; 4] =
                                unsafe { std::ptr::read_unaligned(b as *const [u64; 4]) };
                            let below: [u64; 2] = if b >= self.arena_base + 16 {
                                // SAFETY: as above; `b - 16` is still inside.
                                unsafe { std::ptr::read_unaligned((b - 16) as *const [u64; 2]) }
                            } else {
                                [0; 2]
                            };
                            tracing::error!(
                                target: "cratonvm::gc::guard",
                                site = where_,
                                base = b,
                                size = sz,
                                ends_at = b + sz,
                                next_base = next,
                                overrun = b + sz - next,
                                kind = ?h.kind(),
                                class_id = h.class_id.as_u32(),
                                num_slots = h.num_slots(),
                                array_length = h.array_length(),
                                next_class_id = nh.class_id.as_u32(),
                                next_num_slots = nh.num_slots(),
                                vacated_in_cycle = ?vacated,
                                seen_at_slide_exit = seen_then,
                                size_at_slide_exit = size_then,
                                w0 = format!("{:#018x}", w[0]),
                                w1 = format!("{:#018x}", w[1]),
                                w2 = format!("{:#018x}", w[2]),
                                w3 = format!("{:#018x}", w[3]),
                                below0 = format!("{:#018x}", below[0]),
                                below1 = format!("{:#018x}", below[1]),
                                // WHAT THE WORDS BELOW MEAN, not just what they
                                // are -- see `decode_neighbour_below`.
                                // `covers=true registered=false` is the second
                                // reading confirmed: the victim is not being
                                // corrupted, it is overlapped.
                                below16 = {
                                    let (p, r, c, cid, sz) = self.decode_neighbour_below(b, 16);
                                    format!(
                                        "plausible={p} registered={r} covers={c} class={cid} size={sz}"
                                    )
                                },
                                below32 = {
                                    let (p, r, c, cid, sz) = self.decode_neighbour_below(b, 32);
                                    format!(
                                        "plausible={p} registered={r} covers={c} class={cid} size={sz}"
                                    )
                                },
                                "zgc extent census: a registered object's computed extent \
                                 runs INTO the next registered object"
                            );
                        }
                    }
                }
            }
        }
        if overlaps != 0 || unsizable_now != 0 {
            tracing::error!(
                target: "cratonvm::gc::guard",
                site = where_,
                overlaps,
                unsizable = unsizable_now,
                registered = bases.len(),
                compaction_cycles = self.counters.compaction_cycles.load(Ordering::Relaxed),
                "zgc extent census: survey of the object-start registry"
            );
        }
        overlaps
    }

    /// Can the rewrite pass safely walk the object at `base`?
    ///
    /// Answers three questions the rewrite loop used to assume, and reports
    /// which one failed. The report is the point: a bad answer here becomes a
    /// SIGSEGV *inside the collector*, whose stack names only the collector,
    /// so the crash carries no information about which object was malformed or
    /// why.
    ///
    ///  * **Can it be sized?** `alloc_size` refuses an implausible body or
    ///    array length. A header decoded out of payload bytes usually fails
    ///    here.
    ///  * **Does its extent fit the arena?** The last line of defence: a length
    ///    that is plausible in isolation can still run past the end of the
    ///    heap, and striding it is the unmapped-page read.
    ///
    /// # Registry membership is NOT one of the questions, and that is not an
    /// oversight
    ///
    /// The obvious third check — "is this still a registered base?" — is wrong
    /// *here*, and the existing
    /// `collect_garbage_with_compaction_on_rewrites_roots_and_keeps_the_graph`
    /// caught it as a hard failure the first time this guard was written with
    /// it in. The object-start registry is rebuilt AFTER this loop, so while
    /// the rewrite runs it still holds every survivor's PRE-slide base;
    /// `live_now` holds their post-slide ones. Gating on it therefore skips
    /// precisely the objects that DID move — the ones whose slots most need
    /// rewriting. It is reported as a field because it is informative, and it
    /// is not allowed to veto.
    ///
    /// `was_vacated` discriminates the causes: if THIS slide vacated the exact
    /// address, the object was overwritten by this cycle's bookkeeping.
    ///
    /// **Read `was_vacated=false` narrowly.** the index covers one cycle, so
    /// an address vacated nine collections ago also reports `false`. On the
    /// netty repro every offender reported `false` while its "header" decoded
    /// as ASCII — `class_id=0x41524150` is `"PARA"`, `num_slots=0x444f494e` is
    /// `"NOID"`, i.e. netty's `PARANOID` level string, and neighbours spelled
    /// `io/netty`. So the registry held bases pointing into STRING data: the
    /// same bytes were two things at once, from an earlier cycle. Widening this
    /// to a multi-cycle set (as `corpse_ledger` already is) is the obvious
    /// improvement if this fires again.
    ///
    /// # Callers and scope
    ///
    /// For an IN-ARENA `base` whose size the caller already read (zr z2/r2: the
    /// fused rewrite reads it once for this check and the `live_ceiling`
    /// screen). zs s2/sl2: the unsized wrapper `rewrite_target_is_walkable`
    /// had no caller left and is gone; its doc is this one. Its "off-arena
    /// survivors are the high-address end, which this slide never touches"
    /// was wrong twice: `[arena_lo, arena_hi)` INCLUDES the high end, which
    /// `compact_high_region` does move, and an off-arena base is a spilled
    /// registration, not the large-object end. The caller
    /// (`rewrite_reference_slots_of`) checks only in-arena survivors.
    #[must_use]
    pub fn rewrite_target_walkable_sized(
        &self,
        base: usize,
        size: Option<usize>,
        arena_hi: usize,
        fwd: &relocate::ZForwardIndex,
    ) -> bool {
        let header = self.header_ref(base as *mut u8);
        let fits = size.is_some_and(|sz| base.saturating_add(sz) <= arena_hi);
        if fits {
            return true;
        }
        // zs s1/dg (`CRATONVM_ZGC_REWRITE_OVERSIZE_ARRAYS`): a REAL array past
        // `MAX_PLAUSIBLE_ARRAY_LEN` (1 << 28 elements) is legal to allocate
        // (`ZGC_REAL_MAX_ARRAY_LENGTH` is `i32::MAX`) and refused by
        // `alloc_size`'s element cap, so this screen used to skip it with the
        // "dangling" line below -- a `byte[273_366_446]` on
        // `NativeGrowthReclaimProbe` (zs1 page). It is walkable when the
        // oversize screen sizes it from independent evidence; a primitive one
        // then has nothing to rewrite, a reference one has its elements walked
        // by `reference_slots_with` under the same screen.
        if size.is_none() {
            if let Some(sz) = self.oversize_live_array_extent(base, header) {
                if base.saturating_add(sz) <= arena_hi {
                    self.counters
                        .rewrite_oversize_arrays_walked
                        .fetch_add(1, Ordering::Relaxed);
                    return true;
                }
            }
        }
        // Reported, never a veto — see the header note on why the registry
        // cannot be consulted at this point in the sequence.
        let registered = self.registry.contains(base);
        // Cap the log, not the screen: the skip must happen for every offender
        // or the walk still faults, but sixteen lines is enough to see the
        // shape and a million would itself be the hang.
        let n = self
            .counters
            .unwalkable_reports
            .fetch_add(1, Ordering::Relaxed);
        if n < 16 {
            tracing::error!(
                target: "cratonvm::gc::guard",
                base,
                registered,
                sizable = size.is_some(),
                size = size.unwrap_or(0),
                fits_arena = fits,
                kind = ?header.kind(),
                element_type = ?header.element_type(),
                class_id = header.class_id.as_u32(),
                num_slots = header.num_slots(),
                array_length = header.array_length(),
                was_vacated = fwd.contains_from(base),
                arena_hi,
                "zgc relocate: rewrite target is not walkable -- skipping it rather \
                 than striding a length this header cannot justify"
            );
        }
        false
    }

}
