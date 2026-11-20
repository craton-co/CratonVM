#[allow(unused_imports)]
use super::*;

impl G1Collector {
    /// r11w18 g1store — where region offset `off` falls in region `idx`'s
    /// object grid, walking the region at most once per `remark` call.
    /// `None` when the per-call walk budget is spent.
    pub fn mark_root_grid_place(
        &self,
        grids: &mut MarkRootGrids,
        region: &G1Region,
        idx: usize,
        off: usize,
    ) -> Option<GridPlace> {
        if let Some(grid) = grids.walked.iter().find(|g| g.idx == idx) {
            return Some(grid.place(off));
        }
        if grids.walked.len() >= MARK_ROOT_GRID_REGION_BUDGET {
            return None;
        }
        let skips = grids
            .skips
            .get_or_insert_with(|| self.jit_tlab_skip_spans());
        let grid = Self::walk_mark_root_grid(region, idx, skips);
        let place = grid.place(off);
        grids.walked.push(grid);
        Some(place)
    }

    /// r11w18 g1store — one linear walk of `region` from its base to its
    /// cursor, recording every OBJECT's extent. Steps the published TLAB skip
    /// spans and gap sentinels by their recorded lengths and TLAB fillers by
    /// their `int[]` size without recording them (they are dead space); tags
    /// are validated before any header borrow, exactly as the other walkers
    /// in this file do. Stops, `complete = false`, at the first bytes that are
    /// not a header or at a size that runs past the cursor.
    pub fn walk_mark_root_grid(
        region: &G1Region,
        idx: usize,
        skips: &[(usize, usize)],
    ) -> MarkRootGrid {
        let base = region.data.as_ptr() as usize;
        let cursor = region.cursor();
        let mut objects: Vec<(usize, usize, bool)> = Vec::new();
        let mut offset = 0usize;
        let mut complete = true;
        // r12w1 (W19-3): the same switch as the marker's gray gate, so a root
        // placed in a flag-less "object" is treated as the gate would treat
        // its seed.
        let require_flag = g1_mark_gray_requires_header_flag();
        while offset < cursor {
            let obj_ptr = (base + offset) as *const u8;
            if let Some(skip) = jit_tlab_skip_span_len(skips, obj_ptr as usize) {
                offset += skip;
                continue;
            }
            if let Some(gap) = gap_filler_len(obj_ptr) {
                offset += gap;
                continue;
            }
            // SAFETY: `offset < cursor`, so the header word is inside this
            // region's allocated prefix; the tag bytes are validated before
            // any `ObjectHeader` borrow.
            let tags_ok = unsafe {
                object_kind_from_tag(cratonvm_types::kind_tag_at(obj_ptr)).is_some()
                    && cratonvm_types::element_type_tag_ok_at(obj_ptr)
            };
            if !tags_ok {
                complete = false;
                break;
            }
            // SAFETY: tags validated above.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            if is_humongous_filler(header) {
                complete = false;
                break;
            }
            let size = object_total_size(header);
            if size < HEADER_SIZE || size > cursor - offset {
                complete = false;
                break;
            }
            if !is_tlab_filler_header(header) {
                let published =
                    !require_flag || header.gc_flags() & cratonvm_types::GC_FLAG_HEADER != 0;
                objects.push((offset, offset + size, published));
            }
            offset += size;
        }
        MarkRootGrid {
            idx,
            objects,
            reached: offset,
            complete,
        }
    }

    /// r12w2 g1store (proposals W1-2 and W1-3) — the mark seeds the published
    /// JIT pins owe `remark`, and a census of them.
    ///
    /// Each distinct pin is placed exactly as a root is
    /// ([`Self::mark_root_seed`]): an object start seeds itself, an interior
    /// word the object containing it, a humongous word its span's object. A
    /// pin whose object some root of this pause already seeds (`root_starts`,
    /// sorted and deduplicated) is `covered` and not returned. What is
    /// returned is what ONLY a pin names, an orphan: an object a compiled
    /// frame reaches through a word the root screen rejected (a derived
    /// pointer, a humongous interior word), or through a stale pin, with no
    /// root naming its start. The census counts them, which answers the
    /// page's open JIT question (is a derived pointer ever live without its
    /// base?) with a number per remark.
    pub fn jit_pin_mark_seeds(
        &self,
        regions: &[G1Region],
        pins: &[usize],
        root_starts: &[usize],
        grids: &mut MarkRootGrids,
    ) -> (Vec<usize>, JitPinSeedTally) {
        let mut pins: Vec<usize> = pins.to_vec();
        pins.sort_unstable();
        pins.dedup();
        let mut tally = JitPinSeedTally {
            pins: pins.len(),
            ..JitPinSeedTally::default()
        };
        let mut seeds: Vec<usize> = Vec::new();
        for pin in pins {
            // An accepted-root pin is a start a root seeded: no placement.
            if root_starts.binary_search(&pin).is_ok() {
                tally.covered += 1;
                continue;
            }
            let start = match self.mark_root_seed(regions, pin, grids) {
                MarkRootSeed::Object(start) => start,
                MarkRootSeed::Nothing => {
                    tally.nothing += 1;
                    continue;
                }
                MarkRootSeed::Unresolved => {
                    tally.unresolved += 1;
                    continue;
                }
            };
            let interior = start != pin;
            if interior {
                tally.interior += 1;
            }
            if root_starts.binary_search(&start).is_ok() {
                tally.covered += 1;
                continue;
            }
            tally.orphans += 1;
            if interior {
                tally.orphan_interior += 1;
            }
            let humongous = self
                .lookup_region_for_addr(start)
                .and_then(|idx| regions.get(idx))
                .is_some_and(|r| r.region_type() == RegionType::HumongousStart);
            if humongous {
                tally.orphan_humongous += 1;
            }
            // Name the frame behind an orphan (the census's trace step): the
            // band scans record each rejected word's slot provenance under the
            // same switch. Bounded per remark.
            if g1_dbg_jit_rootscan() && tally.orphans <= JIT_PIN_ORPHAN_SAMPLES {
                eprintln!(
                    "[g1-jitpin-orphan] pin=0x{pin:x} start=0x{start:x} off={} humongous={humongous} prov={}",
                    pin - start,
                    cratonvm_gc_core::gc_quiescence::lookup_jit_reject_provenance(pin)
                        .or_else(|| cratonvm_gc_core::gc_quiescence::lookup_jit_root_provenance(pin))
                        .unwrap_or_else(|| "-".to_owned()),
                );
            }
            seeds.push(start);
        }
        seeds.sort_unstable();
        seeds.dedup();
        (seeds, tally)
    }

    /// r11w19 census (`CRATONVM_DBG_JIT_ROOTSCAN=1`): ask the next
    /// [`Self::remark`] to park its JIT-pin orphan seeds for
    /// [`Self::finish_deferred_pin_seeds`] instead of pushing them.
    pub fn defer_next_remark_pin_seeds(&self) {
        self.defer_pin_seeds.store(true, Ordering::Release);
    }

    /// r11w19 census: called by the final remark's driver after the root and
    /// SATB closure has drained to a fixed point. Of the orphans the deferred
    /// remark parked, counts those that closure reached anyway (marked), those
    /// allocated since the cycle began (above TAMS, implicitly live), and the
    /// rest: objects ONLY a rejected band word keeps alive, each printed with
    /// the provenance of the word. Then pushes the parked seeds; the caller
    /// drains again. Returns whether anything was pushed.
    pub fn finish_deferred_pin_seeds(&self) -> bool {
        let seeds = std::mem::take(&mut *self.deferred_pin_seeds.lock());
        if seeds.is_empty() {
            return false;
        }
        let regions = self.regions.read();
        let mut worklist = self.mark_worklist.lock();
        let (mut reached, mut implicit, mut unreached) = (0usize, 0usize, 0usize);
        for &start in &seeds {
            let Some(idx) = self.lookup_region_for_addr(start) else {
                continue;
            };
            let r = &regions[idx];
            if r.region_type() == RegionType::Free {
                continue;
            }
            if r.mark_bitmap.is_marked(start) {
                reached += 1;
                continue;
            }
            if start.saturating_sub(r.data.as_ptr() as usize) >= r.tams() {
                implicit += 1;
                continue;
            }
            unreached += 1;
            if unreached <= JIT_PIN_ORPHAN_SAMPLES {
                eprintln!(
                    "[g1-jitpin-unreached] start=0x{start:x} region={idx} type={:?}",
                    r.region_type()
                );
            }
            if worklist.len() >= MARK_WORKLIST_CAP {
                if self.cap_arm_seed_passes_gray_gate(r, start) {
                    r.try_mark_and_account(start);
                }
                self.mark_worklist_overflowed.store(true, Ordering::Relaxed);
                continue;
            }
            self.note_gray(start, "remark-seed", 0);
            worklist.push(start);
        }
        eprintln!(
            "[g1-jitpin-need] orphans={} reached={reached} implicit={implicit} unreached={unreached}",
            seeds.len()
        );
        unreached > 0
    }

    /// Remark phase (STW): mark roots + drain SATB buffer onto the mark
    /// worklist (the gray set), set phase to `Remark`, and let
    /// `concurrent_mark_step` drain the worklist transitively.
    ///
    /// Audit fix (HIGH-3): the previous implementation drained the SATB
    /// queue into `_satb_entries` and threw it away, and only marked the
    /// root addresses directly without pushing them to a worklist — no
    /// transitive closure was ever computed.
    ///
    /// SATB semantics: every pointer in the SATB queue is a *previously
    /// live* reference value that was overwritten while concurrent
    /// marking was active. We must treat each as a root to avoid the
    /// classic G1 lost-object scenario where A→B is replaced by A→null
    /// after we scanned A but before we scanned B.
    ///
    /// NOTE: in this codebase `vm_heap::g1_mark_roots` also calls this
    /// function during the *initial-mark* STW (right after
    /// `start_concurrent_mark`), so the function deliberately treats
    /// SATB draining as idempotent and safe at either point — at
    /// initial mark the SATB queue is freshly activated and typically
    /// empty. Callers who want a true STW final-remark should follow
    /// up with `concurrent_mark_step(usize::MAX)` to drain the worklist
    /// before transitioning to sweep.
    ///
    /// # Stop-the-world
    ///
    /// Requires a [`StopTheWorldToken`] — see
    /// [`Self::start_concurrent_mark`] for why the mark cycle takes the witness
    /// (I-17).
    ///
    /// [`StopTheWorldToken`]: cratonvm_gc_core::collector::StopTheWorldToken
    pub fn remark(&self, _stw: &cratonvm_gc_core::collector::StopTheWorldToken, roots: &[ObjectRef]) {
        // LANE W3-C — see [`REMARK_PAUSES`]: this pause was untimed, so the
        // SATB registry walk a few lines below (the subject of
        // `w2c-the-satb-registry-walk-is-still-o-threads-per-cycle.md`) had no
        // denominator. Both of this function's two calls per cycle are counted;
        // that is deliberate, because both of them take the walk.
        let pause_t0 = std::time::Instant::now();
        // r11w19 census: one-shot, consumed by this remark whether or not its
        // pin step runs.
        let defer_pins = self.defer_pin_seeds.swap(false, Ordering::AcqRel);
        // G1AUD-9 — `remark` is an INSTANT (an STW root re-scan plus a SATB
        // drain), not a phase the cycle sits in, and it is called TWICE per
        // cycle: once from the initial-mark pause to seed the gray set
        // (`VmHeap::g1_mark_roots`, immediately after `start_concurrent_mark`)
        // and once from the final remark. The initial-mark call left the phase
        // at `Remark` for the whole concurrent-mark window, so
        // `G1Collector::gc_phase()` — the field every GC log line, JFR event
        // and `--dump-gc-*` report reads — never once said `ConcurrentMark`
        // while a cycle was running. Nothing gated on it (`is_marking_active()`
        // accepts both, which is why the SATB barrier stayed correct), so this
        // was purely an instrument reporting the wrong phase for ~100% of the
        // cycle: a marker thread saturating a core showed up as a "brief STW
        // remark" that lasted seconds.
        //
        // Restore whatever phase we interrupted. `cleanup`/`abort` set `Idle`
        // on their own way out, so the only phase this ever restores is the
        // marking one it should never have replaced.
        let resume_phase = self.gc_state.phase();
        self.gc_state.set_phase(ConcurrentGcPhase::Remark);

        let regions = self.regions.read();
        // gci i29/cv2 — opt-in `CRATONVM_G1_DBG_FLAGLESS_SWEEP`: what the
        // mutator left before this remark, so `cleanup-end` convicts
        // remark/cleanup alone (the initial-mark call sweeps too, harmlessly).
        self.flagless_header_sweep(&regions, "remark-start", None);
        let mut worklist = self.mark_worklist.lock();

        // Round-2 fix (HIGH — GC #5): bitmaps are per-region. Helper
        // returns the owning region index (if any) so we can consult
        // *that* region's bitmap for the already-marked check.
        // CRIT-perf fix: use the O(log R) cached `lookup_region_for_addr`
        // instead of an O(R) linear scan; preserve the Free-region skip
        // by post-filtering on the looked-up region's type.
        let region_for = |addr: usize| -> Option<usize> {
            let idx = self.lookup_region_for_addr(addr)?;
            if regions[idx].region_type() == RegionType::Free {
                None
            } else {
                Some(idx)
            }
        };

        // Round-9 gc HIGH-5 — graceful overflow. The previous panic was
        // reachable from any wide-graph workload (hostile or otherwise).
        //
        // G1MARK-7: at the cap, a SEED-class entry (root or SATB overwrite)
        // must NOT be plain-dropped the way `scan_object_refs` drops child
        // pushes: the overflow rescan only re-walks MARKED objects, and no
        // marked object need reference a root or a SATB seed — a dropped
        // unmarked seed was unrecoverable and cleanup freed it live. Mark it
        // black without scanning instead (same recovery contract as the
        // CSet-drain and `push_gray_or_mark` paths): the forced rescan scans
        // every marked object's fields, closing the seed's subtree.
        let overflow_flag = &self.mark_worklist_overflowed;
        let push_with_cap = |worklist: &mut Vec<usize>, idx: usize, addr: usize| {
            // g1ep7: a young root or SATB entry needs no trace in a
            // root-region cycle.
            if self.root_region_skips(&regions[idx]) {
                return;
            }
            if worklist.len() >= MARK_WORKLIST_CAP {
                // r11w19 g1store (W18-3): through the gray gate first.
                if self.cap_arm_seed_passes_gray_gate(&regions[idx], addr) {
                    regions[idx].try_mark_and_account(addr);
                }
                overflow_flag.store(true, Ordering::Relaxed);
                return;
            }
            self.note_gray(addr, "remark-seed", 0);
            worklist.push(addr);
        };

        // 1) Roots — push every non-null in-heap root onto the gray set.
        //    `concurrent_mark_step` will mark them and follow their refs.
        //
        //    r11w18 g1store — each root is SCREENED first (`mark_root_seed`):
        //    the array is over-approximate by design (range-filtered operand
        //    stacks, conservative band scans of compiled frames), and the
        //    marker trusts a gray address to be an object start. A published
        //    start is pushed as before; an interior word seeds the object that
        //    contains it; a word naming no allocated object seeds nothing; a
        //    word the grid cannot place takes the cleanup fail-safe.
        //    `CRATONVM_G1_MARK_ROOT_SCREEN=0` restores the raw push.
        let screen_roots = g1_mark_root_screen();
        let mut root_grids = MarkRootGrids::default();
        // r12w2 g1store (W1-2): the JIT pin snapshot is seeded below, and
        // needs the roots' seeds to tell a pin the roots already cover from
        // one only a pin names. Collected only when that step will run.
        let seed_jit_pins =
            screen_roots && jit_frames_live_for_pins();
        let mut root_starts: Vec<usize> = Vec::new();
        for root in roots {
            let p = root.as_ptr();
            if p.is_null() {
                continue;
            }
            let addr = if screen_roots {
                match self.mark_root_seed(&regions, p as usize, &mut root_grids) {
                    MarkRootSeed::Object(start) => start,
                    MarkRootSeed::Nothing => continue,
                    MarkRootSeed::Unresolved => {
                        self.mark_saw_implausible.store(true, Ordering::Relaxed);
                        continue;
                    }
                }
            } else {
                p as usize
            };
            if seed_jit_pins {
                root_starts.push(addr);
            }
            if let Some(idx) = region_for(addr) {
                if regions[idx].mark_bitmap.is_marked(addr) {
                    continue; // already black
                }
                push_with_cap(&mut worklist, idx, addr);
            }
        }

        // 1b) r12w2 g1store (proposal W1-2, page
        //     `r11w19-g1store-band-root-rejects-drop-derived-pointer-pins`):
        //     every published JIT pin seeds the object that contains it.
        //     A band word the root screen rejects (a derived pointer, any word
        //     of a humongous span but its base) is never in `roots`, only in
        //     the pin set (`memory::roots::publish_g1_band_reject_pins` and the
        //     two deposit paths). `cleanup` keeps a pinned region, but the
        //     object a pin alone keeps was left UNMARKED: its referents were
        //     unmarked too (freed in place if nothing else in their region
        //     was live), and remark-time reference processing
        //     (`is_live_after_mark`) judged it dead while the frame used it.
        //     A pin goes through the same placement as a root, and one the
        //     grid cannot place takes the same fail-safe: cleanup keeps its
        //     region through the pin set, but nothing marks what the object
        //     it lives in references, in any region.
        //     `CRATONVM_G1_REMARK_SEEDS_JIT_PINS=0` skips this step.
        if seed_jit_pins {
            root_starts.sort_unstable();
            root_starts.dedup();
            let (seeds, tally) = self.jit_pin_mark_seeds(
                &regions,
                &cratonvm_gc_core::gc_quiescence::pinned_jit_roots_snapshot(),
                &root_starts,
                &mut root_grids,
            );
            if tally.unresolved > 0 {
                self.mark_saw_implausible.store(true, Ordering::Relaxed);
            }
            // r11w19 census: under a deferring final remark the seeds are
            // pushed after the closure has drained, by
            // `finish_deferred_pin_seeds`, which first counts what the roots
            // and SATB alone did not reach.
            let seeds = if defer_pins {
                *self.deferred_pin_seeds.lock() = seeds;
                Vec::new()
            } else {
                seeds
            };
            for addr in seeds {
                if let Some(idx) = region_for(addr) {
                    if regions[idx].mark_bitmap.is_marked(addr) {
                        continue;
                    }
                    push_with_cap(&mut worklist, idx, addr);
                }
            }
            tally.report();
        }

        // 2) SATB completeness (finding #18): before draining the global
        //    shards, pull in every live mutator's partially-full per-thread
        //    buffer. The fast-path barrier only spills a thread's local buffer
        //    into the shards when it fills (~256 entries) or when that thread
        //    self-flushes; references a thread overwrote since its last spill
        //    live only in its local buffer, invisible to the shard `drain()`
        //    below. Excluded from the remark snapshot, the still-live objects
        //    they point at are swept while reachable (use-after-free).
        //
        //    G1's remark uses `drain()` (not `deactivate_and_drain()`, which
        //    runs only at end-of-cycle `cleanup` where stragglers are
        //    discarded), so this is the one place that must drain the registry.
        //    Sound only because `remark` runs at the STW safepoint (final
        //    remark; and the initial-mark call, where buffers are typically
        //    empty): no mutator is mid-barrier, so nothing re-fills a buffer
        //    after we drain it. Draining every buffer here from the collector
        //    removes the dependence on each mutator self-flushing at the
        //    safepoint — the external, unenforced contract finding #18 flagged.
        //
        //    No `debug_assert!` that all registered buffers are now empty: the
        //    registry is process-global and this crate cannot observe the VM's
        //    STW state, so such a check races with any concurrent SATB user
        //    (notably the parallel test harness) and would flake. The contract
        //    is instead verified deterministically by
        //    `satb::tests::flush_all_captures_every_parked_mutator_buffer`.
        cratonvm_gc_core::satb::flush_all_thread_satb_buffers(&self.satb_queue);

        // 2) SATB — every overwritten reference becomes a root.
        let satb_entries = self.satb_queue.drain();
        for addr in satb_entries {
            if addr == 0 {
                continue;
            }
            if let Some(idx) = region_for(addr) {
                if regions[idx].mark_bitmap.is_marked(addr) {
                    continue;
                }
                push_with_cap(&mut worklist, idx, addr);
            }
        }

        // Note: we leave SATB *active* — the cycle continues in the
        // background marker. `cleanup()` (the final phase) is the right
        // place to deactivate SATB, since at that point marking is
        // truly complete.

        // G1AUD-9: hand the phase back (see the note at the top). `Remark` is
        // marking-active and so is `ConcurrentMark`, so the SATB gate never
        // flickers across either transition.
        if resume_phase == ConcurrentGcPhase::ConcurrentMark {
            self.gc_state.set_phase(ConcurrentGcPhase::ConcurrentMark);
        }
        drop(worklist);
        drop(regions);
        // Item 9b — the seeds are work; a worker parked on an empty gray set
        // must not learn that from its fallback poll.
        self.wake_marker();
        // LANE W3-C — after `wake_marker`, so the number covers everything the
        // stopped world paid for. See [`REMARK_PAUSES`].
        REMARK_PAUSES.fetch_add(1, Ordering::Relaxed);
        REMARK_PAUSE_US.fetch_add(pause_t0.elapsed().as_micros() as u64, Ordering::Relaxed);
    }

    /// Cleanup phase: compute per-region live_bytes and gc_efficiency,
    /// free completely empty old regions.
    ///
    /// # Stop-the-world
    ///
    /// Requires a [`StopTheWorldToken`] — see
    /// [`Self::start_concurrent_mark`] for why the mark cycle takes the witness
    /// (I-17).
    ///
    /// [`StopTheWorldToken`]: cratonvm_gc_core::collector::StopTheWorldToken
    /// JIT round 23 wave 9, lane L9 (`CRATONVM_G1_CLEANUP_PARALLEL_SCRUB`,
    /// OPT-IN `=1` since the n23-w9c A/B): `cleanup`'s dead-reference scrub
    /// ([`scrub_dead_refs_below_tams`]) over the persistent evacuation pool,
    /// one region per claim.
    ///
    /// # Why
    ///
    /// The scrub walks every object below TAMS of every partly-live region
    /// and nulls the reference slots of the dead ones, inside the cleanup
    /// pause, on the driver alone while the pool is parked. Sampled on
    /// n23-w9base (`KeepLong 30 -XX:+UseG1GC -Xmx1g`, busiest thread, 25 s):
    /// `G1Collector::cleanup` 58% of the pausing thread, 34% of it self (the
    /// inlined walk), `clear_dead_object_reference_edges` 15%. HotSpot does
    /// this work in its concurrent Rebuild-and-Scrub phase, on its worker
    /// threads.
    ///
    /// # Why it is sound
    ///
    /// `scrub_dead_refs_below_tams` reads its region's mark bitmap, TAMS and
    /// objects, and writes zeros into the reference slots of that region's
    /// own dead objects below TAMS: two regions never share a byte, so each
    /// claimed index is one thread's alone (one `fetch_add` cursor), the
    /// disjointness the parallel Phase-4 walk (`CRATONVM_G1_PARALLEL_FIXUP`)
    /// rests on. `jit_skips` is read only, and the census is two atomics. The
    /// table is reached through [`RegionsBase`] as there: the caller holds
    /// the regions write guard for the whole call and does not touch it until
    /// `EvacPool::scope`'s barrier. Every write is a zero into a dead object,
    /// so the heap after the barrier is the serial loop's whatever the order.
    pub fn cleanup_scrub_regions_parallel(
        &self,
        regions: &mut Vec<G1Region>,
        work: &[(usize, usize, usize)],
        jit_skips: &[(usize, usize)],
    ) {
        let helpers = phase4_fixup_helpers(
            self.parallel_worker_count(),
            self.evac_pool().helpers(),
            work.len(),
        );
        let regions_len = regions.len();
        let base = RegionsBase(regions.as_mut_ptr());
        let next = AtomicUsize::new(0);
        let body = |_worker: usize| {
            // SAFETY: as in `update_references_in_regions_parallel_screened`:
            // the caller's guard keeps the table alive and unreallocated, and
            // each region is touched by the one thread that claimed it.
            let table: RegionsBase = base;
            let view: &[G1Region] =
                unsafe { std::slice::from_raw_parts(table.0 as *const G1Region, regions_len) };
            loop {
                let k = next.fetch_add(1, Ordering::Relaxed);
                if k >= work.len() {
                    break;
                }
                let (idx, region_base, tams) = work[k];
                // SAFETY: stop-the-world cleanup under the regions write guard,
                // `tams` bounds the walk inside the region's allocated prefix
                // (the inline call's contract), and no other thread walks
                // this region.
                let (objs, bytes) =
                    unsafe { scrub_dead_refs_below_tams(&view[idx], region_base, tams, jit_skips) };
                CLEANUP_SCRUBBED_OBJECTS.fetch_add(objs, Ordering::Relaxed);
                CLEANUP_SCRUBBED_BYTES.fetch_add(bytes, Ordering::Relaxed);
            }
        };
        self.evac_pool().scope(helpers, &body, || body(helpers));
    }

    pub fn cleanup(&self, stw: &cratonvm_gc_core::collector::StopTheWorldToken) {
        // LANE W3-C — see [`REMARK_PAUSES`]. Taken before the `regions` write
        // guard on purpose: waiting for that guard is time the world is
        // stopped, and a measurement that starts after the wait would
        // under-report exactly the contention a thread-heavy workload creates.
        let pause_t0 = std::time::Instant::now();
        // The mark closure is complete (or cleanup is about to refuse to act
        // on it): nothing scans an owner after this point, and a snapshot left
        // behind would keep its values alive through every pause until the
        // next cycle. See `g1_overlay.rs`.
        self.clear_overlay_snapshot();
        let mut regions = self.regions.write();
        // LANE W2-C — sample the old-generation occupancy BEFORE this pause
        // reclaims anything.
        //
        // Two consumers, both of which need the PRE-reclaim number and neither
        // of which could get it from where they run:
        //
        //  * `note_mark_cycle_end` (adaptive IHOP) wants how far old occupancy
        //    ROSE while marking ran, which is what decides whether the heap
        //    fills before the closure completes. It is called at the very end
        //    of this function, after `recompute_old_gen_bytes` has applied the
        //    in-place Old frees and the dead-humongous reclaim, so the figure
        //    it reads is the one this cycle LEFT BEHIND.
        //  * the mark-cycle back-off wants the DIFFERENCE across the reclaim,
        //    which is this value minus the republished one.
        //
        // Taken under the regions write guard, so it cannot straddle an
        // evacuation pause's own republish. `usize::MAX` is the sentinel for
        // "not sampled"; a swap here would be wrong, because a `cleanup` that
        // takes a fail-safe still passes through the consumer below.
        let old_bytes_before_reclaim = self.old_gen_bytes.load(Ordering::Relaxed);
        self.mark_cycle_gross_old_bytes
            .store(old_bytes_before_reclaim, Ordering::Relaxed);
        // SECURITY FIX (V7a): cleanup recycles completely-empty Old
        // regions (reset to Free below). Invalidate every mutator's RSet
        // fast-path cache before that reclassification — see
        // `rset_cache_epoch`.
        self.bump_rset_cache_epoch();
        // Item 1 -- a pause's span marks belong to that pause.
        self.clear_humongous_references();
        // G1AUD-5: the generation this cleanup owns. Read AFTER the bump so
        // every region recycled below is stamped with a value strictly greater
        // than any generation a still-running mutator could have recorded an
        // edge in.
        let cleanup_generation = self.rset_generation();
        let region_size = self.config.region_size;
        // G1MARK-8 fail-safe: the marker skipped an implausible-header gray
        // entry this cycle, so the closure may be incomplete — a 0-live
        // verdict is not trustworthy. Retain everything (no in-place frees,
        // no humongous reclaim); the next cycle re-derives liveness from
        // scratch. Swap-and-clear: the flag is per-cycle.
        let saw_implausible = self.mark_saw_implausible.swap(false, Ordering::Relaxed);
        // The routine half of the old gate, reported so its absence from the
        // fail-safe is visible rather than assumed. A large count here with
        // `saw_implausible` false is the expected steady state, not a warning:
        // unscrubbed recycled regions plus SATB retention make stale gray
        // addresses ordinary.
        let oob_skips = self.mark_oob_gray_skips.swap(0, Ordering::Relaxed);
        if oob_skips > 0 {
            tracing::debug!(
                "g1 cleanup: {} gray entries skipped as never-allocated (not a torn header; reclamation decisions retained)",
                oob_skips
            );
        }
        if saw_implausible {
            tracing::warn!(
                "g1 cleanup: implausible gray entry seen during marking — \
                 retaining all regions this cycle (no in-place frees)"
            );
        }
        // r11w19 g1store (W18-1) -- how many of the impugning refusals were a
        // header without `GC_FLAG_HEADER`. Non-zero implies `saw_implausible`
        // (every such refusal is a `TornHeader`), so this adds no noisy cycle.
        let flagless_grays = self
            .mark_gray_without_header_flag
            .swap(0, Ordering::Relaxed);
        if flagless_grays > 0 {
            tracing::warn!(
                "g1 cleanup: {} gray entries or cap-arm seeds had no GC_FLAG_HEADER \
                 (refused as torn headers; CRATONVM_G1_MARK_GRAY_REQUIRES_HEADER_FLAG=0 \
                 restores the decode-only gate)",
                flagless_grays
            );
        }
        // r12w1 g1store (proposal W19-2) -- one line per cycle, zeros included,
        // so "the flag is never missing on a real object" is a number every G1
        // run can publish rather than the absence of a warning.
        tracing::debug!(
            "g1 cleanup summary: oob_gray_skips={oob_skips} flagless_grays={flagless_grays} \
             saw_implausible={saw_implausible}"
        );
        // G1AUD-3 — the gray set MUST be empty here.
        //
        // Cleanup's in-place free of a zero-live Old region (and the humongous
        // reclaim below) is sound only if the transitive closure is COMPLETE:
        // `live_bytes == 0` is read as "every object in this region predates
        // the mark snapshot and is unreachable in it". A non-empty gray set
        // means the closure was never driven to a fixed point, so unmarked
        // does not imply unreachable and the verdict frees live objects.
        //
        // The precondition was documented on the driver
        // (`VmHeap::g1_final_remark_and_cleanup` runs
        // `while !concurrent_mark_step(usize::MAX) {}` before calling here) and
        // enforced by nothing — and the sibling driver
        // `VmHeap::g1_signal_marking_complete`, retained for the abort/teardown
        // path, calls `cleanup()` with NO remark and NO drain at all. Rather
        // than trust the caller, detect it and take the same fail-safe the
        // implausible-header gate takes: retain everything for this cycle. The
        // next cycle re-derives liveness from scratch, so the cost is one
        // delayed reclamation, against freeing a live region.
        //
        // Lock order is regions -> worklist, matching every other site that
        // holds both (`marking_keepalive_roots`, `concurrent_mark_step`, and
        // the post-pause worklist remaps).
        // Deliberately a warn + retain, NOT a `debug_assert!`: cleanup runs on
        // a heap that may already be damaged, and the module's own policy (see
        // the `live_bytes > region.cursor()` clamp below) is that it must not
        // introduce a panic path there. The invariant is pinned by
        // `cleanup_with_an_undrained_gray_set_retains_every_region` instead,
        // which is a stronger check than an assertion because it proves the
        // fail-safe actually retains.
        let closure_incomplete = !self.gray_set_is_empty();
        if closure_incomplete {
            tracing::warn!(
                "g1 cleanup: gray set non-empty at cleanup — the mark closure is \
                 incomplete; retaining all regions this cycle (no in-place frees). \
                 Drive `concurrent_mark_step(usize::MAX)` to a fixed point first \
                 (see `VmHeap::g1_final_remark_and_cleanup`)."
            );
        }
        // Either fail-safe suppresses every reclamation decision this cycle.
        let retain_all = saw_implausible || closure_incomplete;
        self.last_mark_trusted.store(!retain_all, Ordering::Release);
        // gci i18/nr (vi17-nr R1 fallout): the fail-safe's warnings are below
        // the default log filter, so a run whose every cleanup retains
        // everything (vi17-nr `nr17_jt_lt1_r4`: 55 cleanups, nothing freed,
        // old regions to 510 / 512, OutOfMemoryError) said nothing. One line
        // per fail-safe cleanup under `--verbose:gc`, and a per-heap count.
        if retain_all {
            let n = self
                .scrub_counters
                .retain_all_cycles
                .fetch_add(1, Ordering::Relaxed)
                + 1;
            if self.gc_log_enabled.load(Ordering::Relaxed) {
                eprintln!(
                    "[GC] g1 cleanup fail-safe: retain_all=true saw_implausible={saw_implausible} \
                     closure_incomplete={closure_incomplete} flagless_grays={flagless_grays} \
                     oob_gray_skips={oob_skips} cycles_total={n}"
                );
            }
        }
        // TAMS guard (see `mark_start_snapshot`): bytes allocated after the
        // mark-start snapshot carry no mark information and MUST count as
        // live, or this pass frees Old regions filled by promotion during
        // the cycle and zeroes live objects.
        let mark_snapshot: Vec<(u64, usize, RegionType)> = self.mark_start_snapshot.lock().clone();
        // r12w1 g1store: regions a live compiled frame pins are not freed in
        // place this cycle. See `cleanup_jit_pinned_regions`.
        let cleanup_jit_pins = self.cleanup_jit_pinned_regions(&regions);
        let jit_skips = self.jit_tlab_skip_spans();
        // gci i16/nc -- the regions this cleanup frees and their rsets' sources,
        // taken before `reset` clears them; see `scrub_slots_into_freed_regions`.
        let scrub_freed_targets = self.cleanup_scrub_freed_targets;
        let mut freed_targets = CleanupFreedTargets::default();
        // gci i17/nr (R2): with the scrub on, the in-place frees are decided in
        // the loop but applied only after the scrub (see the commit below).
        let mut pending_old_frees: Vec<usize> = Vec::new();
        // gci i39/cv -- opt-in `CRATONVM_G1_DBG_FREED_SLOT_AUDIT`: the rsets of
        // every region this cleanup may free (in-place Old, dead humongous
        // span starts), before any reset; audited after the reclaim below.
        let freed_slot_pre = if g1_dbg_freed_slot_audit() {
            let candidates: Vec<usize> = regions
                .iter()
                .enumerate()
                .filter(|(_, r)| {
                    matches!(
                        r.region_type(),
                        RegionType::Old | RegionType::HumongousStart
                    )
                })
                .map(|(i, _)| i)
                .collect();
            Some(FreedSlotAuditPre::take(&regions[..], candidates, &[]))
        } else {
            None
        };

        // JIT round 23 wave 9, lane L9 (`CRATONVM_G1_CLEANUP_PARALLEL_SCRUB`):
        // the regions whose dead-reference scrub runs on the pool after this
        // loop instead of inline in it. Empty with the switch off.
        let parallel_scrub = g1_cleanup_parallel_scrub_enabled();
        let mut deferred_scrub: Vec<(usize, usize, usize)> = Vec::new();
        for (region_idx, region) in regions.iter_mut().enumerate() {
            if region.region_type() == RegionType::Free {
                continue;
            }

            // F-06 — per-region liveness, read rather than recomputed.
            //
            // This used to WALK every object of every non-Free region here,
            // consulting the bitmap and summing sizes: an O(heap)
            // stop-the-world pass at the end of every concurrent cycle, growing
            // with the old generation — the one part of the heap the collector
            // otherwise never touches synchronously. Real G1 has no such pause
            // because it accumulates the same number DURING marking, which is
            // what `G1Region::try_mark_and_account` now does. Cleanup is
            // arithmetic over `regions`.
            //
            // TAMS (top-at-mark-start) for this region. Objects BELOW it were
            // in the mark snapshot, so the bitmap — and therefore the
            // accumulator — is authoritative for them; everything at or above
            // it postdates the snapshot, carries no mark information, and is
            // implicitly live. The three cases the rule has to cover moved to
            // `G1Region::tams`, which the marker reads too, so the accumulation
            // and the verdict cannot drift apart.
            //
            // G1MAT-1 (double-count fix) is preserved by construction rather
            // than by a bound on a walk: `try_mark_and_account` adds nothing
            // for an object at or above TAMS, so a post-TAMS object the marker
            // DID reach (SATB keep-alive, `push_gray_or_mark`, a fresh
            // promotion a root still names) is counted once, by the wholesale
            // extent below. Getting that wrong inflates `gc_efficiency`, which
            // is the key `select_old_regions_for_mixed_gc` sorts on
            // (ascending = worst-first), so mixed GC would pick the wrong
            // regions and `estimated_evac_cost_ns` would over-charge the pause
            // budget and pick fewer of them.
            let base = region.data.as_ptr() as usize;
            let tams = region.tams();
            debug_assert_eq!(
                tams,
                cleanup_tams_from_snapshot(&mark_snapshot, region_idx, region),
                "region {region_idx}: the per-region TAMS and the mark-start \
                 snapshot disagree — `start_concurrent_mark` writes both in one \
                 loop, so they can only diverge if one of them stopped being \
                 maintained"
            );
            // Retyping during marking makes TAMS zero. Credits earned under
            // the preceding type no longer describe bytes below this TAMS;
            // counting them plus the whole implicitly-live extent counts the
            // same objects twice (not evidence of header corruption).
            let marked_below = if tams == 0 {
                0
            } else {
                region.marked_bytes_below_tams.load(Ordering::Relaxed)            };
            let mut live_bytes = marked_below.saturating_add(region.cursor().saturating_sub(tams));

            // The walk this replaced, kept as a lever and as an oracle.
            //
            // `CRATONVM_G1_CLEANUP_WALK=1` restores it as the authority, which
            // is the single-binary A/B for the change and the first thing to
            // try if a G1 cycle is suspected of freeing a live Old region. In
            // a debug build it runs anyway and must agree: an accumulated
            // liveness is only as good as the claim that every mark site goes
            // through the accumulator, and that claim deserves a check rather
            // than a comment.
            if gc_flags().g1_cleanup_walk || cfg!(debug_assertions) {
                let (walked, walk_complete) =
                    walk_marked_bytes_below_tams(region, base, tams, &jit_skips);
                let walked_live = walked.saturating_add(region.cursor().saturating_sub(tams));
                // The walk may not exceed the accumulator. The reverse is
                // allowed, and that asymmetry is the whole content of this
                // check.
                //
                // The two count the same bits by DIFFERENT KEYS: the
                // accumulator is keyed by the address that was marked, the walk
                // by the object grid. They agree exactly when every mark landed
                // on an object start — and marks do not always. Measured on
                // `StringNativeAllocationChurn` under G1: region 1 accumulated
                // 136 bytes against a COMPLETE walk of 104, and the two
                // addresses responsible (offsets 545408 and 545520 of a 655408
                // extent) were marked but were not object starts, so the
                // accumulator charged them a header each and the grid never
                // landed on them to ask. `concurrent_mark_step` tolerates such
                // an address by design — `classify_mark_scan_target` refuses a
                // torn header, but a stale reference into an unscrubbed
                // recycled region is the documented COMMON case — so this is
                // not a defect in the marker either.
                //
                // A walk that stops early (a humongous filler, or a header
                // whose extent runs past the cursor) is the second way the
                // accumulator legitimately reads larger. That break is the one
                // the walk had when it WAS cleanup's implementation, which
                // means the old cleanup silently under-counted exactly there.
                //
                // Both cases over-count, and over-counting RETAINS a region for
                // one more cycle. The direction that must stay impossible is
                // the other one: a walk finding MORE live bytes than the
                // accumulator means a bit was set without going through
                // `try_mark_and_account`, and an under-count is what lets
                // cleanup free a live Old region in place.
                debug_assert!(
                    walked_live <= live_bytes,
                    "region {region_idx}: a walk found MORE live bytes ({walked_live}) than the accumulator ({live_bytes}) — walk_complete={walk_complete}, tams={tams}, cursor={}. Some mark site is setting a bit without going through `try_mark_and_account`; an under-count is what lets cleanup free a live Old region in place",
                    region.cursor(),
                );
                if gc_flags().g1_cleanup_walk {
                    live_bytes = walked_live;
                }
            }

            // Invariant restored by G1MAT-1: a region can never be more than
            // 100% live, so `gc_efficiency` stays in [0, 1] and the worst-first
            // mixed-GC ranking is meaningful. Under bump allocation TAMS is
            // always an object boundary, so the walk above cannot legitimately
            // count an object that straddles it — but a corrupt header can
            // report an implausible extent that still fits inside `cursor`.
            // Clamp (and log) rather than `debug_assert!`: cleanup must not
            // introduce a new panic path on an already-damaged heap, and a
            // clamped value is still non-zero, so the in-place-free decision
            // below is unaffected.
            if live_bytes > region.cursor() {
                tracing::warn!(
                    "g1 cleanup: region {} reported {} live bytes over a {}-byte \
                     cursor (tams {}) — clamping; a header in this region is \
                     likely corrupt",
                    region_idx,
                    live_bytes,
                    region.cursor(),
                    tams
                );
                live_bytes = region.cursor();
            }

            region.live_bytes = live_bytes;
            // gci i10/rh finding 1 -- HotSpot's garbage is fixed here.
            region.cursor_at_cleanup = region.cursor();
            if Self::gray_prov_enabled()
                && (region.region_type() == RegionType::HumongousStart
                    || (region.cursor() >= 240_000
                        && unsafe { &*(base as *const ObjectHeader) }.kind() == ObjectKind::Array))
            {
                eprintln!("[HUM-CLEANUP] addr={base:#x} region={region_idx} epoch={} tams={tams} cursor={} live={live_bytes} marked={} jit_pin={} jni_pin={} retain_all={retain_all}",
                    region.reuse_epoch, region.cursor(), region.mark_bitmap.is_marked(base), cleanup_jit_pins.contains(&region_idx), region.pinned);
            }
            region.gc_efficiency = if region_size > 0 {
                live_bytes as f64 / region_size as f64
            } else {
                0.0
            };

            // In-place free of wholly-dead Old regions — RESTORED. This was
            // disabled ("containment") when concurrent marking missed objects
            // whose holders young collections moved mid-cycle (SteadyChurn
            // @16m: live list nodes vanished right after a mark cycle,
            // observed via CRATONVM_G1_DBG_REACH + the [FREED] cleanup
            // trace). The marker itself is fixed now, so a 0-live verdict is
            // trustworthy again:
            //
            //   1. FINAL REMARK actually runs: the VM finishes every cycle
            //      with an STW remark (root re-scan + SATB drain) and drains
            //      the gray set to a fixed point BEFORE calling cleanup
            //      (`vm_heap::g1_final_remark_and_cleanup`). Previously the
            //      SATB log was never consumed — the whole cycle was a
            //      one-shot closure racing mutator edge deletions.
            //   2. KEEP-ALIVE across evacuation pauses: CSet-resident gray
            //      and SATB entries are evacuated like roots and re-grayed
            //      (`marking_keepalive_roots`), never dropped — snapshot-live
            //      subtrees survive every young/mixed pause in the cycle.
            //   3. TAMS: the `mark_start_snapshot` guard above counts every
            //      post-snapshot allocation as live.
            //
            // Under (1)–(3), `live_bytes == 0` ⇒ every object in the region
            // predates the mark snapshot and is unreachable in it ⇒ garbage
            // by SATB. The empty-snapshot gate keeps the bitmap-only verdict
            // advisory when cleanup is driven outside a real cycle (unit
            // tests) — no in-place free there.
            if !retain_all
                && !mark_snapshot.is_empty()
                && region.live_bytes == 0
                && region.region_type() == RegionType::Old
                && !region.pinned
                && !cleanup_jit_pins.contains(&region_idx)
            {
                if scrub_freed_targets {
                    // gci i17/nr (R2): a CANDIDATE. The reset waits for the
                    // freed-target scrub below, which may keep it.
                    freed_targets.note(region_idx, &region.rset);
                    pending_old_frees.push(region_idx);
                } else {
                    if gc_flags().g1_dbg_reach {
                        eprintln!(
                            "[g1][FREED] cleanup region={region_idx} cursor={:#x}",
                            region.cursor()
                        );
                    }
                    self.old_regions_reclaimed.fetch_add(1, Ordering::Relaxed);
                    region.reset(cleanup_generation);
                }
            } else if !retain_all
                && !mark_snapshot.is_empty()
                && matches!(
                    region.region_type(),
                    RegionType::Eden | RegionType::Survivor | RegionType::Old
                )
                // gci i15/ra F7: a JNI-pinned region is scrubbed too -- the
                // pin keeps its live objects in place, and an unscrubbed dead
                // object's stale slots would re-derive rset edges into reused
                // regions on every later fix-up.
                && (!cleanup_jit_pins.contains(&region_idx)
                    || (g1_mark_root_screen()))
                && tams > 0
                && g1_cleanup_scrub()
            {
                // SAFETY: stop-the-world cleanup under the regions write
                // guard; `tams` bounds the walk inside the region's
                // allocated prefix, and every object below it predates the
                // mark snapshot this bitmap answers for.
                if parallel_scrub {
                    deferred_scrub.push((region_idx, base, tams));
                } else {
                    let (objs, bytes) =
                        unsafe { scrub_dead_refs_below_tams(region, base, tams, &jit_skips) };
                    CLEANUP_SCRUBBED_OBJECTS.fetch_add(objs, Ordering::Relaxed);
                    CLEANUP_SCRUBBED_BYTES.fetch_add(bytes, Ordering::Relaxed);
                }
            }
        }
        // JIT round 23 wave 9, lane L9: the deferred scrubs, before anything
        // after the loop reads a region's objects (the freed-target scrub
        // below counts the slots it nulls, so it must see these already
        // nulled, as the inline order did).
        if !deferred_scrub.is_empty() {
            self.cleanup_scrub_regions_parallel(&mut regions, &deferred_scrub, &jit_skips);
        }

        // WHERE CLASS UNLOADING IS (F-19, and it is not here).
        //
        // A 2026-09-02 review reported "no class unloading is wired to the G1
        // mark cycle", from `grep class_unloading gc/src/g1.rs` returning
        // nothing. It IS wired, and the grep could not have found it: the real
        // unload transaction lives in `vm/`, driven from
        // `interpreter::gc_and_alloc::g1_remark_process_references` — the final
        // remark, which is the only point in a cycle where a weak reference to a
        // dead OLD-region referent can be observed dead — and from
        // `process_references_after_gc` on the post-pause path. Each calls
        // `classloader::gc_reconcile_defining_loaders` and then
        // `memory::gc::unload_dead_class_metadata`.
        //
        // `gc/src/class_unloading.rs` is NOT that. Its module header says so:
        // no caller anywhere in the workspace, every table permanently empty,
        // kept as scaffolding. Do not wire it in without reading it.
        //
        // Left here because this is where the question gets asked from, and a
        // grep that finds nothing reads as an answer.
        // `vm/tests/g1_class_unloading_wired.rs` is the tripwire.

        // gci i17/nr (R2) -- the dead humongous spans the reclaim below will
        // free, predicted with `reclaim_dead_humongous_spans_locked`'s own
        // predicate (a `HumongousStart` with `live_bytes == 0` whose extent is
        // all `HumongousContinuation`, no region of it JNI- or JIT-pinned), so
        // that the freed-target scrub runs BEFORE anything is reset and can
        // still keep a span. A dead span's start region carries the rset its
        // holders' edges were recorded in (gci i16/nc).
        let mut pending_spans: Vec<(usize, usize)> = Vec::new();
        if !retain_all && scrub_freed_targets && self.config.region_size > 0 {
            let region_size = self.config.region_size;
            for (i, r) in regions.iter().enumerate() {
                if r.region_type() != RegionType::HumongousStart || r.live_bytes != 0 {
                    continue;
                }
                let len = r.cursor().div_ceil(region_size).max(1);
                let Some(end) = i.checked_add(len).filter(|&end| end <= regions.len()) else {
                    continue;
                };
                let well_formed = regions[i + 1..end]
                    .iter()
                    .all(|c| c.region_type() == RegionType::HumongousContinuation);
                let pinned = regions[i..end]
                    .iter()
                    .enumerate()
                    .any(|(k, c)| c.pinned || cleanup_jit_pins.contains(&(i + k)));
                if !well_formed || pinned {
                    continue;
                }
                freed_targets.note(i, &r.rset);
                for k in i + 1..end {
                    freed_targets.freed.insert(k);
                }
                pending_spans.push((i, len));
            }
        }

        // gci i16/nc -- null every slot that names a region this cleanup
        // frees, in every region those regions' rsets named. A dead object the
        // bitmap scrub above cannot see (at or above TAMS: an evacuated copy
        // of an object already unreachable at the snapshot) otherwise keeps a
        // slot into memory the mutator reuses at once, and the next pause's
        // whole-region source walk follows it into a live object. See
        // `scrub_slots_into_freed_regions`. `CRATONVM_G1_CLEANUP_SCRUB_FREED_TARGETS=0`
        // turns it off.
        //
        // gci i17/nr (i16/nc residual R2) -- a source walk that stops at an
        // extent it cannot trust leaves the rest of that source unexamined,
        // and a dead object there may still name a candidate. Such a candidate
        // is KEPT this cycle (an over-retention: every object in it is dead,
        // the next cycle frees it) when its rset names the broken source (a
        // coarsened rset names every source). A kept candidate is itself
        // walked for slots into the candidates still freed, and if one of
        // those walks breaks the rule repeats; after `CLEANUP_RETAIN_ROUNDS`
        // rounds every candidate is kept. A candidate whose rset does not
        // name a broken source cannot be named from behind its break (the
        // remembered set is complete: every producer records the holder's
        // region), so it is freed as before.
        let mut retained_units = RegionSet::new();
        if scrub_freed_targets && !freed_targets.freed.is_empty() {
            let card_screen = true;
            let mut broken: Vec<usize> = Vec::new();
            // SAFETY: stop-the-world cleanup under the regions write guard;
            // no candidate has been reset yet.
            let mut tally = unsafe {
                self.scrub_slots_into_cleanup_freed_regions(
                    &regions,
                    &freed_targets,
                    &jit_skips,
                    &mut broken,
                )
            };
            // A unit is one in-place Old free or one span: `(first index, regions)`.
            let units: Vec<(usize, usize)> = pending_old_frees
                .iter()
                .map(|&i| (i, 1usize))
                .chain(pending_spans.iter().copied())
                .collect();
            let mut rounds = 0usize;
            while !broken.is_empty() && !freed_targets.freed.is_empty() {
                rounds += 1;
                let newly: Vec<usize> = units
                    .iter()
                    .map(|&(u, _)| u)
                    .filter(|u| {
                        !retained_units.contains(u)
                            && broken.iter().any(|&s| regions[*u].rset.names_source(s))
                    })
                    .collect();
                if newly.is_empty() {
                    break;
                }
                for &u in &newly {
                    retained_units.insert(u);
                }
                if rounds >= CLEANUP_RETAIN_ROUNDS {
                    for &(u, _) in &units {
                        retained_units.insert(u);
                    }
                }
                let mut still = RegionSet::with_region_capacity(regions.len());
                for &(u, len) in &units {
                    if !retained_units.contains(&u) {
                        for k in u..u + len {
                            still.insert(k);
                        }
                    }
                }
                freed_targets.freed = still;
                broken.clear();
                if freed_targets.freed.is_empty() {
                    break;
                }
                for &u in &newly {
                    // SAFETY: as above; `u` is an Old region or a span start,
                    // whose cursor bounds its (contiguous) objects.
                    let one = unsafe {
                        scrub_slots_into_freed_regions(
                            self,
                            &regions[u],
                            regions[u].cursor(),
                            &freed_targets.freed,
                            &jit_skips,
                            card_screen,
                        )
                    };
                    if one.breaks > 0 {
                        broken.push(u);
                    }
                    tally.add(one);
                }
            }
            let retained_regions: usize = units
                .iter()
                .filter(|(u, _)| retained_units.contains(u))
                .map(|&(_, len)| len)
                .sum();
            let sc = &self.scrub_counters;
            sc.runs.fetch_add(1, Ordering::Relaxed);
            sc.sources.fetch_add(tally.sources as u64, Ordering::Relaxed);
            sc.holders.fetch_add(tally.holders as u64, Ordering::Relaxed);
            sc.slots.fetch_add(tally.slots as u64, Ordering::Relaxed);
            sc.walk_breaks.fetch_add(tally.breaks as u64, Ordering::Relaxed);
            sc.retained.fetch_add(retained_regions as u64, Ordering::Relaxed);
            sc.marked_holders
                .fetch_add(tally.marked_holders as u64, Ordering::Relaxed);
            if self.gc_log_enabled.load(Ordering::Relaxed) {
                // gci i18/nr: ` marked_holders=N` appended at the END (every
                // field earlier greps read keeps its place).
                eprintln!(
                    "[GC] g1 cleanup freed-target-scrub: freed_regions={} sources={} \
                     coarsened={} objects={} bytes={} holders={} slots_nulled={} \
                     walk_breaks={} flagless_candidates_total={} retained_regions={} \
                     retain_rounds={} marked_holders={}",
                    freed_targets.freed.len(),
                    tally.sources,
                    freed_targets.coarsened,
                    tally.objects,
                    tally.bytes,
                    tally.holders,
                    tally.slots,
                    tally.breaks,
                    sc.evac_candidate_flagless.load(Ordering::Relaxed),
                    retained_regions,
                    rounds,
                    tally.marked_holders,
                );
            }
        }

        // gci i17/nr (R2) -- commit the in-place Old frees the scrub kept
        // nothing of (with the scrub off they were reset in the loop above).
        for &idx in &pending_old_frees {
            if retained_units.contains(&idx) {
                continue;
            }
            if gc_flags().g1_dbg_reach {
                eprintln!(
                    "[g1][FREED] cleanup region={idx} cursor={:#x}",
                    regions[idx].cursor()
                );
            }
            self.old_regions_reclaimed.fetch_add(1, Ordering::Relaxed);
            regions[idx].reset(cleanup_generation);
        }

        // G1MARK-8: humongous reclaim trusts the same possibly-incomplete
        // closure — skip it under either fail-safe (G1AUD-3 adds the
        // undrained-gray-set case to the implausible-header one). gci i17/nr:
        // a span the freed-target scrub kept is passed in the keep set, which
        // the reclaim honours as it honours a JIT pin.
        if !retain_all {
            let keep_with_retained;
            let keep: &RegionSet = if retained_units.is_empty() {
                &cleanup_jit_pins
            } else {
                let mut k = cleanup_jit_pins.clone();
                k.extend(retained_units.iter());
                keep_with_retained = k;
                &keep_with_retained
            };
            self.reclaim_dead_humongous_spans_locked(&mut regions, keep);
        }

        // gci i39/cv -- every slot that still names a region this cleanup
        // reset (in place or as a dead humongous span).
        if let Some(pre) = freed_slot_pre.as_ref() {
            let _ = freed_slot_audit(self, &regions[..], pre, "cleanup", None);
        }

        // G1MAT-4: prune remembered-set entries naming regions that are now
        // Free. Nothing else in the collector ever removes an rset entry —
        // `RememberedSet::clear` only runs on the TARGET region's own
        // `reset()` — so without this a source index recorded once is kept for
        // the rest of that target's life. Cleanup is the right place: it runs
        // once per mark cycle, already holds the regions lock, and the Free
        // set is final here (both the in-place Old frees above and the
        // humongous reclaim have completed).
        //
        // Safety: a `Free` region holds no live object, so it cannot be the
        // holder of a live cross-region edge — dropping it can only remove
        // work, never hide a reachable referent. The scan side already skips
        // Free sources (`scan_source_region_for_cset_refs`), so this changes
        // no collection decision; it bounds memory and per-pause iteration.
        // If such a region is later re-typed and stores a cross-region
        // reference, the mutator post-write barrier re-adds it, and the
        // Phase-4 rebuild re-derives GC-internal edges.
        //
        // G1AUD-5 (defect G1-8) — the Free test alone is not enough. A source
        // that was freed and then RE-TYPED (the common case: a young pause
        // recycles a region and the next allocation claims it as Eden) is not
        // Free at cleanup time, so its entry survived forever, and every pause
        // re-walked that region wholesale on behalf of an object that had been
        // zero-filled cycles ago — the "undead" entry. Each entry now carries
        // the generation it was recorded in and each region the generation it
        // was last recycled in, which makes the sharper test available here.
        {
            // `(is_free, recycled_in_generation)` per region index, snapshotted
            // so the retain closures below do not re-borrow `regions`.
            let source_state: Vec<(bool, u64)> = regions
                .iter()
                .map(|r| {
                    (
                        r.region_type() == RegionType::Free,
                        r.recycled_in_generation(),
                    )
                })
                .collect();
            for region in regions.iter() {
                region.rset.retain_sources_in_generation(|src, generation| {
                    match source_state.get(src) {
                        // Free: holds nothing, so it holds no live edge.
                        // Recycled since the edge was recorded: the holder
                        // was zero-filled by `reset`. Either way the entry
                        // is dead. `RSET_GENERATION_PINNED` entries (no
                        // generation available at record time) survive the
                        // second test by construction.
                        Some(&(is_free, recycled_in)) => !is_free && generation >= recycled_in,
                        // Out of range — can never be walked.
                        None => false,
                    }
                });
            }
        }

        // Publish the remembered-set size gauge for G1. Until now
        // `remembered_set_bytes` described only the generational card table, so
        // `rset_bytes_per_live_byte` read as zero under `-XX:+UseG1GC` — the
        // reconciliation item left open by `tlab-and-card-audit.md`
        // §2.3. Measured here (once per mark cycle, after the prune) rather
        // than per pause: this is the point at which the set is smallest and
        // final, and it costs one lock per region on a path that just walked
        // every region anyway.
        //
        // G1AUD-5: one entry is now `(source_index, generation)`, so the
        // per-entry cost is `usize + u64`, not `usize`. Keeping the gauge in
        // step with the representation matters more than the absolute number:
        // `rset_bytes_per_live_byte` is the measurement that says whether the
        // remembered set needs a real bound (audit §9 item 5), and a gauge that
        // silently under-reports by a third would answer that question wrong.
        const RSET_ENTRY_BYTES: usize = std::mem::size_of::<usize>() + std::mem::size_of::<u64>();
        let rset_sources_total: usize = regions.iter().map(|r| r.rset.source_count()).sum();
        cratonvm_gc_core::gc_metrics::record_remembered_set_bytes(
            (rset_sources_total * RSET_ENTRY_BYTES) as u64,
        );
        let pinned_regions = regions.iter().filter(|r| r.pinned).count();

        // Refresh the IHOP occupancy statistic NOW: cleanup just freed Old
        // regions and dead humongous spans, and leaving the pre-cleanup sum
        // in place until the next evacuation pause lets
        // `g1_should_start_marking` immediately re-fire a pointless
        // back-to-back mark cycle against stale occupancy.
        self.recompute_old_gen_bytes(&regions);
        // LANE W2-C — and NOW the cycle's outcome is knowable: the occupancy
        // either side of that republish is exactly "what this cycle got back".
        // Recorded here rather than at the end of the function because this is
        // the only point at which both numbers exist; `check_ihop` reads the
        // pair to decide whether another cycle against the same heap could
        // reach any other verdict. See `note_mark_cycle_outcome`.
        self.note_mark_cycle_outcome(
            old_bytes_before_reclaim,
            self.old_gen_bytes.load(Ordering::Relaxed),
        );
        self.publish_occupancy_metrics(&regions);
        // G1AUD-9 — and the same is true of the Free-region cache `needs_gc`
        // reads. Cleanup is the ONE reclamation path that does not run inside
        // `collect_garbage` (the VM drives it directly from
        // `g1_final_remark_and_cleanup`), so nothing else republishes for it:
        // the in-place Old frees and the dead-humongous reclaim above could
        // hand back a large fraction of the heap and `needs_gc` would go on
        // answering from the pre-cleanup count for up to
        // `NEEDS_GC_RECOUNT_INTERVAL` allocations — asking for a collection
        // that has nothing to collect (the young CSet was just evacuated),
        // right after the cycle whose entire purpose was to relieve the
        // pressure. The count is free here: we are already holding the lock and
        // have already walked every region.
        self.publish_region_census(&regions);
        self.reset_free_scan_hint();
        // G1CORE-6: entries whose canonical object was freed in place above.
        self.purge_dead_string_dedup_entries(&regions);

        // Audit fix (HIGH-3): clear any stragglers from the gray set and
        // deactivate the SATB write barrier — the cycle is fully done.
        self.clear_gray_set();
        // Round-9 gc HIGH-5: clear the overflow indicator so the next
        // cycle starts in a clean state. Captured on the way out so the cycle
        // record can name it: `concurrent_mark_step` clears the flag when it
        // runs its recovery rescan, so a `true` here means an overflow was
        // still OUTSTANDING at cleanup (the rescan never ran), which is the
        // case worth reporting.
        let overflow_outstanding = self.mark_worklist_overflowed.swap(false, Ordering::Relaxed);
        // INT-8: referent-slot hiding ends with the cycle.
        // Length stored BEFORE the clear, so a marker racing the two lines
        // COULD see zero over the still-populated set (gci i14/gq G-5: the
        // old comment said it never could). That direction only traces a
        // referent -- over-retention, never under-marking -- and no marker
        // runs here: the controller was joined (or never spawned) first.
        self.reference_skip_len.store(0, Ordering::Release);
        self.reference_skip.lock().clear();
        // G1AUD-2 (lane C) — LEAVE THE MARKING-ACTIVE PHASE FIRST.
        //
        // These two statements used to be in the other order, which is
        // precisely the ordering defect G1-4 recorded against
        // `abort_concurrent_mark`: between `deactivate_and_drain()` and the
        // phase store there is a window in which `is_marking_active()` is true
        // while the queue is INACTIVE, so a reference store in it reads its old
        // slot value (the phase gate admits it) and then discards it (the queue
        // gate rejects it) — a silently lost snapshot edge, and a `debug_assert!`
        // abort inside a GC pause if `satb_pre_barrier_required` is consulted
        // there.
        //
        // The audit's reason for tolerating it here was that "cleanup runs with
        // the phase already at ConcurrentSweep", which is true of the ONE
        // production driver (`VmHeap::g1_final_remark_and_cleanup` sets it
        // immediately before the call) and of nothing else: the sibling
        // teardown driver does the same, every unit test that drives a cycle by
        // hand does not, and `cleanup` is a `pub fn`. That is an unenforced
        // caller precondition guarding an invariant the crate asserts on — the
        // same shape as G1-3, which this function already refuses to trust its
        // caller about a dozen lines above. Ordering it correctly here costs
        // one statement and makes the invariant hold for every caller. The
        // driver's own `set_phase(Idle)` after this call stays idempotent.
        //
        // gc-common w5-e — history now: the barrier gates on the QUEUE
        // (`satb_pre_barrier_required`), so the half-open window described
        // above can no longer occur and its `debug_assert!` is gone. The order
        // is kept so the phase never claims marking with a closed queue.
        self.gc_state.set_phase(ConcurrentGcPhase::Idle);
        // Round-5 CRIT #4: close the SATB barrier with a drain-then-flip
        // protocol so no mutator log push that observed the gate as
        // active can be stranded after the cycle ends. The drained
        // stragglers are discarded — the mark cycle is complete and
        // anything not yet marked is correctly dead; the next cycle
        // will re-discover live state from roots. (We are at the very
        // end of the cycle so missing a few late SATB entries is
        // semantically fine.)
        //
        // LANE W2-C — and the `Vec` that discard names is one this pause was
        // paying for and throwing away. `deactivate_and_drain` begins with
        // `flush_all_thread_satb_buffers`, which takes the global SATB buffer
        // registry's mutex, upgrades and locks every registered mutator
        // thread's buffer, moves its entries into the shards, then walks the
        // orphan list doing the same — inside this STW pause, to build a result
        // dropped on the next line. `deactivate_and_discard` runs the identical
        // state machine (ACTIVE→DRAINING, visit every bucket, clear every shard
        // under its own lock, INACTIVE) without ever concatenating. Behind
        // `CRATONVM_G1_CLEANUP_SATB_DISCARD` (opt-in); the A/B to run is
        // cleanup pause time on a many-threaded workload.
        if gc_flags().g1_cleanup_satb_discard {
            self.satb_queue.deactivate_and_discard();
        } else {
            let _stragglers = self.satb_queue.deactivate_and_drain(stw);
        }

        // Round-9 HIGH-1: Release publishes marking_complete=true so any
        // subsequent RSet writes / mutator-side reads of the flag observe
        // all of the prior cycle's effects (gray set drained, SATB queue
        // deactivated, phase set to Idle). Paired with Acquire load in
        // `needs_mixed_gc` and any other consumer.
        // gci i10/rh finding 2 (CRATONVM_G1_MIXED_HOTSPOT_CHOICE): HotSpot's per-pause
        // minimum, and a phase bounded by the candidate count (every mixed pause takes at
        // least one candidate and frees it) instead of `mixed_gc_count_target` pauses; the
        // phase ends earlier through `mixed_phase_has_work` (no candidate, or the floor).
        // g1ep7 (`CRATONVM_G1_MIXED_PHASE_BOUND`, default ON): bounded instead by
        // `ceil(candidates / min_old)` pauses, HotSpot's, since a retained or pinned
        // candidate is not freed (`mixed_choice::mixed_phase_pause_bound`).
        // gci i22/sz (opt-in `CRATONVM_G1_YOUNG_RESERVES_MIXED_MIN_OLD`): the
        // candidate census the young model reserves the minimum old CSet from.
        let mixed_reserve = g1_young_reserves_mixed_min_old();
        if mixed_reserve {
            self.note_mixed_reserve_census(&regions);
        }
        let mixed_budget = if self.mixed_hotspot_choice {
            let threshold = self.config.mixed_gc_live_threshold_percent as usize;
            let candidates = regions
                .iter()
                .filter(|r| {
                    r.region_type() == RegionType::Old
                        && !r.pinned
                        && r.live_bytes > 0
                        && r.mixed_live(true) * 100 < region_size * threshold
                })
                .count();
            self.mixed_min_old.store(
                mixed_choice::min_old_regions(candidates, self.config.mixed_gc_count_target),
                Ordering::Relaxed,
            );
            {
                mixed_choice::mixed_phase_pause_bound(candidates, self.config.mixed_gc_count_target)
            }
        } else {
            self.config.mixed_gc_count_target as u64
        };
        // gci i11/go (finding 9): the next pause is `Prepare Mixed` on the
        // switch arm. Stored before the Release publish below, so a pause that
        // sees `marking_complete` sees the flag.
        self.prepare_mixed_pending
            .store(self.prepare_mixed, Ordering::Relaxed);
        // gci i18/mx (orchestrator-applied): fresh mark data opens a fresh
        // mixed phase for the per-cycle breaker restart, even if the previous
        // phase never closed through `end_mixed_phase`.
        self.mixed_futile_phase_polled.store(false, Ordering::Relaxed);
        self.marking_complete.store(true, Ordering::Release);
        self.mixed_gc_remaining.store(mixed_budget, Ordering::Relaxed);
        // gci i22/sz: HotSpot re-sizes the young generation when the phase
        // turns mixed (the record that ends the young-only phase sizes with
        // `calculate_desired_eden_length_before_mixed`); here the phase turns
        // at cleanup, so the first mixed pause's Eden is sized now, not by the
        // young record before the mark finished. Model arm only.
        if mixed_reserve && self.young_target_hotspot_model && gc_flags().g1_young_pause_target {
            let (min, max) = self.young_region_bounds();
            self.store_predicted_young_target(min, max, true);
        }

        // F-15 — close the adaptive-IHOP measurement window and re-plan the
        // next cycle's start from what this one measured.
        //
        // Here rather than earlier in `cleanup` because the duration should
        // cover the whole cycle including this pause.
        //
        // LANE W2-C — the second reason this block used to give ("the
        // old-generation growth must be read AFTER `recompute_old_gen_bytes`
        // … NET of what the cycle got back") was the defect. Freeing at the end
        // of a cycle does not retroactively give the marker headroom it did not
        // have, so the number the model wants is the GROSS rise during the
        // cycle — sampled at the top of this function into
        // `mark_cycle_gross_old_bytes` and consumed by `note_mark_cycle_end`
        // under `CRATONVM_G1_IHOP_GROSS_GROWTH`.
        if gc_flags().g1_adaptive_ihop {
            self.note_mark_cycle_end();
        } else {
            // The legacy arm still has to close the window, or the next cycle's
            // `note_mark_cycle_start` measures from the wrong instant should the
            // flag be flipped mid-run by a test. Same for the gross sample: a
            // sample belongs to the cycle it was taken in and to no other.
            *self.mark_cycle_start.lock() = None;
            self.mark_cycle_gross_old_bytes
                .store(usize::MAX, Ordering::Relaxed);
        }

        // LANE W2-C — the cycle's TAMS dies with the cycle.
        //
        // `abort_concurrent_mark` clears both `mark_start` and
        // `mark_start_snapshot` and says why: a stale TAMS would let a later
        // `cleanup` driven outside a cycle treat post-snapshot bytes as
        // implicitly live "on the strength of a cycle that never finished".
        // `cleanup` cleared neither, so a second bare `cleanup()` with no
        // intervening `start_concurrent_mark` ran against the previous cycle's
        // (complete, so not unsound) bitmap and TAMS. The direction is safe
        // today — a region recycled since the snapshot fails the epoch/type
        // match and is treated as wholly live — but it is state outliving the
        // cycle that defines it, which is the shape this module keeps getting
        // bitten by.
        //
        // PLACEMENT is the whole of the care this needs. It must land after
        // every consumer of the snapshot in this pause:
        //   * the per-region liveness verdicts above, which call `region.tams()`;
        //   * `reclaim_dead_humongous_spans_locked`, which reads the bitmap;
        //   * `is_live_after_mark`, which the VM's reference processing calls
        //     BEFORE `cleanup` (see `VmHeap::g1_final_remark_and_cleanup`), so
        //     it is already done by the time control reaches here.
        // And it must NOT clear `marked_bytes_below_tams` the way the abort
        // path does: that accumulator is what mixed GC's region selection is
        // sorted on, and mixed collections run AFTER this cleanup, not before.
        // An abort discards its bitmap, so it has no such consumer.
        //
        // Behind `CRATONVM_G1_CLEANUP_RESET_TAMS` (opt-in). The A/B to watch is
        // `cleanup_frees_a_zero_live_old_region_when_the_closure_is_complete`
        // followed by a second bare `cleanup(&stw())`: with the reset armed the
        // second one must free nothing and must not warn.
        if gc_flags().g1_cleanup_reset_tams {
            for r in regions.iter_mut() {
                r.mark_start = None;            }
            self.mark_start_snapshot.lock().clear();
        }

        // F-16 — and give back the pages of any trailing run of Free regions.
        // Last, after every free this cycle performs: the humongous reclaim and
        // the in-place Old frees above are exactly what create such a run.
        let uncommitted = self.uncommit_trailing_free_regions(&regions);
        if uncommitted > 0 {
            tracing::debug!(
                "g1 cleanup: returned {} bytes to the OS ({} committed remain)",
                uncommitted,
                self.arena.committed_len()
            );
        }

        // State what this cleanup decided, including any fail-safe it took.
        // Without this an operator watching G1 fail to reclaim old gen cannot
        // tell "the closure was abandoned" from "there is no garbage".
        let mut degraded = cratonvm_gc_core::gc_metrics::g1_degraded::NONE;
        if saw_implausible {
            degraded |= cratonvm_gc_core::gc_metrics::g1_degraded::MARK_IMPLAUSIBLE_HEADER;
        }
        if closure_incomplete {
            degraded |= cratonvm_gc_core::gc_metrics::g1_degraded::CLEANUP_CLOSURE_INCOMPLETE;
        }
        if overflow_outstanding {
            degraded |= cratonvm_gc_core::gc_metrics::g1_degraded::MARK_WORKLIST_OVERFLOW;
        }
        if pinned_regions > 0 {
            degraded |= cratonvm_gc_core::gc_metrics::g1_degraded::JNI_PINNED_REGIONS_EXCLUDED;
        }
        cratonvm_gc_core::gc_metrics::record_g1_cycle(
            cratonvm_gc_core::gc_metrics::g1_cycle_kind::CONCURRENT_CLEANUP,
            0,
            0,
            pinned_regions as u32,
            rset_sources_total as u32,
            degraded,
        );

        // --- lane E resize hook ---
        //
        // Run the adaptive heap-sizing policy against the census cleanup has
        // just earned. A concurrent-mark cleanup is the other moment at which a
        // large amount of memory can come back at once — the in-place Old frees
        // and the dead-humongous reclaim above — and unlike an evacuation pause
        // it holds a fresh whole-heap view, so the occupancy the policy decides
        // on here is exact rather than inherited from a per-pause census.
        //
        // Last in the function, after the F-16 shrink above, on purpose: that
        // call releases the trailing Free run against the OLD floor, and this
        // one may lower the floor and release more. Doing it in the other order
        // would mean the target dropped at this cleanup and nothing acted on it
        // until the next one, which for a workload that crosses IHOP rarely is
        // the same "holds its high-water mark for life" the finding is about.
        //
        // No-op unless `CRATONVM_G1_HEAP_RESIZE` is armed. See
        // `consider_heap_resize_at_cleanup`.
        let resized = self.consider_heap_resize_at_cleanup(&regions);
        if resized > 0 {
            tracing::info!(
                "[GC] g1 heap-resize: cleanup returned {resized} bytes to the OS \
                 (target_bytes={} committed={})",
                self.heap_target_bytes(),
                self.committed_bytes(),
            );
        }
        // gci i29/cv2 — opt-in `CRATONVM_G1_DBG_FLAGLESS_SWEEP`: what
        // remark/cleanup (the in-place frees and the freed-target scrub) left.
        self.flagless_header_sweep(&regions, "cleanup-end", None);
        // LANE W3-C — last, so the figure is the whole pause. See
        // [`CLEANUP_PAUSES`].
        CLEANUP_PAUSES.fetch_add(1, Ordering::Relaxed);
        CLEANUP_PAUSE_US.fetch_add(pause_t0.elapsed().as_micros() as u64, Ordering::Relaxed);
    }

    /// The regions `cleanup` must not free in place this cycle because a live
    /// compiled frame pins them (round 12 wave 1, lane g1store): the same set
    /// an evacuation pause keeps out of its collection set and out of eager
    /// humongous reclaim ([`Self::jit_pinned_region_set_screened`]).
    ///
    /// Cleanup frees an Old region or a humongous span on `live_bytes == 0`,
    /// which the marker earns from ROOTS. A conservative band word that is not
    /// an object start (a derived pointer, or any word of a humongous span but
    /// its base) is never a mark root, so the object it points into can be
    /// unmarked and freed at cleanup while that frame is still using it. Its
    /// PIN reaches this pause's snapshot (`memory::roots::collect_roots`
    /// publishes the rejects' pins since this wave), so honouring the pin set
    /// here closes that gap the way the pause paths already do. Retaining a
    /// region a stale word pins costs one cycle's reclamation of it.
    ///
    /// Since round 12 wave 2 `remark` also MARKS the object each pin names
    /// ([`Self::jit_pin_mark_seeds`]), so its referents are marked too; this
    /// set remains the backstop for a pin the grid could not place.
    ///
    /// Empty under `CRATONVM_G1_CLEANUP_HONOURS_JIT_PINS=0` (default ON).
    ///
    /// Blocked peers' captured stack words are left out under
    /// [`g1_cleanup_ignores_peer_stack_words`] (default ON since ngr, `=0` adds
    /// them back). Their pin (`g1_pins_peer_stack_words`) exists so that
    /// nothing MOVES under a word the blocked-wake fold might rewrite; cleanup
    /// only frees what the mark left unmarked, which writes no forwarding for
    /// the fold to apply. With them in, one stale word in a parked Reference
    /// Handler's frame (an old TLAB bound, landing inside a continuation
    /// region) kept a dead 54 MB humongous array of `NativeGrowthReclaimProbe`
    /// through every cleanup and full collection, and the next round threw
    /// `OutOfMemoryError` with 88 MB of humongous spans live (page
    /// `native-growth-reclaim-probe-fails-on-g1`).
    pub fn cleanup_jit_pinned_regions(&self, regions: &[G1Region]) -> RegionSet {
        {
            self.jit_pinned_region_set_screened(
                None,
                regions,
                !g1_cleanup_ignores_peer_stack_words(),
            )
            .0
        }
    }

    /// gci i16/nc -- [`scrub_slots_into_freed_regions`] over every region the
    /// freed regions' remembered sets named (every region when one of them was
    /// coarsened). A continuation index names its span's start; a freed or
    /// Free region is skipped.
    ///
    /// gci i17/nr: every walked region whose walk stopped at an extent it
    /// could not trust is pushed onto `broken` (a span by its start index, the
    /// index its holders' edges are recorded under), so `cleanup` can keep the
    /// candidates such a region may still name.
    ///
    /// # Safety
    /// Stop-the-world cleanup; the caller holds the regions write guard
    /// `regions` was borrowed from.
    pub unsafe fn scrub_slots_into_cleanup_freed_regions(
        &self,
        regions: &[G1Region],
        freed: &CleanupFreedTargets,
        jit_skips: &[(usize, usize)],
        broken: &mut Vec<usize>,
    ) -> FreedTargetScrub {
        let card_screen = true;
        let mut walk = RegionSet::with_region_capacity(regions.len());
        if freed.coarsened {
            for i in 0..regions.len() {
                walk.insert(i);
            }
        } else {
            for s in freed.sources.iter() {
                if s >= regions.len() {
                    continue;
                }
                walk.insert(humongous_span_start(regions, s).unwrap_or(s));
            }
        }
        // JIT round 23 wave 3, lane L4 (the young-source prerequisite): with
        // the young-holder barrier filter on, a young holder's store recorded
        // no edge and no card, so the freed regions' rsets cannot name it.
        // Every young region is scrubbed, and unscreened (its cards say
        // nothing), so no dead young object keeps a slot into a freed region
        // for a later pinned-region walk to trust.
        let young_unscreened = g1_young_source_filter_enabled();
        if young_unscreened {
            for (i, r) in regions.iter().enumerate() {
                if matches!(r.region_type(), RegionType::Eden | RegionType::Survivor) {
                    walk.insert(i);
                }
            }
        }
        let mut total = FreedTargetScrub::default();
        for idx in walk.iter() {
            if freed.freed.contains(&idx) {
                continue;
            }
            let region = &regions[idx];
            if matches!(
                region.region_type(),
                RegionType::Free | RegionType::HumongousContinuation
            ) {
                continue;
            }
            let screen = card_screen
                && !(young_unscreened
                    && matches!(
                        region.region_type(),
                        RegionType::Eden | RegionType::Survivor
                    ));
            // SAFETY: the caller's contract; the walk stays below the cursor.
            let one = unsafe {
                scrub_slots_into_freed_regions(
                    self,
                    region,
                    region.cursor(),
                    &freed.freed,
                    jit_skips,
                    screen,
                )
            };
            if one.breaks > 0 {
                broken.push(idx);
            }
            total.add(one);
        }
        total
    }

    /// Reclaim dead humongous spans after a mark cycle.
    ///
    /// Cleanup runs after whole-heap marking, so an unmarked `HumongousStart`
    /// with no pinned slice is safe to recycle along with each contiguous
    /// continuation region.
    ///
    /// This is no longer the only humongous reclaimer: since 2026-08-13
    /// evacuation pauses run [`Self::eager_reclaim_humongous_locked`], which
    /// reaches the same conclusion from the Phase-4 reference walk instead of
    /// the bitmap. The two are complementary rather than redundant — this one
    /// works during a mark cycle and after an evacuation failure, which are
    /// exactly the cases the pause path refuses.
    ///
    /// `jit_pinned` is [`Self::cleanup_jit_pinned_regions`]: a span with any
    /// region in it is kept, as a JNI-pinned span is.
    pub fn reclaim_dead_humongous_spans_locked(
        &self,
        regions: &mut [G1Region],
        jit_pinned: &RegionSet,
    ) -> usize {
        let region_size = self.config.region_size;
        if region_size == 0 {
            return 0;
        }

        let mut reclaimed = 0usize;
        let mut i = 0usize;
        while i < regions.len() {
            if regions[i].region_type() != RegionType::HumongousStart {
                i += 1;
                continue;
            }

            let total_size = regions[i].cursor();
            let regions_needed = total_size.div_ceil(region_size).max(1);
            let Some(end) = i
                .checked_add(regions_needed)
                .filter(|&end| end <= regions.len())
            else {
                i += 1;
                continue;
            };

            // G1MAT-2: validate the span before resetting anything in it. The
            // extent is derived purely from `regions[i].cursor()`, so a stale or
            // corrupt cursor on a `HumongousStart` would silently `reset()`
            // (zero-fill + retype-to-Free) regions belonging to OTHER live
            // objects. Every region a humongous span actually owns is typed
            // `HumongousContinuation` (`claim_humongous_span_locked`), so require
            // exactly that before touching them. A mismatch means the derived
            // extent disagrees with the region table: skip the span and log,
            // rather than free memory we cannot prove is ours.
            //
            // This is the same class of defect as the round-9 `HumongousFiller`
            // walker audit: a humongous invariant assumed at a call site rather
            // than checked there.
            let span_well_formed = regions[i + 1..end]
                .iter()
                .all(|r| r.region_type() == RegionType::HumongousContinuation);
            if !span_well_formed {
                tracing::warn!(
                    "g1 cleanup: humongous span at region {} claims {} regions \
                     (cursor {}) but the following regions are not all \
                     HumongousContinuation — skipping reclaim",
                    i,
                    regions_needed,
                    total_size
                );
                i += 1;
                continue;
            }

            let pinned = regions[i..end]
                .iter()
                .enumerate()
                .any(|(k, r)| r.pinned || jit_pinned.contains(&(i + k)));
            if regions[i].live_bytes == 0 && !pinned {
                reclaimed = reclaimed.saturating_add(total_size);
                let generation = self.rset_generation();
                for region in &mut regions[i..end] {
                    region.reset(generation);
                }
                i = end;
            } else {
                i += 1;
            }
        }

        reclaimed
    }

    // gci i11/gm finding 7 -- `want_humongous_census` and
    // `eager_reclaim_early_decline` were deleted here (no caller since
    // ten-findings item 1 moved the walk-width decision into
    // `phase4_regions_to_walk` and the census decision into
    // `update_references_in_regions`). Their history -- the 843-of-1024-region
    // census walk on `TestKillProcessWhileWriting` and the finalizer gate that
    // declined every pause -- is kept in `eager_reclaim_humongous_locked`'s
    // body comments below.

    /// Reclaim humongous spans that THIS PAUSE can prove nothing points at
    /// (`CRATONVM_G1_EAGER_HUMONGOUS`, default on).
    ///
    /// # Why an evacuation pause can decide this at all
    ///
    /// Humongous spans are never evacuated, so until now the only thing that
    /// ever freed one was [`Self::cleanup`] — the end of a whole concurrent
    /// mark cycle. For a program whose humongous garbage is short-lived (a
    /// `new byte[4 MiB]` per request, discarded immediately) that is the
    /// difference between reclaiming on every pause and reclaiming when IHOP
    /// happens to fire, which on a heap sized for the live set may be never.
    ///
    /// Cleanup decides liveness from the mark bitmap. A pause has no bitmap,
    /// but it does have something the bitmap is a compressed form of: Phase 4
    /// has just walked every reference slot of every non-CSet region, and Phase
    /// 5 has just freed the CSet. So immediately after Phase 5, "no walked
    /// object referenced span H, and no root does" IS "nothing in the heap
    /// references H" — provided the walk really did cover the whole live heap.
    /// Every gate below exists to establish that proviso, and each one
    /// corresponds to a way the walk could have missed a live edge:
    ///
    /// * **`census.complete`** — a Phase-4 region walk that aborted on an
    ///   implausible object size never inspected the rest of that region.
    /// * **no mark cycle in flight** — under SATB, liveness is measured against
    ///   the mark-start snapshot, and the gray set and SATB log hold raw
    ///   addresses that Phase 4 does not walk. An object unreferenced *now* may
    ///   still be snapshot-live, and freeing it unmarks its whole subtree.
    ///   (gci i11/gm finding 6: under `CRATONVM_G1_EAGER_RECLAIM_DURING_MARK` a
    ///   PRIMITIVE-array span, which has no subtree, may go while the cycle is
    ///   open; its gray entries are dropped with it. See the gate below.)
    /// * **no pending finalizer roots** — `resurrect_dead_finalizers` holds
    ///   addresses of objects that are unreachable and must nevertheless stay
    ///   allocated until `finalize()` has run.
    /// * **no evacuation failure** — a self-forwarded object stays in its CSet
    ///   region, so Phase 5 KEEPS that region. Phase 4 skips CSet regions, so a
    ///   live evacuation-failed object holding the only reference to H was
    ///   never walked. `pointer_map` containing an identity forward is the
    ///   exact signal (it is what Phase 5 itself reads to decide to keep).
    ///
    /// # The ordering this depends on
    ///
    /// It must run AFTER Phase 4, and Phase 4 must have walked the pause's
    /// to-space. Consider a young object Y in Eden holding the only reference
    /// to humongous H: the mutator barrier recorded source=Eden in H's
    /// remembered set, and this pause copies Y to Survivor and frees Eden. Ask
    /// the remembered set now and it names a region that no longer holds
    /// anything — a stale entry pointing at freed Eden — while the live
    /// Survivor copy is unaccounted for. That is why liveness here comes from
    /// the Phase-4 walk (which visits the Survivor copy, because to-space
    /// regions are typed and cursor-committed before Phase 4 runs) and NOT from
    /// the remembered set. Reversing the order, or substituting the rset for
    /// the census, frees a live H.
    ///
    /// Returns bytes reclaimed.
    pub fn eager_reclaim_humongous_locked(
        &self,
        regions: &mut Vec<G1Region>,
        roots: &[ObjectRef],
        pointer_map: &cratonvm_types::PointerMap,
        census: &HumongousCensus,
        jit_pinned: &RegionSet,
    ) -> usize {
        if !regions
            .iter()
            .any(|r| r.region_type() == RegionType::HumongousStart)
        {
            // G1AUD-9: no span exists, so there is nothing to reclaim AND
            // nothing was declined. This is also why `census.complete` may be
            // `false` here without a walk having aborted —
            // `update_references_in_regions` skips the census entirely on a
            // humongous-free heap — so the test must come BEFORE the
            // `census.complete` gate below, or every pause on such a heap would
            // record a fail-safe that never fired.
            return 0;
        }

        let declined = |code: usize, why: &str| -> usize {
            if gc_flags().g1_dbg_reach {
                eprintln!("[g1][HUMONGOUS] eager reclaim declined: {why}");
            }
            cratonvm_gc_core::gc_metrics::record_g1_eager_decline_reason(code);
            cratonvm_gc_core::gc_metrics::record_g1_eager_humongous(0, 0, true);
            0
        };

        if census.taken && !census.complete {
            return declined(
                cratonvm_gc_core::gc_metrics::eager_decline::CENSUS_INCOMPLETE,
                "a phase-4 region walk aborted, so the census under-counts live edges",
            );
        }
        let marking =
            self.gc_state.phase() != ConcurrentGcPhase::Idle || self.satb_queue.is_active();
        let gray_open = !self.gray_set_is_empty();
        // gci i11/gm finding 6 -- HotSpot's `humongous_region_is_candidate`
        // admits a `typeArray` while marking: it has no references (so freeing
        // it cannot unmark a subtree) and its metadata is built in. Under
        // `CRATONVM_G1_EAGER_RECLAIM_DURING_MARK` this pause does the same for
        // a PRIMITIVE-array span, with this collector's two extra holders
        // handled: the SATB log was drained into the gray set at the start of
        // this pause (`marking_keepalive_roots`, which every evacuation driver
        // runs first; `has_pending` must agree), and the gray entries that name
        // a doomed span are dropped below. A marker holding popped work outside
        // every queue (`mark_active`) declines the arm for this pause. A span
        // reset mid-cycle bumps its regions' `reuse_epoch`, so TAMS reads 0 and
        // whatever is later allocated there is implicitly live, as for every
        // Eden region recycled during a cycle.
        let during_mark = (marking || gray_open)
            && self.eager_reclaim_during_mark
            && self.mark_active.load(Ordering::Acquire) == 0
            && !self.satb_queue.has_pending();
        if (marking || gray_open) && self.eager_reclaim_during_mark && !during_mark {
            self.eager_mark_declined.fetch_add(1, Ordering::Relaxed);
        }
        if marking && !during_mark {
            return declined(
                cratonvm_gc_core::gc_metrics::eager_decline::MARKING_ACTIVE,
                "a concurrent mark cycle is in flight (SATB snapshot liveness applies)",
            );
        }
        if gray_open && !during_mark {
            return declined(
                cratonvm_gc_core::gc_metrics::eager_decline::GRAY_SET_NON_EMPTY,
                "the gray set is non-empty",
            );
        }
        // NO FINALIZER GATE. It used to decline the ENTIRE reclaim whenever
        // `finalizer_pause` was set, and that flag is true for ANY registered
        // not-yet-enqueued finalizable object -- one live `FileInputStream` is
        // enough -- so eager humongous reclaim never ran for the life of the
        // process. Measured on H2 `TestKillProcessWhileWriting`: 830 of 1024
        // regions humongous, Eden squeezed to 1.1 regions, 76 787 young pauses
        // per 900 s each freeing 485 KB, against 88 pauses and a PASS without
        // it. The exact shape the gate protected -- a humongous object WITH a
        // finalizer, which is never in the CSet and so never resurrected -- is
        // named in the live set below instead. See `finalizer_addrs_this_pause`
        // and `a_humongous_span_awaiting_finalization_is_not_eagerly_reclaimed`.
        if gc_flags().g1_dbg_reach && pointer_map.iter().any(|(o, n)| o == n) {
            let mut free = 0usize;
            let mut eden = 0usize;
            let mut surv = 0usize;
            let mut old_r = 0usize;
            let mut hstart = 0usize;
            let mut hcont = 0usize;
            for r in regions.iter() {
                match r.region_type() {
                    RegionType::Free => free += 1,
                    RegionType::Eden => eden += 1,
                    RegionType::Survivor => surv += 1,
                    RegionType::Old => old_r += 1,
                    RegionType::HumongousStart => hstart += 1,
                    RegionType::HumongousContinuation => hcont += 1,
                }
            }
            eprintln!(
                "[g1][EVACFAIL] regions={} free={free} eden={eden} surv={surv} old={old_r} hstart={hstart} hcont={hcont} map={}",
                regions.len(),
                pointer_map.len(),
            );
        }
        // g1sz: the tracked no-self-forward fact first; the identity search
        // walks every forward of the pause.
        if !self.no_self_forward_this_pause()
            && pointer_map
                .iter()
                .any(|(old, new)| old == new && !self.peer_object_pinned(*old))
        {
            return declined(
                cratonvm_gc_core::gc_metrics::eager_decline::EVACUATION_FAILURE,
                "evacuation failure kept cset regions phase 4 never walked",
            );
        }

        // Everything the pause can see a reference from. A wide walk took a
        // census, exact for what it walked; a narrow walk did not (item 1), and
        // the spans' remembered sets answer instead.
        let mut live = if census.taken {
            census.referenced.clone()
        } else {
            // Ten-findings item 1, HotSpot's rule: a span survives if this
            // pause SCANNED a slot into it, or if its own remembered set could
            // hold an edge this pause never scanned.
            //
            // The marks come free -- the two slot loops already resolve every
            // referent's region for the CSet test, so the added cost is a
            // region-type read and, for the rare humongous hit, one lock. The
            // rset half is a pass over region TYPES and rset summaries, with no
            // object walk at all.
            //
            // What this replaced: a walk over every remembered-set SOURCE of
            // every span, re-reading heap to ask whether an edge the rset had
            // already recorded was still there. Bounded, but the bound was
            // EAGER_RECLAIM_MAX_SOURCES regions of real object walking per
            // pause -- and a span whose walk the cap declined was marked live
            // anyway, which is the same answer the rset test gives directly.
            //
            // JIT-PINNED regions still need their walk: pinning is exactly what
            // keeps a region OUT of the collection set, so nothing scans it and
            // a young pinned region records no rset edge for a span it holds.
            // Seeded with EVERY cheap source of liveness this pause already
            // holds, not just the scan marks: a span named by a root or by a
            // pending finalizer is live on the spot, and seeding it here is
            // what stops the walk reading its sources to re-learn that. The
            // same facts are re-applied below for the census path, where they
            // are additions rather than a seed.
            // Two DIFFERENT sources, counted apart, because they answer
            // different questions. `scan_marks` is item 1's marking -- spans a
            // slot was scanned into -- and is the thing to judge the change on.
            // The roots/finalizer seeding below is a cheaper win that was
            // always available and merely applied too late (after the walk).
            // Folding them into one number hid the first behind the second.
            let scan_marks = self.take_humongous_references();
            let mut seed = scan_marks.clone();
            let mut seed_addr = |addr: usize| {
                if let Some(idx) = self.lookup_region_for_addr(addr) {
                    if let Some(span) = humongous_span_start(regions, idx) {
                        seed.insert(span);
                    }
                }
            };
            for root in roots {
                seed_addr(root.as_ptr() as usize);
            }
            for &addr in self.finalizer_addrs_this_pause.lock().iter() {
                seed_addr(addr);
            }
            for &addr in self.resurrected_finalizers.lock().iter() {
                seed_addr(addr);
            }
            for addr in self.overlay_live_addrs_this_pause() {
                seed_addr(addr);
            }
            match self.humongous_spans_referenced_by_rset(regions, jit_pinned, seed, &scan_marks) {
                Some(set) => set,
                None => return declined(
                    cratonvm_gc_core::gc_metrics::eager_decline::SOURCE_WALK_ABORTED,
                    "a remembered-set source walk aborted, so rset-derived liveness under-counts",
                ),
            }
        };
        let mut note_addr = |live: &mut RegionSet, addr: usize| {
            if let Some(idx) = self.lookup_region_for_addr(addr) {
                if let Some(span) = humongous_span_start(regions, idx) {
                    live.insert(span);
                }
            }
        };
        for root in roots {
            note_addr(&mut live, root.as_ptr() as usize);
        }
        // EVERY object registered for finalization this pause, at the address
        // it had when the pause started. This is what replaced the wholesale
        // `finalizer_pause` decline, and it is aimed at the one shape that
        // decline existed for: a HUMONGOUS object with a finalizer. Such an
        // object is unreachable (that is why it is being finalized), is never
        // in the CSet (humongous spans are not evacuated), and therefore never
        // reaches `resurrected_finalizers` — so nothing else in this function
        // would name it live and the span would be freed out from under a
        // `finalize()` that has not run.
        //
        // A dead finalizable object that merely REFERENCES a humongous span
        // needs nothing extra: the census counts references from dead holders
        // too (see `HumongousCensus::referenced`, "over-approximates
        // liveness"), and a resurrected one is walked at its post-copy address
        // by the loop below.
        for &addr in self.finalizer_addrs_this_pause.lock().iter() {
            note_addr(&mut live, addr);
        }
        // Post-copy addresses of objects resurrected for finalization this
        // pause: unreachable by definition, and exactly why they need naming.
        for &addr in self.resurrected_finalizers.lock().iter() {
            note_addr(&mut live, addr);
        }
        // Collection-overlay values this pause kept live: an overlay row is
        // not a heap slot, so a humongous array held only by one (a large
        // `TreeMap`'s backing `data`) is named by nothing above.
        for addr in self.overlay_live_addrs_this_pause() {
            note_addr(&mut live, addr);
        }
        // A conservatively-discovered JIT root can name a region without naming
        // an object (that is the whole reason this set exists), so treat any
        // humongous span it covers as referenced.
        for idx in jit_pinned.iter() {
            if let Some(span) = humongous_span_start(regions, idx) {
                live.insert(span);
            }
        }

        // Select the doomed spans before mutating anything.
        let region_size = self.config.region_size;
        if region_size == 0 {
            return 0;
        }
        let mut doomed: Vec<(usize, usize, usize)> = Vec::new(); // (start, end, bytes)
        let mut i = 0usize;
        while i < regions.len() {
            if regions[i].region_type() != RegionType::HumongousStart {
                i += 1;
                continue;
            }
            let total_size = regions[i].cursor();
            let regions_needed = total_size.div_ceil(region_size).max(1);
            let Some(end) = i
                .checked_add(regions_needed)
                .filter(|&end| end <= regions.len())
            else {
                i += 1;
                continue;
            };
            // Same span validation cleanup applies (G1MAT-2): the extent comes
            // from a cursor, so refuse to reset regions the region table does
            // not agree belong to this span.
            let well_formed = regions[i + 1..end]
                .iter()
                .all(|r| r.region_type() == RegionType::HumongousContinuation);
            let pinned = regions[i..end]
                .iter()
                .enumerate()
                .any(|(k, r)| r.pinned || jit_pinned.contains(&(i + k)));
            // gci i11/gm finding 6 -- while a mark cycle is open, only a
            // primitive array (HotSpot's `typeArray`) may go.
            let eligible = !during_mark
                || (total_size >= HEADER_SIZE
                    && span_holds_primitive_array(regions[i].data.as_ptr() as usize));
            if well_formed && !pinned && eligible && !live.contains(&i) {
                doomed.push((i, end, total_size));
                i = end;
            } else {
                i += 1;
            }
        }

        if doomed.is_empty() {
            cratonvm_gc_core::gc_metrics::record_g1_eager_humongous(0, 0, false);
            return 0;
        }

        // gci i11/gm finding 6 -- a gray entry naming a doomed span would name
        // freed (and later reused) memory; drop it with the span. Stop-the-world
        // under the regions write guard, the protocol
        // `remap_gray_set_after_pause` relies on.
        if during_mark {
            let ranges: Vec<(usize, usize)> = doomed
                .iter()
                .map(|&(start, end, _)| {
                    let lo = regions[start].data.as_ptr() as usize;
                    (lo, lo + (end - start) * region_size)
                })
                .collect();
            let dropped = self.drop_gray_entries_in_ranges(&ranges);
            self.eager_mark_gray_dropped
                .fetch_add(dropped as u64, Ordering::Relaxed);
            self.eager_mark_spans
                .fetch_add(doomed.len() as u64, Ordering::Relaxed);
            self.eager_mark_bytes.fetch_add(
                doomed.iter().map(|&(_, _, b)| b as u64).sum::<u64>(),
                Ordering::Relaxed,
            );
        }

        // An independent oracle, debug builds only. The census is derived from
        // the Phase-4 walk and therefore shares its region FILTER (skip CSet,
        // skip Free) — which is where the subtle mistakes live, and where the
        // evacuation-failure hole above would have been. This re-derives the
        // answer after Phase 5 over every region that is not Free, so a span
        // held only by a region the filter wrongly excluded fails here rather
        // than becoming a use-after-free in a shipped build.
        #[cfg(debug_assertions)]
        {
            let condemned: RegionSet = doomed
                .iter()
                .flat_map(|&(start, end, _)| start..end)
                .collect();
            self.debug_assert_no_reference_into_spans(regions, roots, &condemned);
        }

        let generation = self.rset_generation();
        let mut bytes = 0usize;
        let spans = doomed.len();
        for (start, end, total_size) in doomed {
            if gc_flags().g1_dbg_reach {
                eprintln!(
                    "[g1][HUMONGOUS] eager reclaim span start={start} regions={} bytes={total_size}",
                    end - start
                );
            }
            bytes = bytes.saturating_add(total_size);
            for region in &mut regions[start..end] {
                region.reset(generation);
            }
        }
        cratonvm_gc_core::gc_metrics::record_g1_eager_humongous(spans as u64, bytes as u64, false);
        bytes
    }

    /// Item 1 — record that a slot scanned this pause points into `idx`.
    ///
    /// The type test is inline and the lock is behind it: a pause scans
    /// millions of slots and only a handful land in a span.
    pub fn note_humongous_reference(&self, regions: &[G1Region], idx: usize) {
        // DEFAULT OFF, and the flag is checked BEFORE the region table is
        // touched. `region_for_ptr` is pure address arithmetic and
        // `cset.contains` is a bitset, so after item 4 the slot loop reads the
        // region table not at all for a non-CSet referent -- and this
        // `region_type` read would put back exactly the cold line per slot that
        // item 4 removed, on the old->old path that dominates a source walk.
        //
        // Measured 2026-09-05, every workload and every run: `marked_live=0`.
        // The mechanism fires (see
        // `a_span_referenced_from_a_scanned_young_object_is_marked_not_walked`),
        // but the holders of humongous spans are old objects the pause has no
        // other reason to scan, which is the same fact that makes the
        // remembered-set walk necessary. Paying a per-slot cache miss for an
        // answer that never arrives is the wrong trade, so this is opt-in until
        // a workload is found where young objects hold humongous arrays.
        if !gc_flags().g1_humongous_marks {
            return;
        }
        let is_humongous = matches!(
            regions.get(idx).map(|r| r.region_type()),
            Some(RegionType::HumongousStart) | Some(RegionType::HumongousContinuation)
        );
        if !is_humongous {
            return;
        }
        if let Some(span) = humongous_span_start(regions, idx) {
            self.humongous_referenced.lock().insert(span);
        }
    }

    /// Item 1 — start a pause with an empty mark set.
    pub fn clear_humongous_references(&self) {
        self.humongous_referenced.lock().clear();
    }

    /// Item 1 — the spans this pause actually scanned a slot into.
    pub fn take_humongous_references(&self) -> RegionSet {
        std::mem::replace(&mut *self.humongous_referenced.lock(), RegionSet::new())
    }

    /// Ten-findings item 1 — which humongous spans some object outside the CSet
    /// still references, answered from the spans' REMEMBERED SETS rather than
    /// from a whole-heap walk.
    ///
    /// Every cross-region edge into a span's start region is recorded there: by
    /// the mutator post-write barrier, by the Phase-4 rebuild for the regions
    /// it walks (which now includes humongous targets), and by the
    /// evacuation-failure fix-up. An entry names a SOURCE REGION, and the
    /// source's object may have died since it stored the edge, so an entry is
    /// evidence, not a verdict: the source is walked — card-screened, because
    /// every recording path dirties the holder's card and a card is cleared
    /// only when its region is reset — and the span is live iff a reference
    /// into it is actually found. A span whose rset is coarsened, or that names
    /// more than [`EAGER_RECLAIM_MAX_SOURCES`] live sources, is simply retained
    /// until the next mark cycle's cleanup, which is the shape of HotSpot's
    /// `G1EagerReclaimRemSetThreshold`. JIT-pinned regions are walked wholesale
    /// for the reason Phase 2 walks them wholesale: a compiled store there is
    /// not assumed to have taken the barrier.
    ///
    /// A source is walked once even when several spans name it, since one walk
    /// notes every span the source references. Returns `None` if a walk broke
    /// on an object it could not size — the refusal a wide census reports as
    /// `complete == false`.
    pub fn humongous_spans_referenced_by_rset(
        &self,
        regions: &[G1Region],
        jit_pinned: &RegionSet,
        mut live: RegionSet,
        scan_marks: &RegionSet,
    ) -> Option<RegionSet> {
        let mut walked = RegionSet::new();
        let mut sources: Vec<usize> = Vec::new();
        let prune = lane_a_rset_prune_on_scan();
        // Item 1's engagement census: spans the marks decided, against source
        // regions the walk still had to read. See `record_g1_humongous_liveness`.
        let mut marked_live = 0u64;
        // The SEED, captured before the loop can add to `live`. Counting
        // `live.contains(i)` directly conflated a seeded span with one an
        // earlier span's walk had just inserted (`note_humongous_targets_in_region`
        // writes into the same set), which made the census report marking
        // engagement in a run with the marking switched OFF.
        let seeded = live.clone();
        let mut seeded_by_roots = 0u64;
        for (i, r) in regions.iter().enumerate() {
            if r.region_type() != RegionType::HumongousStart {
                continue;
            }
            // Item 1's marking half: this pause already scanned a slot into the
            // span, so it is live on evidence the pause had to look at anyway.
            // No remembered set to consult, and no source to walk.
            if seeded.contains(&i) {
                if scan_marks.contains(&i) {
                    marked_live += 1;
                } else {
                    seeded_by_roots += 1;
                }
                continue;
            }
            if live.contains(&i) {
                continue;
            }
            if r.rset.is_coarsened() {
                live.insert(i);
                continue;
            }
            sources.clear();
            // LANE A — prune the stale entries here too, for the reason
            // `live_rset_sources` gives: a humongous span is long-lived by
            // construction, so its remembered set is the one that accumulates
            // dead entries the longest, and the `sources.len() >
            // EAGER_RECLAIM_MAX_SOURCES` bail-out below is decided on that
            // count. A span retained because its rset still NAMES sixty
            // recycled regions is a megabyte the heap keeps for bookkeeping
            // nobody would have acted on.
            //
            // Note which criterion prunes and which only skips. `Free` is a
            // SKIP, not a delete: a `RSET_GENERATION_PINNED` entry (recorded by
            // a caller with no generation to hand) is never stale by design and
            // must survive a moment when its source happens to be Free, or the
            // fail-safe that constant exists to provide is silently undone.
            let live_sources = if prune {
                r.rset.retain_and_collect_sources(|source, generation| {
                    !Self::rset_entry_is_stale(regions, source, generation)
                })
            } else {
                r.rset
                    .sources_with_generations()
                    .into_iter()
                    .filter(|&(source, generation)| {
                        !Self::rset_entry_is_stale(regions, source, generation)
                    })
                    .map(|(source, _)| source)
                    .collect()
            };
            for source in live_sources {
                if regions[source].region_type() != RegionType::Free {
                    sources.push(source);
                }
            }
            if sources.len() > EAGER_RECLAIM_MAX_SOURCES {
                live.insert(i);
                continue;
            }
            for &s in &sources {
                if walked.insert(s)
                    && !self.note_humongous_targets_in_region(regions, s, true, &mut live)
                {
                    return None;
                }
            }
        }
        for s in jit_pinned.iter() {
            if s < regions.len()
                && walked.insert(s)
                && !self.note_humongous_targets_in_region(regions, s, false, &mut live)
            {
                return None;
            }
        }
        cratonvm_gc_core::gc_metrics::record_g1_humongous_liveness(
            marked_live,
            seeded_by_roots,
            walked.len() as u64,
        );
        Some(live)
    }

    /// Item 1 — walk one source region, read-only, and add every humongous span
    /// it references (other than its own) to `out`. With `card_screen`, an
    /// object touching no dirty card is stepped over — sound because every path
    /// that records an edge dirties the holder's card. Returns `false` if the
    /// walk broke on an object it could not size.
    pub fn note_humongous_targets_in_region(
        &self,
        regions: &[G1Region],
        src_idx: usize,
        card_screen: bool,
        out: &mut RegionSet,
    ) -> bool {
        let r = &regions[src_idx];
        if matches!(
            r.region_type(),
            RegionType::Free | RegionType::HumongousContinuation
        ) {
            return true;
        }
        let own_span = humongous_span_start(regions, src_idx);
        let base = r.data.as_ptr() as usize;
        let cursor = r.cursor();
        let screen = card_screen;
        let jit_skips = self.jit_tlab_skip_spans();
        let mut note = |raw: usize, out: &mut RegionSet| {
            if raw == 0 {
                return;
            }
            if let Some(j) = self.lookup_region_for_addr(raw) {
                if let Some(span) = humongous_span_start(regions, j) {
                    if Some(span) != own_span {
                        out.insert(span);
                    }
                }
            }
        };
        let mut offset = 0usize;
        while offset < cursor {
            let obj_addr = base + offset;
            if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, obj_addr) {
                offset += skip;
                continue;
            }
            if let Some(gap) = gap_filler_len(obj_addr as *const u8) {
                offset += gap;
                continue;
            }
            // LANE A — VALIDATE BEFORE DECODING, exactly as
            // `scan_source_region_for_cset_refs` does and for the same reason
            // its comment gives: the only rejection below is an implausible
            // SIZE, and a ZEROED header passes it (`class_id=0`, `num_slots=0`,
            // `kind=Object` sizes to exactly `HEADER_SIZE`). So a region whose
            // object grid no longer describes its contents was walked sixteen
            // bytes at a time through reclaimed memory until some stale bytes
            // decoded as a large array, and the element loop below then read
            // off the end of the region. This walk runs on OLD regions named by
            // a humongous span's remembered set, which is precisely the
            // population that accumulates additive entries, so it is at least
            // as exposed as the Phase-2 walk that got the guard.
            //
            // `return false` rather than `break`: the caller reads false as
            // "this walk could not be trusted" and retains every span rather
            // than reclaiming one on partial evidence. Refusing to reclaim is
            // the over-retaining direction.
            if !self.candidate_header_is_plausible(regions, obj_addr) {
                return false;
            }
            let header = unsafe { &*(obj_addr as *const ObjectHeader) };
            if is_humongous_filler(header) {
                break;
            }
            let obj_size = object_total_size(header);
            if obj_size < HEADER_SIZE || offset + obj_size > cursor {
                return false;
            }
            if screen && !self.cards.any_dirty_in(obj_addr, obj_size) {
                offset += obj_size;
                continue;
            }
            let obj_ptr = obj_addr as *mut u8;
            // LANE A — the same region clamp the other three walks have; see
            // `collect_outgoing_cross_region_edges`. A plausible header does not
            // bound its own declared element/slot count.
            if header.kind() == ObjectKind::Array {
                if header.element_type() == ArrayElementType::Reference {
                    let data = unsafe { obj_ptr.add(ARRAY_DATA_OFFSET) };
                    let walkable = self.holder_walkable_slots(
                        regions,
                        obj_ptr,
                        header.array_length() as usize,
                        8,
                    );
                    for k in 0..walkable {
                        let raw = unsafe { std::ptr::read(data.add(k * 8) as *const u64) };
                        note(raw as usize, out);
                    }
                }
            } else {
                let walkable = self.holder_walkable_slots(
                    regions,
                    obj_ptr,
                    header.num_slots() as usize,
                    SLOT_SIZE,
                );
                for_each_flat_object_reference_capped(obj_ptr, header, 0, walkable, |_, raw, _| {
                    note(raw, out)
                });
            }
            offset += obj_size;
        }
        true
    }

    #[cfg(debug_assertions)]
    /// Debug oracle for [`Self::eager_reclaim_humongous_locked`]: abort if a
    /// root-reachable object or root still references a span about to be freed.
    ///
    /// A non-Free region is not a liveness proof: it can contain a dropped
    /// old array whose stale slots must not keep a span alive.  Restrict the
    /// independent walk to the precise graph rooted at this pause's roots,
    /// then check every slot of those surviving objects.  A walk that cannot
    /// make sense of an object stops covering that object, which is exactly
    /// the case the caller has already refused to act on — the condition is
    /// checked there, so reaching one here means the census and this pass
    /// disagree, and the assertion below is the point.
    pub fn debug_assert_no_reference_into_spans(
        &self,
        regions: &[G1Region],
        roots: &[ObjectRef],
        condemned: &RegionSet,
    ) {
        let reachable = self.reachable_object_addresses_from_roots(regions, roots, &[], false);
        let mut offend = |addr: usize, from: &str| {
            if let Some(idx) = self.lookup_region_for_addr(addr) {
                assert!(
                    !condemned.contains(&idx),
                    "g1 eager humongous reclaim would free a live span: {from} \
                     references {addr:#x} in region {idx}"
                );
            }
        };

        for root in roots {
            offend(root.as_ptr() as usize, "a GC root");
        }

        let jit_skips = self.jit_tlab_skip_spans();
        for (i, region) in regions.iter().enumerate() {
            if region.region_type() == RegionType::Free || condemned.contains(&i) {
                continue;
            }
            let base = region.data.as_ptr() as *mut u8;
            let cursor = region.cursor();
            let mut offset = 0usize;
            while offset < cursor {
                let obj_ptr = unsafe { base.add(offset) };
                if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, obj_ptr as usize) {
                    offset += skip;
                    continue;
                }
                if let Some(gap) = gap_filler_len(obj_ptr) {
                    offset += gap;
                    continue;
                }
                let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                if is_humongous_filler(header) {
                    break;
                }
                let obj_size = object_total_size(header);
                if obj_size < HEADER_SIZE || offset + obj_size > cursor {
                    break;
                }
                if !reachable.contains(&(obj_ptr as usize)) {
                    offset += obj_size;
                    continue;
                }
                let data_start = unsafe { obj_ptr.add(ARRAY_DATA_OFFSET) };
                if header.kind() == ObjectKind::Array {
                    if header.element_type() == ArrayElementType::Reference {
                        for k in 0..header.array_length() as usize {
                            let raw: u64 =
                                unsafe { std::ptr::read(data_start.add(k * 8) as *const u64) };
                            if raw != 0 {
                                offend(raw as usize, "a surviving array");
                            }
                        }
                    }
                } else {
                    for_each_flat_object_reference_trusting_header(
                        obj_ptr as *mut u8,
                        header,
                        0,
                        |_, raw, _| {
                            offend(raw, "a surviving object");
                        },
                    );
                }
                offset += obj_size;
            }
        }
    }

    // -----------------------------------------------------------------------
    // IHOP
    // -----------------------------------------------------------------------

    /// Check if old-gen occupancy has reached the IHOP threshold.
    ///
    /// # LANE W2-C — why a level test alone is not enough
    ///
    /// This was `old_bytes >= threshold` and nothing else: a pure comparison
    /// with no memory of what the last cycle achieved. If the LIVE old set
    /// legitimately sits above the threshold — a big cache, a large resident
    /// graph, or simply a heap sized close to the live set — then the level
    /// test is true, the VM starts a concurrent cycle, the cycle marks the
    /// whole old generation, `cleanup` frees nothing because nothing is dead,
    /// `recompute_old_gen_bytes` republishes the same occupancy, and the level
    /// test is true again. Forever.
    ///
    /// `cleanup`'s own comment worries about exactly this shape and fixes only
    /// the STALENESS half. Refreshing the occupancy does not help when the
    /// occupancy is genuinely high: the collector then burns
    /// `concurrent_mark_worker_count()` threads continuously, beside the
    /// application, for no reclamation at all — and it does so precisely when
    /// the heap is tightest and the application least able to spare the cores.
    ///
    /// Real G1 does not have this because `G1AdaptiveIHOP` folds the cycle's
    /// OUTCOME back in. So does this now: after the level test passes,
    /// additionally require that the old generation has GROWN since a cycle
    /// that reclaimed nothing. Any growth re-arms it, so this cannot wedge
    /// reclamation — the only state it suppresses is "re-run a cycle against a
    /// heap that has not changed since the last one found nothing".
    ///
    /// The width doubles per futile cycle (capped at
    /// [`MARK_BACKOFF_MAX_REGIONS`]) and resets to one region as soon as a
    /// cycle reclaims well, which is what keeps a workload that promotes a
    /// region between cycles from re-firing the trigger every time while still
    /// reclaiming nothing.
    ///
    /// Behind `CRATONVM_G1_IHOP_BACKOFF` (opt-in, `=1`), so the change is a
    /// single-binary A/B. The A/B to run: `-XX:+UseG1GC -Xmx256m` on a workload
    /// with a large resident live set and read `[GC] g1 ihop:`'s
    /// `backoff_declined_polls` (LANE W3-C renamed it from
    /// `backoff_suppressions`; it counts declined CONSULTS, not declined
    /// cycles — see [`IHOP_POLLS`]) against `[GC] g1 mark-pauses:`'
    /// `cleanup_pauses`, which is the cycle counter. With the flag off the
    /// cycle counter climbs continuously with zero regions freed; with it on
    /// the counter should stop climbing while the live set is flat, and
    /// `to_space_exhausted` must NOT rise — a back-off that defers a cycle the
    /// heap genuinely needed shows up there.
    pub fn check_ihop(&self) -> bool {
        // LANE W6-B — THE FAST DECLINE, and it is first because the whole
        // point is that it reaches no shared write.
        //
        // Everything below this block runs two read-modify-writes on two
        // process-global cache lines before it can say "no", and it says "no"
        // at the ALLOCATION rate: a suppressed cycle never sets
        // `g1_is_marking_active()`, so nothing short-circuits the JIT mark
        // driver that every compiled `new` calls. Wave 5 measured 41 641 213
        // declined polls against 130 mark cycles on ONE thread, where those
        // lines are uncontended. With N allocating mutators they are N-way
        // contention on two lines, and each one is a cache-line round trip
        // between cores.
        //
        // The condition is the slow path's own, not an approximation of it.
        // `backoff_gate_open_at_bytes` is `last_cycle_end_old_bytes +
        // required` as the slow path computed it; every input to that
        // expression has exactly one writer, `note_mark_cycle_outcome`, and
        // that writer clears this watermark. So while the watermark is
        // non-zero, an occupancy below it provably still declines. The level
        // test above the back-off is skipped, and skipping it is safe in the
        // only direction that matters: if occupancy has fallen below the IHOP
        // threshold the slow path returns `false` as well.
        //
        // NOT taken when `CRATONVM_G1_MARK_BACKOFF_DEADLINE` is armed. The
        // deadline is a second exit from the same decline, on a clock this
        // path does not read, so with it on the poll goes the long way round
        // and pays what it costs. Stated here rather than discovered later,
        // because "the fix silently disabled the fail-safe" is the shape of
        // defect this round keeps finding.
        if gc_flags().g1_ihop_poll_gate && !gc_flags().g1_mark_backoff_deadline {
            let gate = self.backoff_gate_open_at_bytes.load(Ordering::Relaxed);
            if gate != 0 && self.old_gen_bytes.load(Ordering::Relaxed) < gate {
                self.note_gated_poll();
                return false;
            }
        }
        // Anything that gets past the gate takes the long path, so this
        // thread's residue is published first: the counters must never read
        // as though polls arrived out of order, and the slow path is rare by
        // construction.
        self.flush_gated_polls();
        // LANE W3-C — the denominator. See [`IHOP_POLLS`]: this is a poll on
        // the allocation slow path, not a per-cycle decision, and
        // `mark_backoff_suppressions` is a count of times THIS CALL declined.
        IHOP_POLLS.fetch_add(1, Ordering::Relaxed);
        let old_bytes = self.old_gen_bytes.load(Ordering::Relaxed);
        let threshold = self.marking_threshold_bytes.load(Ordering::Relaxed);
        if old_bytes < threshold {
            return false;
        }
        if !gc_flags().g1_ihop_backoff && !self.breached_reserve_backs_off() {
            return true;
        }
        let last_reclaimed = self.last_cycle_reclaimed_bytes.load(Ordering::Relaxed);
        if last_reclaimed > 0 {
            // The last cycle got something back. Nothing to back off from.
            return true;
        }
        let last_end_old = self.last_cycle_end_old_bytes.load(Ordering::Relaxed);
        if last_end_old == 0 {
            // No cycle has completed yet (or the last one ended on an empty
            // old generation, in which case the level test above could not
            // have passed on the same occupancy). The first cycle is never
            // suppressed: the back-off is evidence about a cycle that ran.
            return true;
        }
        let required = self
            .mark_backoff_regions
            .load(Ordering::Relaxed)
            .max(1)
            .saturating_mul(self.config.region_size);
        if old_bytes < last_end_old.saturating_add(required) {
            // LANE W5-C — the growth gate says no. Before believing it, ask
            // whether it is ENTITLED to say no, because there is one thing it
            // cannot see.
            //
            // The gate's premise is "nothing has changed since the cycle that
            // found nothing". It tests that by comparing occupancy, which
            // detects promotion and detects nothing else. An old object dies
            // when a mutator overwrites the last reference to it: the bytes do
            // not move, the region does not shrink, `recompute_old_gen_bytes`
            // republishes the identical number, and the gate reads "unchanged"
            // about a heap in which half the old generation just became
            // garbage. Discovering that is the concurrent marker's entire job,
            // so the gate is refusing the cycle on the strength of evidence
            // that the cycle is the only way to obtain.
            //
            // `G1OldBurstProbe` is the shape: a grow half where every cycle is
            // genuinely futile (and widens this gate), then a settle half
            // where a large old data set is dropped and only young garbage is
            // allocated. Occupancy is flat across the drop, so the widened
            // gate holds shut over exactly the cycle worth a large fraction of
            // the heap, and the collector learns this by failing to evacuate.
            //
            // A better level test cannot fix that — the information is not in
            // the heap's occupancy — so the bound is a DEADLINE instead: after
            // `MARK_BACKOFF_DEADLINE_COLLECTIONS` collections have completed
            // since the last cycle ended, let one through anyway. This keeps
            // the anti-spin property the back-off exists for, because the spin
            // wave 2 described is a cycle starting with ZERO collections in
            // between, and it converts "never again until the heap grows" into
            // "at most one cycle per N collections while it does not".
            //
            // Ordered after the suppression test and behind its own flag so a
            // build with the deadline off is byte-identical here.
            //
            // The release ADVANCES the clock, by compare-exchange, and that is
            // not bookkeeping — it is what makes the sentence above true. This
            // is a POLL: wave 3 found the sibling counter reading 1 616 073
            // declined polls against 2 mark cycles and had to rename it
            // (`backoff_declined_polls`, see [`IHOP_POLLS`]). A deadline that
            // merely answered "true" once expired would answer true to every
            // poll until a cycle actually started — the gate would be OPEN, not
            // bounded, and `backoff_deadline_releases` would be a second
            // poll-counter wearing a cycle-counter's name. Moving the clock to
            // `now` means the next release needs another
            // `MARK_BACKOFF_DEADLINE_COLLECTIONS` collections whether or not
            // the caller acted on this one, so the counter is releases and the
            // rate is bounded.
            //
            // `compare_exchange` rather than a store because `check_ihop` is
            // called from every mutator's allocation slow path: two threads
            // crossing the deadline together must produce one release, not two.
            // Relaxed on both sides — this orders no data, and a thread that
            // loses the exchange simply takes the suppression below, which is
            // the same answer it would have got a microsecond earlier.
            if gc_flags().g1_mark_backoff_deadline {
                let last_collections = self.last_cycle_end_collections.load(Ordering::Relaxed);
                let now = self.collection_count.load(Ordering::Relaxed);
                if last_collections != u64::MAX
                    && now.saturating_sub(last_collections) >= MARK_BACKOFF_DEADLINE_COLLECTIONS
                    && self
                        .last_cycle_end_collections
                        .compare_exchange(
                            last_collections,
                            now,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        )
                        .is_ok()
                {
                    self.mark_backoff_deadline_releases
                        .fetch_add(1, Ordering::Relaxed);
                    return true;
                }
            }
            // LANE W6-B — publish the occupancy at which this answer could
            // change, so the NEXT poll can decline without touching either of
            // the two global lines below. Behind the flag, so the default
            // build's declined path is the one it has always been.
            if gc_flags().g1_ihop_poll_gate {
                self.backoff_gate_open_at_bytes
                    .store(last_end_old.saturating_add(required), Ordering::Relaxed);
            }
            // Counted, never silent: a collector that declines to collect must
            // be distinguishable from a collector that has stopped working.
            // `ihop_model_state()` reports this and
            // `collector_decision_report()` prints it.
            self.mark_backoff_suppressions
                .fetch_add(1, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// LANE W6-B — record one fast-path decline on this thread, without a
    /// shared write in the common case.
    ///
    /// See [`IHOP_POLL_FLUSH_BATCH`] for the exactness bound and
    /// [`IHOP_POLL_BATCH`] for why the collector's identity is carried.
    #[inline]
    pub fn note_gated_poll(&self) {
        let key = self as *const G1Collector as usize;
        let flush = IHOP_POLL_BATCH.with(|c| {
            let (owner, pending) = c.get();
            if owner != key {
                // A different collector's residue. Both statics are
                // process-wide, so they stay exact; only the departed
                // collector's own suppression count loses it.
                if pending > 0 {
                    IHOP_POLLS.fetch_add(pending, Ordering::Relaxed);
                }
                c.set((key, 1));
                return 0;
            }
            let pending = pending + 1;
            if pending >= IHOP_POLL_FLUSH_BATCH {
                c.set((key, 0));
                pending
            } else {
                c.set((key, pending));
                0
            }
        });
        if flush > 0 {
            IHOP_POLLS.fetch_add(flush, Ordering::Relaxed);
            self.backoff_fast_declines
                .fetch_add(flush, Ordering::Relaxed);
            self.mark_backoff_suppressions
                .fetch_add(flush, Ordering::Relaxed);
        }
    }

    /// LANE W6-B — publish this thread's unflushed fast-path declines.
    ///
    /// Called by `check_ihop` whenever a poll takes the long path, and public
    /// so a test that asserts on `backoff_declined_polls` can be exact rather
    /// than exact-to-within-[`IHOP_POLL_FLUSH_BATCH`]. A report printed while
    /// other threads are mid-streak still under-reports by at most
    /// `IHOP_POLL_FLUSH_BATCH - 1` per such thread; that is stated on the
    /// `[GC] g1 ihop-poll-gate:` line rather than left for a reader to
    /// discover.
    pub fn flush_gated_polls(&self) {
        let key = self as *const G1Collector as usize;
        let flush = IHOP_POLL_BATCH.with(|c| {
            let (owner, pending) = c.get();
            if owner != key || pending == 0 {
                return 0;
            }
            c.set((key, 0));
            pending
        });
        if flush > 0 {
            IHOP_POLLS.fetch_add(flush, Ordering::Relaxed);
            self.backoff_fast_declines
                .fetch_add(flush, Ordering::Relaxed);
            self.mark_backoff_suppressions
                .fetch_add(flush, Ordering::Relaxed);
        }
    }

    /// LANE W6-B — `G1Collector::backoff_fast_declines` (the field), for the report
    /// line and for `gc/tests/g1_w6b_ihop_poll_gate.rs`.
    ///
    /// Call `flush_gated_polls` first if the exact number matters: the calling
    /// thread may be holding up to [`IHOP_POLL_FLUSH_BATCH`] declines it has
    /// not published yet.
    pub fn backoff_fast_declines(&self) -> u64 {
        self.backoff_fast_declines.load(Ordering::Relaxed)
    }

    /// LANE W5-C — how many times the back-off's deadline
    /// (`CRATONVM_G1_MARK_BACKOFF_DEADLINE`) overrode the growth gate.
    ///
    /// Reported on the `[GC] g1 ihop:` line beside `backoff_declined_polls`,
    /// and it is the number that says which of the two readings of a run is
    /// true: a large count means the growth gate was systematically wrong on
    /// this workload, and zero with the flag armed means the gate was never
    /// holding anything back, so the arm is inert and its wall clock says
    /// nothing about the deadline.
    pub fn mark_backoff_deadline_releases(&self) -> u64 {
        self.mark_backoff_deadline_releases.load(Ordering::Relaxed)
    }

    // LANE W2-C — fold one completed cycle's reclamation outcome into the
    // back-off state. Called from the end of `cleanup` with the occupancy
    // either side of `recompute_old_gen_bytes`.
    //
    // `reclaimed` is deliberately compared against a FRACTION of the occupancy
    // rather than against zero — see [`MARK_BACKOFF_PROGRESS_SHIFT`] for why a
    // cycle that frees one region out of two hundred must not count as
    // progress. The width doubles on a futile cycle and snaps back to one
    // region on a productive one: slow to give up, immediate to resume.
    //
    // Recorded on BOTH arms of `CRATONVM_G1_IHOP_BACKOFF`. Only `check_ihop`
    // consults it, so with the flag off this is pure bookkeeping — and keeping
    // it live means a run that flips the flag does not start from a blank
    // history, and `ihop_model_state()` reports a truthful width either way.
    // ALLOWLISTED in `scripts/baselines/orphan-instruments-allowlist.txt`, and
    // reviewed rather than inherited: this recorder IS fed, by `cleanup` a few
    // hundred lines above, and is `pub` only because `gc/tests/g1_w2c_ihop_model.rs`
    // and `g1_w5c_mark_backoff_deadline.rs` drive it. The gate cannot see either
    // fact -- a same-file caller is not a caller by its rule, and `**/tests/**`
    // is not searched at all. It deliberately does NOT carry an
    // `orphan-gate: read-by cleanup` annotation: that check is textual, and
    // `cleanup(` matches unrelated functions in difftest and vm, so the
    // annotation would pass for the wrong reason. An exemption that verifies
    // against somebody else's code is worse than an allowlist entry.
    pub fn note_mark_cycle_outcome(&self, old_before: usize, old_after: usize) {
        let reclaimed = old_before.saturating_sub(old_after);
        let progress_floor = old_before >> MARK_BACKOFF_PROGRESS_SHIFT;
        self.last_cycle_reclaimed_bytes
            .store(reclaimed, Ordering::Relaxed);
        self.last_cycle_end_old_bytes
            .store(old_after, Ordering::Relaxed);
        // LANE W5-C — and the deadline's origin. Written on BOTH arms, for the
        // same reason the pair above is: a run that flips
        // `CRATONVM_G1_MARK_BACKOFF_DEADLINE` must not start from a blank
        // history, or the first deadline after the flip would be measured from
        // process start.
        self.last_cycle_end_collections.store(
            self.collection_count.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        // LANE W6-B — a cycle has just rewritten every input to the decline
        // condition, so the published watermark is stale by definition.
        // Cleared on BOTH arms of `CRATONVM_G1_IHOP_POLL_GATE` and of the
        // back-off, for the reason the two stores above are written on both
        // arms: a run that flips a flag must not start from stale state, and a
        // watermark that outlived its inputs would be the one way the fast
        // path could disagree with the gate it stands for.
        self.backoff_gate_open_at_bytes.store(0, Ordering::Relaxed);
        if reclaimed > progress_floor {
            self.mark_backoff_regions.store(1, Ordering::Relaxed);
        } else {
            // Treated as "reclaimed nothing" by `check_ihop` too: the zero it
            // tests for is this same verdict, so the two must be written
            // together or a cycle could widen the gate and then be allowed
            // through it.
            self.last_cycle_reclaimed_bytes.store(0, Ordering::Relaxed);
            let width = self.mark_backoff_regions.load(Ordering::Relaxed).max(1);
            self.mark_backoff_regions.store(
                width.saturating_mul(2).min(MARK_BACKOFF_MAX_REGIONS),
                Ordering::Relaxed,
            );
        }
    }

    /// F-18 — the age at which this pause promotes rather than copies.
    ///
    /// Reads the adaptive value under `CRATONVM_G1_ADAPTIVE_TENURING` (the
    /// default) and the configured `promotion_age` otherwise.
    #[inline]
    pub fn tenuring_threshold(&self) -> u8 {
        {
            self.tenuring_threshold.load(Ordering::Relaxed) as u8
        }
    }

    /// gci i47w/sg47 -- the age at which THIS pause's evacuators promote:
    /// [`Self::tenuring_threshold`], or 0 on a `System.gc()`-door pause that
    /// tenures every live young object (`CRATONVM_GC_SYSTEM_GC_TENURES_ALL`,
    /// `pause_tenures_all`). Only the evacuators read it; the reported and
    /// adaptive threshold stays [`Self::tenuring_threshold`].
    #[inline]
    pub fn evac_tenuring_threshold(&self) -> u8 {
        if self.pause_tenures_all.load(Ordering::Relaxed) {
            0
        } else {
            self.tenuring_threshold()
        }
    }

    /// gci i23/tn -- HotSpot's `MaxTenuringThreshold` on this collector:
    /// `config.promotion_age` unless [`Self::set_tenuring_flags`] set it.
    #[inline]
    pub fn max_tenuring_threshold(&self) -> u8 {
        self.max_tenuring_threshold.load(Ordering::Relaxed)
    }

    /// gci i23/tn -- HotSpot's `TargetSurvivorRatio` on this collector
    /// (percent; 50 unless [`Self::set_tenuring_flags`] set it).
    #[inline]
    pub fn target_survivor_ratio(&self) -> u8 {
        self.target_survivor_ratio.load(Ordering::Relaxed)
    }

    /// gci i23/tn (`gci-i22sz-g1-survivor-target-is-twice-hotspots-RETIRED-UNFIXED-20261018`
    /// item 5) -- apply the typed HotSpot tenuring flags to G1, before the
    /// first collection (`VmHeap::set_tenuring_config`'s G1 arm). `None` =
    /// not typed: that value is left as it is. The launcher has validated
    /// both against HotSpot's ranges; out-of-range values are clamped here
    /// all the same (`0..=16`, `0..=100`).
    ///
    /// * `MaxTenuringThreshold` is the ceiling AND the starting threshold
    ///   (HotSpot's `G1Policy` constructor: `_tenuring_threshold(MaxTenuringThreshold)`);
    ///   0 is `AlwaysTenure` (every pause tenures everything), 16 is
    ///   `NeverTenure` (no age reaches it). The `gc,age` "used" pair of GC(0)
    ///   follows it.
    /// * `TargetSurvivorRatio` is the share of the survivor capacity in
    ///   [`g1_desired_survivor_bytes_at`]; GC(0)'s desired size is re-derived
    ///   at the current young target.
    ///
    /// `-XX:InitialTenuringThreshold` is not an argument: HotSpot G1 does not
    /// read it (Parallel only).
    pub fn set_tenuring_flags(
        &self,
        max_tenuring_threshold: Option<u8>,
        target_survivor_ratio: Option<u8>,
    ) {
        if let Some(max) = max_tenuring_threshold {
            let max = max.min(G1_MAX_TENURING_THRESHOLD_LIMIT);
            self.max_tenuring_threshold.store(max, Ordering::Relaxed);
            self.always_tenure.store(max == 0, Ordering::Relaxed);
            self.tenuring_threshold
                .store(u64::from(max), Ordering::Relaxed);
            self.last_pause_tenuring_threshold
                .store(u64::from(max), Ordering::Relaxed);
        }
        if let Some(ratio) = target_survivor_ratio {
            self.target_survivor_ratio
                .store(ratio.min(100), Ordering::Relaxed);
            self.tenuring_desired_bytes
                .store(self.survivor_target_bytes() as u64, Ordering::Relaxed);
        }
    }

    /// gci i23/tn (i22/sz page item 4) -- arm this pause's Survivor region cap
    /// at `cap` regions (HotSpot's `_max_survivor_regions`; `collect_garbage`
    /// passes [`g1_max_survivor_regions`] of the young target at pause
    /// start). Resets the claim count, the refusal flag and the serial
    /// evacuator's survivor-full latch.
    pub fn arm_survivor_region_cap(&self, cap: usize) {
        self.survivor_regions_claimed.store(0, Ordering::Relaxed);
        self.survivor_cap_refused_this_pause
            .store(false, Ordering::Relaxed);
        self.survivor_full_serial.store(false, Ordering::Relaxed);
        self.survivor_region_cap.store(cap, Ordering::Relaxed);
    }

    /// gci i23/tn -- end of the pause: count it when the cap refused a region,
    /// and unarm (a collection entered outside `collect_garbage` -- the unit
    /// tests' direct `young_collection` calls -- runs uncapped).
    pub fn disarm_survivor_region_cap(&self) {
        if self
            .survivor_cap_refused_this_pause
            .swap(false, Ordering::Relaxed)
        {
            self.survivor_cap_pauses.fetch_add(1, Ordering::Relaxed);
        }
        self.survivor_full_serial.store(false, Ordering::Relaxed);
        self.survivor_region_cap
            .store(usize::MAX, Ordering::Relaxed);
    }

    /// gci i23/tn -- may this pause claim one more Survivor region? Counts the
    /// claim when it may. Always `true` while unarmed. Called by both
    /// evacuators right before they take a FRESH Survivor region (the
    /// parallel `claim_pool_slot`, the serial `alloc_in_type_locked_scan`),
    /// never for a bump into a region already claimed.
    pub fn survivor_cap_admits_region(&self) -> bool {
        let cap = self.survivor_region_cap.load(Ordering::Relaxed);
        if cap == usize::MAX {
            return true;
        }
        let admitted = self
            .survivor_regions_claimed
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < cap).then_some(n + 1)
            })
            .is_ok();
        if !admitted {
            self.survivor_cap_refused_this_pause
                .store(true, Ordering::Relaxed);
        }
        admitted
    }

    /// gci i23/tn -- has this pause's cap refused a Survivor region? The
    /// evacuators' test for "a failed Survivor copy goes to Old" (HotSpot's
    /// `allocate_in_next_plab`); `false` while unarmed.
    #[inline]
    pub fn survivor_cap_reached(&self) -> bool {
        self.survivor_region_cap.load(Ordering::Relaxed) != usize::MAX
            && self.survivor_cap_refused_this_pause.load(Ordering::Relaxed)
    }

    /// gci i23/tn -- copies the Survivor region cap sent to Old so far.
    pub fn survivor_cap_promotions(&self) -> u64 {
        self.survivor_cap_promotions.load(Ordering::Relaxed)
    }

    /// gci i23/tn -- pauses in which the Survivor region cap refused a region.
    pub fn survivor_cap_pauses(&self) -> u64 {
        self.survivor_cap_pauses.load(Ordering::Relaxed)
    }

    /// F-18 — record that `bytes` of live data reached age `age` in this pause.
    ///
    /// Called from both evacuators for every object copied to survivor space
    /// (gci i26/pc: the parallel one through its worker's
    /// `TlabSet::note_survivor_age`, summed in at `SharedEvac::retire_all`).
    /// Promotions are deliberately NOT counted: the histogram answers "how much
    /// would survivor space have to hold if the threshold were N", and an
    /// object that has already been tenured is not in survivor space at any
    /// threshold.
    #[inline]
    pub fn note_survivor_age(&self, age: u8, bytes: usize) {
        let idx = (age as usize).min(G1_MAX_TENURING_AGE - 1);
        self.survivor_age_bytes[idx].fetch_add(bytes, Ordering::Relaxed);
    }

    /// Record `bytes` copied into an OLD region — see
    /// [`Self::bytes_promoted_total`].
    ///
    /// (gci i26/pc: the parallel evacuator counts into its worker's
    /// `TlabSet::note_promoted_bytes` and sums in at `SharedEvac::retire_all`.)
    /// Called from the winning arm of both evacuators, i.e. from the point at
    /// which the copy has been PUBLISHED by installing the forwarding word, for
    /// the same reason `note_survivor_age` moved there: a CAS loser's copy is
    /// abandoned to-space garbage that no reference names, and counting it
    /// would inflate the figure in proportion to the worker count.
    ///
    /// An evacuation FAILURE is deliberately not counted. A self-forwarded
    /// object did not move, so no bytes reached old, however old its header
    /// says it is.
    pub fn note_promoted_bytes(&self, bytes: usize) {
        self.bytes_promoted_total
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// Bytes copied into Old regions since this collector was built.
    ///
    /// The producer for `VmHeap::bytes_promoted_total`'s G1 arm; see
    /// [`Self::bytes_promoted_total`] the field for why the GC-overhead limit
    /// needs it.
    pub fn bytes_promoted_total(&self) -> u64 {
        self.bytes_promoted_total.load(Ordering::Relaxed)
    }

    /// F-18 — how many bytes survivor space is aiming to hold.
    ///
    /// A share of the young generation, which is itself adaptive
    /// (`young_target_regions`, F-17/G1AUD-9), so this tracks a young
    /// generation that grows and shrinks rather than being a fixed byte count
    /// that stops meaning anything when it does.
    ///
    /// gci i22/sz: HotSpot's desired survivor size
    /// ([`g1_desired_survivor_bytes`]: `ceil(young_target / SurvivorRatio)`
    /// regions at `TargetSurvivorRatio` = 50 %). It was `young_target * region
    /// / 8`, twice HotSpot's at eight young regions and more.
    ///
    /// When it is read: HotSpot computes it at the START of a young pause
    /// (`record_young_collection_start` -> `update_survivors_policy`) from the
    /// age table the previous pause left and the young target then current;
    /// ours computes it at the END of the previous pause, after the young
    /// re-size, from the same table and the same target. The threshold a
    /// pause uses is therefore the same; only the `gc,age` line's placement
    /// differs (HotSpot prints it in the pause that uses it). HotSpot can
    /// also revise the young target between pauses (the remembered-set
    /// sampling task); ours does not.
    pub fn survivor_target_bytes(&self) -> usize {
        // gci i23/tn: at this collector's `TargetSurvivorRatio` (50 unless
        // typed; `set_tenuring_flags`). gci i43/gy: and its `SurvivorRatio`
        // (8 unless typed; `set_survivor_ratio`).
        g1_desired_survivor_bytes_with(
            self.young_target_within_free(),
            self.config.region_size,
            usize::from(self.target_survivor_ratio()),
            self.survivor_ratio.load(Ordering::Relaxed),
        )
    }

    /// g1ep6 -- HotSpot's young target now ([`g1_young_target_within_free`]
    /// of `young_target_regions`, the young regions allocated and the Free
    /// pool): what the survivor policy (the pause's Survivor region cap and
    /// the desired survivor size) divides. Read at the end of a pause (the
    /// desired size) and at the start of the next (the cap), it is the same
    /// figure: the Eden allocated in between moved from Free to young.
    pub fn young_target_within_free(&self) -> usize {
        g1_young_target_within_free(
            self.young_target_regions.load(Ordering::Relaxed),
            self.young_region_count.load(Ordering::Relaxed),
            self.free_region_count.load(Ordering::Relaxed),
            self.region_table_len(),
        )
    }

    /// gci i43/gy (lane lf's hunk H-gy1) -- HotSpot's `-XX:SurvivorRatio` on
    /// this collector (8 unless [`Self::set_survivor_ratio`] set it).
    #[inline]
    pub fn survivor_ratio(&self) -> usize {
        self.survivor_ratio.load(Ordering::Relaxed)
    }

    /// gci i43/gy (lane lf's hunk H-gy1) -- apply a typed `-XX:SurvivorRatio`
    /// (the launcher has validated HotSpot's `>= 1`; 0 reads as 1) before the
    /// first collection: HotSpot G1's `_max_survivor_regions = ceil(young
    /// target / SurvivorRatio)` (`G1Policy::update_survivors_policy`) -- the
    /// pause's Survivor region cap and the desired survivor size; GC(0)'s
    /// desired size is re-derived. `None`, or `CRATONVM_G1_YOUNG_GEN_SIZER=0`,
    /// changes nothing (8, as before).
    pub fn set_survivor_ratio(&self, survivor_ratio: Option<u64>) {
        let Some(r) = survivor_ratio else {
            return;
        };
        if !g1_young_gen_sizer_enabled() {
            return;
        }
        let r = usize::try_from(r).unwrap_or(usize::MAX).max(1);
        self.survivor_ratio.store(r, Ordering::Relaxed);
        self.tenuring_desired_bytes
            .store(self.survivor_target_bytes() as u64, Ordering::Relaxed);
    }

    /// F-18 — re-derive the tenuring threshold from the age histogram this
    /// pause just filled, then clear it for the next one.
    ///
    /// # Why a fixed threshold is the wrong shape
    ///
    /// `promotion_age` defaulted to 15 and was read directly, so every object
    /// was copied FIFTEEN TIMES before promotion regardless of how full
    /// survivor space was. That is the right answer for a workload whose
    /// medium-lived objects are few, and a straightforwardly wasteful one for a
    /// workload where they are not: a burst of objects that live for a dozen
    /// pauses is copied a dozen times, and the copying is the expensive half of
    /// an evacuation pause.
    ///
    /// # The rule
    ///
    /// HotSpot's: walk the histogram from the youngest age accumulating
    /// surviving bytes, and stop at the first age whose cumulative total
    /// exceeds the survivor target. Objects at or above that age are promoted;
    /// everything younger is copied. So a pause whose survivors comfortably fit
    /// keeps the configured threshold, and one whose survivors do not tenures
    /// earlier — exactly enough earlier to fit.
    ///
    /// Clamped to `[1, config.promotion_age]`. The floor of 1 is what stops a
    /// pathological histogram from promoting objects on their FIRST collection,
    /// which would defeat generational filtering entirely; the ceiling is the
    /// operator's number, which the adaptive rule may undercut but not exceed.
    pub fn update_tenuring_threshold(&self) {
        self.update_tenuring_threshold_merging(false);
    }

    /// [`Self::update_tenuring_threshold`] for one record, `merge` = the
    /// record is a drain pass of the pause that just recorded itself.
    ///
    /// gci i22/sz: a drain pass (`retry_after_evacuation_failure`, up to
    /// eight per pause) is part of the SAME pause, and HotSpot derives one
    /// threshold from one age table per pause. This used to re-derive the
    /// threshold from the drain's copies ALONE, overwriting the containing
    /// pause's decision with the residue's: a drain that copies little sets
    /// the threshold back to the configured age right after the pause whose
    /// survivors overflowed (the evacuation failure), the opposite of the
    /// adaptive rule. Merging adds the drain's ages to the pause's retained
    /// table and derives the threshold from the sum; the `gc,age` line's
    /// "used" pair is not shifted again (the pause used what it used).
    pub fn update_tenuring_threshold_merging(&self, merge: bool) {
        // gci i23/tn: the typed `-XX:MaxTenuringThreshold` (`0..=16`) when
        // there is one, `config.promotion_age` otherwise.
        let configured = self.max_tenuring_threshold().max(1);
        let target = self.survivor_target_bytes();
        let bucket_bytes = |age: usize| -> usize {
            let live = self.survivor_age_bytes[age].load(Ordering::Relaxed);
            if merge {
                live.saturating_add(self.last_survivor_age_bytes[age].load(Ordering::Relaxed))
            } else {
                live
            }
        };
        let mut cumulative = 0usize;
        let mut chosen = configured;
        for age in 1..G1_MAX_TENURING_AGE {
            cumulative = cumulative.saturating_add(bucket_bytes(age));
            if cumulative > target {
                chosen = (age as u8).clamp(1, configured);
                break;
            }
        }
        // gci i23/tn: a typed `-XX:MaxTenuringThreshold=0` is HotSpot's
        // `AlwaysTenure` -- `AgeTable::compute_tenuring_threshold` answers
        // `MaxTenuringThreshold` (0) without walking the table.
        if self.always_tenure.load(Ordering::Relaxed) {
            chosen = 0;
        }
        // LANE E (2026-09-20) — the STORE is gated on the flag; the consume
        // below is not, and the caller no longer gates either.
        //
        // `note_survivor_age` is called unconditionally by both evacuators, and
        // this — the only consumer — used to be called only when
        // `CRATONVM_G1_ADAPTIVE_TENURING` was on. With the flag off the
        // histogram therefore accumulated for the life of the process (a
        // `usize` per age band summing every survivor byte ever copied, which
        // wraps on a long soak) and `last_survivor_age_bytes` was never
        // written, so `tenuring_state()` — and the `[GC] g1 tenuring:` summary
        // line built from it — reported all zeros structurally. That is the
        // exact failure `tenuring_state`'s own doc names: "a diagnostic that is
        // structurally always zero is worse than none, because a zero reads as
        // an answer."
        //
        // So: always snapshot and clear, and publish the derived threshold only
        // when the policy that reads it is on. `tenuring_threshold()` returns
        // `config.promotion_age` in the off arm regardless, so the off arm's
        // BEHAVIOUR is byte-for-byte what it was — only the diagnostic changes.
        //
        // gci i22/sz: before the store, keep what the pause that just ran
        // USED (the desired size and threshold derived at the end of the
        // pause before it) for its `gc,age` line: HotSpot G1 derives them at
        // the start of the pause that uses them and prints them there.
        if !merge {
            self.last_pause_tenuring_desired_bytes.store(
                self.tenuring_desired_bytes.load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
            self.last_pause_tenuring_threshold
                .store(u64::from(self.tenuring_threshold()), Ordering::Relaxed);
        }
        self.tenuring_desired_bytes
            .store(target as u64, Ordering::Relaxed);
        self.tenuring_xlog_fresh.store(true, Ordering::Release);
        {
            self.tenuring_threshold
                .store(chosen as u64, Ordering::Relaxed);
        }
        for (age, bucket) in self.survivor_age_bytes.iter().enumerate() {
            // Snapshot before clearing: this is the only moment the histogram
            // exists, and a summary line read between pauses would otherwise
            // always report zeros. (gci i22/sz: a drain pass's snapshot is the
            // pause's table plus its own, `bucket_bytes`.)
            self.last_survivor_age_bytes[age]
                .store(bucket_bytes(age), Ordering::Relaxed);
            bucket.store(0, Ordering::Relaxed);
        }
    }

    /// F-18 diagnostics: the current threshold and the age histogram of the
    /// most recent pause that had one.
    ///
    /// The LAST pause's, not the live counters: `update_tenuring_threshold`
    /// consumes and clears the histogram at the end of every pause, so a read
    /// taken between pauses — which is every read an operator or a summary line
    /// can make — always finds it empty. A diagnostic that is structurally
    /// always zero is worse than none, because a zero reads as an answer.
    pub fn tenuring_state(&self) -> (u8, [usize; G1_MAX_TENURING_AGE]) {
        (
            self.tenuring_threshold.load(Ordering::Relaxed) as u8,
            std::array::from_fn(|i| self.last_survivor_age_bytes[i].load(Ordering::Relaxed)),
        )
    }

    /// gci i22/sz -- the `-Xlog:gc+age` facts of the pause that last ran, in
    /// HotSpot G1's shape: `(desired_survivor_bytes, threshold, max_threshold,
    /// ages)`. The desired size and threshold are the ones that pause USED
    /// (HotSpot's `GC(n) Desired survivor size D bytes, new threshold T (max
    /// threshold M)` is computed at the start of pause `n`); the ages are the
    /// table that pause FILLED (HotSpot's `Age table:` of pause `n`). Read
    /// after the pause; the VM renders them with the Serial renderer.
    pub fn tenuring_xlog_state(&self) -> (u64, u8, u8, [u64; G1_MAX_TENURING_AGE]) {
        (
            self.last_pause_tenuring_desired_bytes.load(Ordering::Relaxed),
            self.last_pause_tenuring_threshold.load(Ordering::Relaxed) as u8,
            self.max_tenuring_threshold(),
            std::array::from_fn(|i| self.last_survivor_age_bytes[i].load(Ordering::Relaxed) as u64),
        )
    }

    /// gci i22/sz -- [`Self::tenuring_xlog_state`] once per pause that
    /// derived a threshold (`None` after a pause that did not: a remark or
    /// cleanup pause, a second read), as `GenerationalHeap::take_tenuring_xlog`
    /// answers for Serial. For the VM's `-Xlog:gc+age` lines on G1.
    pub fn take_tenuring_xlog(&self) -> Option<(u64, u8, u8, [u64; G1_MAX_TENURING_AGE])> {
        self.tenuring_xlog_fresh
            .swap(false, Ordering::AcqRel)
            .then(|| self.tenuring_xlog_state())
    }

    /// F-15 — the static ceiling: the configured `-XX:InitiatingHeapOccupancyPercent`
    /// as a byte count.
    ///
    /// The adaptive threshold may float BELOW this and recover back up to it,
    /// never above. G1 has no full-GC fallback, so letting the threshold drift
    /// upward means dead promoted objects accumulate in Old unreclaimed until
    /// the heap is exhausted — observed on the SteadyChurn recreation, where
    /// ~7 ms pauses raised the old pause-time-driven threshold 5% per
    /// collection to its cap, concurrent marking never started across 61k young
    /// collections, and every heap size died with a true OOM while >80% of Old
    /// was garbage.
    ///
    /// gci i13/ef finding 11: a percentage of the region GRID
    /// ([`Self::heap_grid_bytes`]), not of `-Xmx`, which can exceed the grid
    /// by up to a region less a byte.
    #[inline]
    pub fn ihop_static_ceiling(&self) -> usize {
        self.heap_grid_bytes() * self.config.ihop_percent as usize / 100
    }

    /// gci i13/ef finding 11: the bytes the region table covers,
    /// `region_table_len() * region_size` -- "how big is this heap" in the one
    /// unit every region count already uses. `-Xmx` (`config.heap_size`)
    /// truncated to the grid.
    #[inline]
    pub fn heap_grid_bytes(&self) -> usize {
        self.region_table_len()
            .saturating_mul(self.config.region_size)
    }

    /// F-15 — the floor the threshold may never decay below: 1% of the heap, or
    /// one region, whichever is larger.
    ///
    /// Without a floor, a chronically-late collector decays the threshold to 0
    /// through integer truncation, and the VM-side trigger gate
    /// (`vm_heap::g1_should_start_marking` requires `marking_threshold_bytes() > 0`)
    /// then reads a zero threshold as "marking disabled" — permanently.
    #[inline]
    pub fn ihop_floor(&self) -> usize {
        // gci i13/ef finding 11: of the grid, not `-Xmx`.
        (self.heap_grid_bytes() / 100).max(self.config.region_size)
    }

    /// F-15 — record the start of a concurrent mark cycle.
    pub fn note_mark_cycle_start(&self) {
        // gci i22/sz: a cycle that opened answers any pending humongous
        // concurrent-start request (it must not open a second one later).
        self.humongous_conc_start_requested
            .store(false, Ordering::Relaxed);
        *self.mark_cycle_start.lock() = Some((
            std::time::Instant::now(),
            self.old_gen_bytes.load(Ordering::Relaxed),
        ));
    }

    /// F-15 — close the cycle opened by [`Self::note_mark_cycle_start`], fold
    /// its duration and the old-generation growth that happened during it into
    /// the running estimates, and recompute the threshold from them.
    ///
    /// Called at the end of `cleanup`, which is the point at which both
    /// quantities are known and the cycle's reclamation has been applied.
    pub fn note_mark_cycle_end(&self) {
        let Some((started, old_at_start)) = self.mark_cycle_start.lock().take() else {
            return;
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        // LANE W2-C — WHICH occupancy closes the measurement window.
        //
        // The model predicts "how much room must be left when a cycle starts so
        // that marking finishes before the heap fills", and the input it needs
        // is how fast old occupancy ROSE WHILE MARKING RAN. This function is
        // called from the very end of `cleanup`, after `recompute_old_gen_bytes`
        // has already applied that cycle's in-place Old frees and its
        // dead-humongous reclaim — so `old_gen_bytes` here is the occupancy the
        // cycle LEFT BEHIND, not the occupancy it had to survive. Freeing at the
        // end of a cycle does not retroactively give the marker headroom it did
        // not have.
        //
        // Two consequences, both in the unsafe direction. A cycle that reclaims
        // roughly what it promoted folds `grew == 0`, and `recompute_marking_
        // threshold` responds to a zero rate by leaving the STATIC IHOP in place
        // — i.e. the adaptive model silently disengages on exactly the workload
        // it was built for (steady-state churn). When it does not disengage it
        // under-reports, so the headroom is too small, so cycles start too late,
        // and the only signal that pulls it back is `note_to_space_exhausted` —
        // an evacuation failure that has already happened.
        //
        // `mark_cycle_gross_old_bytes` is sampled at the TOP of `cleanup`,
        // before that reclaim. Swapped (not loaded) so a second
        // `note_mark_cycle_end` outside a cleanup cannot re-use one cycle's
        // sample for another cycle's window.
        let old_now = if gc_flags().g1_ihop_gross_growth {
            let sampled = self
                .mark_cycle_gross_old_bytes
                .swap(usize::MAX, Ordering::Relaxed);
            if sampled == usize::MAX {
                // Driven outside `cleanup` (a test, or the teardown driver):
                // no pre-reclaim sample exists, so the live figure is the only
                // one available and this behaves exactly as the off arm does.
                self.old_gen_bytes.load(Ordering::Relaxed)
            } else {
                sampled
            }
        } else {
            self.old_gen_bytes.load(Ordering::Relaxed)
        };
        // Saturating: cleanup may have freed more than the cycle promoted, in
        // which case the growth rate for this cycle is zero, not negative.
        let grew = old_now.saturating_sub(old_at_start);
        self.fold_mark_cycle_sample(elapsed_ms, grew);
    }

    /// F-15 — the measurable core of [`Self::note_mark_cycle_end`]: fold one
    /// `(duration, growth)` sample into the running estimates and re-plan.
    ///
    /// Split from the wrapper because the wrapper reads a real clock. A test
    /// that drove it would have to sleep, and would then be asserting on
    /// whatever the scheduler did — this is the arithmetic, stated directly.
    /// LANE W2-C made this `pub`: `gc/tests/g1_w2c_ihop_model.rs` drives it
    /// directly, which is the whole reason it was split out from the clock in
    /// the first place. Same precedent as `dbg_mark_worker_scans_public`.
    pub fn fold_mark_cycle_sample(&self, elapsed_ms: u64, grew_bytes: usize) {
        // A cycle too short to time says nothing about either quantity; folding
        // a zero-millisecond sample in would drag both estimates toward zero
        // and quietly disable the headroom.
        //
        // LANE W3-C — but it is not SILENT any more. This return is a second
        // way for the adaptive model to disengage, and `ihop_disengaged_cycles`
        // (which is counted below, after this point) could not see it: a cycle
        // that folds nothing at all is at least as disengaged as one that folds
        // a zero rate. See [`IHOP_UNTIMED_CYCLES`] for why it gets its own
        // counter rather than being added to that one.
        if elapsed_ms == 0 {
            IHOP_UNTIMED_CYCLES.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // LANE W2-C — divide ONCE, and in bytes.
        //
        // This was `(grew_bytes / 1024) as u64 / elapsed_ms`: two integer
        // truncations in series, so any cycle growing under 1 KiB per
        // millisecond read as EXACTLY ZERO and fell into
        // `recompute_marking_threshold`'s "nothing measured" branch. 1 KiB/ms
        // is 1 MB/s — a perfectly ordinary promotion rate for a long cycle on
        // a small heap, and the model was structurally blind to all of it.
        //
        // So the rate is now carried in BYTES per millisecond
        // (`alloc_rate_bytes_per_ms`, which is what the model reads) and the
        // KiB figure stays as the reported diagnostic it was always described
        // as. `u64` bytes/ms times `u64` milliseconds cannot overflow at any
        // rate or duration a heap can produce, and the multiply saturates
        // regardless.
        let rate_bytes_per_ms = if gc_flags().g1_ihop_gross_growth {
            grew_bytes as u64 / elapsed_ms
        } else {
            // Off arm: byte-for-byte the old double truncation.
            ((grew_bytes / 1024) as u64 / elapsed_ms).saturating_mul(1024)
        };
        let rate_kib_per_ms = rate_bytes_per_ms / 1024;
        // LANE W2-C — a controller that quietly stops controlling is worse than
        // one that is absent, because nothing reports it. A zero rate is
        // exactly that: `recompute_marking_threshold` leaves the static IHOP in
        // place and the adaptive model has disengaged. Counted (and reported by
        // `ihop_model_state`) on BOTH arms of the flag, because the point of
        // the counter is to let an operator on the default build see the
        // disengaged state and decide whether to flip it.
        if rate_bytes_per_ms == 0 {
            let n = self.ihop_disengaged_cycles.fetch_add(1, Ordering::Relaxed) + 1;
            // Warn once every 16 so a permanently-disengaged model is visible
            // in an ordinary log without a cycle-per-line flood.
            if n == 1 || n % 16 == 0 {
                tracing::warn!(
                    "g1 adaptive IHOP: cycle measured zero old-gen growth over {} ms \
                     ({} such cycles) — the adaptive threshold is disengaged and the \
                     static -XX:InitiatingHeapOccupancyPercent stands. If this is a \
                     steady-state workload, try CRATONVM_G1_IHOP_GROSS_GROWTH=1, which \
                     measures growth DURING the cycle instead of net of its own reclaim.",
                    elapsed_ms,
                    n
                );
            }
        }

        // Exponential moving average, 1/4 weight on the newest sample. Slow
        // enough that one anomalous cycle does not re-plan the heap, fast
        // enough to follow a phase change within a handful of cycles.
        let blend = |old: u64, new: u64| if old == 0 { new } else { (old * 3 + new) / 4 };
        self.mark_ms_ema.store(
            blend(self.mark_ms_ema.load(Ordering::Relaxed), elapsed_ms),
            Ordering::Relaxed,
        );
        self.alloc_rate_kib_per_ms.store(
            blend(
                self.alloc_rate_kib_per_ms.load(Ordering::Relaxed),
                rate_kib_per_ms,
            ),
            Ordering::Relaxed,
        );
        // LANE W2-C — the same EMA, at byte precision, for the model to read.
        // Kept in step with the KiB one on both arms of the flag so a run that
        // flips the flag mid-life does not have to re-learn the rate.
        self.alloc_rate_bytes_per_ms.store(
            blend(
                self.alloc_rate_bytes_per_ms.load(Ordering::Relaxed),
                rate_bytes_per_ms,
            ),
            Ordering::Relaxed,
        );

        // A cycle that completed without running out of to-space is evidence
        // the current margin is adequate; give a quarter of any accumulated
        // penalty back, so a transient burst does not permanently pessimise the
        // threshold.
        let boost = self.ihop_headroom_percent.load(Ordering::Relaxed);
        if boost > 100 {
            self.ihop_headroom_percent
                .store((boost - (boost - 100) / 4).max(100), Ordering::Relaxed);
        }

        self.recompute_marking_threshold();
    }

    /// F-15 — a pause ran out of to-space.
    ///
    /// This is the signal the old model was missing. "Did the concurrent cycle
    /// start early enough that marking finished before the heap filled?" has
    /// exactly one direct observation, and it is this one: an evacuation
    /// failure means the collector could not find room for the objects it was
    /// obliged to copy, which is what running out of heap looks like from
    /// inside a pause.
    ///
    /// Raises the headroom multiplier by 25% (capped), so the next recompute
    /// starts marking correspondingly earlier, and recomputes immediately —
    /// waiting for the next cycle boundary would be waiting for the thing that
    /// is already late.
    pub fn note_to_space_exhausted(&self) {
        self.ihop_late_events.fetch_add(1, Ordering::Relaxed);
        let boost = self.ihop_headroom_percent.load(Ordering::Relaxed);
        // 800% is four doublings of the margin; past that the threshold is
        // pinned to its floor anyway and the multiplier is just a counter.
        self.ihop_headroom_percent
            .store((boost + boost / 4).min(800), Ordering::Relaxed);
        self.recompute_marking_threshold();
    }

    /// F-15 — set the marking threshold from the allocation rate and the
    /// predicted mark duration.
    ///
    /// # Why the old model could not work
    ///
    /// `update_ihop` moved the threshold on YOUNG PAUSE TIME: over the pause
    /// goal, lower it; under half of it, raise it. Pause time is a property of
    /// the young live set and has no causal relationship to the question IHOP
    /// answers. A workload with fast young pauses and a fast-filling old
    /// generation got its threshold RAISED, which is exactly backwards — and
    /// the code's own comment records that failure mode being hit in production
    /// and then fixed by clamping the ceiling rather than by changing the
    /// signal.
    ///
    /// # The model
    ///
    /// Marking takes `mark_ms`, during which the old generation grows at
    /// `alloc_rate`. To finish before the heap fills, the cycle must start with
    /// at least `alloc_rate * mark_ms` of room left — plus a margin, which is
    /// what `ihop_headroom_percent` carries and what to-space exhaustion
    /// raises.
    ///
    /// Until a cycle has been measured both estimates are zero and this leaves
    /// the statically configured threshold alone. That is the right default:
    /// with no measurement, the operator's number is the best available one.
    pub fn recompute_marking_threshold(&self) {
        let mark_ms = self.mark_ms_ema.load(Ordering::Relaxed);
        // LANE W2-C — read the BYTE rate when the measurement fix is armed.
        // See `fold_mark_cycle_sample`: the KiB/ms form floors every rate below
        // 1 MB/s to zero, and a zero rate is what sends this function down the
        // "nothing measured" branch and disengages the controller.
        let rate_bytes_per_ms = if gc_flags().g1_ihop_gross_growth {
            self.alloc_rate_bytes_per_ms.load(Ordering::Relaxed)
        } else {
            self.alloc_rate_kib_per_ms
                .load(Ordering::Relaxed)
                .saturating_mul(1024)
        };
        let ceiling = self.ihop_static_ceiling();
        if mark_ms == 0 || rate_bytes_per_ms == 0 {
            // Nothing measured (or a workload that promotes nothing during
            // marking): the configured IHOP stands, less any lateness penalty —
            // an evacuation failure is evidence about the threshold whether or
            // not a mark cycle has been timed yet.
            let boost = self.ihop_headroom_percent.load(Ordering::Relaxed).max(100);
            let threshold = ((ceiling as u64) * 100 / boost) as usize;
            self.marking_threshold_bytes.store(
                threshold.clamp(self.ihop_floor(), ceiling.max(self.ihop_floor())),
                Ordering::Relaxed,
            );
            return;
        }
        let headroom = rate_bytes_per_ms.saturating_mul(mark_ms) as usize;

        // What the measurement asks for: start with at least one mark cycle's
        // worth of allocation still available.
        //
        // LANE W2-C — and the young generation is not available to the old one.
        //
        // The threshold this produces is compared against `old_gen_bytes`,
        // which counts Old plus Humongous and NOTHING ELSE. Subtracting the
        // headroom from the whole heap therefore hands the old generation the
        // young generation's footprint as if it were free space, and over-states
        // the room available by exactly the size of the young gen. Today the
        // operator's `ihop_percent` ceiling usually swallows the difference
        // (`capped = planned.min(ceiling)`), so it only bites when `headroom`
        // exceeds `100 - ihop_percent` of the heap — which is precisely the
        // fast-allocating case where the model is supposed to take over from
        // the static number.
        //
        // `young_target_regions` is the adaptive young size (F-17/G1AUD-9), so
        // this tracks a young generation that grows and shrinks rather than
        // subtracting a constant that stops meaning anything when it does.
        let young_bytes = if gc_flags().g1_ihop_gross_growth {
            self.young_target_regions
                .load(Ordering::Relaxed)
                .saturating_mul(self.config.region_size)
        } else {
            0
        };
        let planned = self
            .config
            .heap_size
            .saturating_sub(young_bytes)
            .saturating_sub(headroom);

        // The operator's IHOP is a CEILING on that, never a floor. G1 has no
        // full-GC fallback, so a threshold that floats up on thin evidence is
        // how a heap ends up OOMing while mostly dead.
        let capped = planned.min(ceiling);

        // The lateness penalty applies to the RESULT, not to the headroom.
        //
        // Scaling the headroom instead was the first shape and it does not
        // work: on a heap whose configured IHOP already sits well below
        // `heap - headroom` — the common case, since the default is 70% — the
        // ceiling swallows the whole adjustment, so an evacuation failure moved
        // the threshold not at all. The one signal that directly means "the
        // last cycle started too late" has to be able to bite whatever the
        // measurement happened to plan.
        let boost = self.ihop_headroom_percent.load(Ordering::Relaxed).max(100);
        let threshold = ((capped as u64) * 100 / boost) as usize;

        let threshold = threshold.clamp(self.ihop_floor(), ceiling.max(self.ihop_floor()));
        self.marking_threshold_bytes
            .store(threshold, Ordering::Relaxed);
    }

    /// F-15 diagnostics: `(predicted mark ms, old-gen growth KiB/ms, headroom
    /// percent, to-space exhaustion events, disengaged cycles, back-off
    /// suppressions)`.
    ///
    /// LANE W2-C added the last two, and they are the two that say whether the
    /// controller is CONTROLLING. The first four describe a model that is
    /// running; neither of them distinguishes "the measurement says no headroom
    /// is needed" from "the measurement folded zero, so the adaptive threshold
    /// was never computed and the static IHOP is what you are actually getting".
    ///
    /// * `disengaged cycles` — mark cycles that folded `grew == 0`, i.e. told
    ///   the model nothing. A number that climbs with the cycle count means the
    ///   adaptive threshold is inert: see `fold_mark_cycle_sample`.
    /// * `back-off suppressions` — cycles `check_ihop` declined because the old
    ///   generation has not grown since the last cycle reclaimed nothing. A
    ///   number that climbs is the back-off doing its job; a number that climbs
    ///   while the heap is filling is the back-off being wrong, and
    ///   `CRATONVM_G1_IHOP_BACKOFF=0` is the single-binary A/B for that.
    pub fn ihop_model_state(&self) -> (u64, u64, u64, u64, u64, u64) {
        (
            self.mark_ms_ema.load(Ordering::Relaxed),
            self.alloc_rate_kib_per_ms.load(Ordering::Relaxed),
            self.ihop_headroom_percent.load(Ordering::Relaxed),
            self.ihop_late_events.load(Ordering::Relaxed),
            self.ihop_disengaged_cycles.load(Ordering::Relaxed),
            self.mark_backoff_suppressions.load(Ordering::Relaxed),
        )
    }

    /// LEGACY (pre-F-15) pause-time-driven IHOP, retained as the `=0` arm of
    /// `CRATONVM_G1_ADAPTIVE_IHOP` so the change has a single-binary A/B.
    ///
    /// See [`Self::recompute_marking_threshold`] for why this signal cannot
    /// answer the question it was wired to.
    pub fn update_ihop(&self, actual_pause_ms: u64) {
        let target = self.config.max_gc_pause_ms;
        let current_threshold = self.marking_threshold_bytes.load(Ordering::Relaxed);

        let new_threshold = if actual_pause_ms > target {
            // Pause too long: lower threshold to start marking earlier —
            // but FLOOR the decay at 1% of the heap (min one region). With
            // no floor, a chronically-slow host (every pause > target)
            // decays the threshold to 0 through integer truncation; the
            // raise branch can never recover it (0 * 1.05 == 0) and the
            // VM-side trigger gate (`g1_should_start_marking` requires
            // `marking_threshold_bytes() > 0`) then reads a zero threshold
            // as "marking disabled" — permanently: no cycle ever starts,
            // Old/humongous garbage is never reclaimed, and the heap OOMs
            // while mostly dead.
            // gci i13/ef finding 11: the same floor `ihop_floor` states.
            let floor = self.ihop_floor();
            ((current_threshold as f64 * 0.9) as usize).max(floor)
        } else if actual_pause_ms < target / 2 {
            // Pause well under target: raise threshold — but NEVER above the
            // statically-configured IHOP. G1 has no full-GC fallback: letting
            // fast young pauses defer marking indefinitely means dead promoted
            // objects accumulate in Old unreclaimed until the heap is
            // exhausted. Observed on the SteadyChurn recreation: ~7ms pauses
            // raised the threshold 5% per collection to the old 90%-of-heap
            // cap, concurrent marking never started across 61k young
            // collections, and every heap size died with a true OOM while
            // >80% of Old was garbage. The adaptive threshold may float
            // BELOW the configured IHOP (long pauses → mark earlier) and
            // recover back up to it, no further.
            // gci i13/ef finding 11: the same ceiling `ihop_static_ceiling`
            // states.
            let max = self.ihop_static_ceiling();
            ((current_threshold as f64 * 1.05) as usize).min(max)
        } else {
            current_threshold
        };

        self.marking_threshold_bytes
            .store(new_threshold, Ordering::Relaxed);
    }

    // -----------------------------------------------------------------------
    // Region Pinning (JEP 423)
    // -----------------------------------------------------------------------

    /// Pin a region, preventing it from being evacuated during GC. Refcounted:
    /// each `pin_region` must be balanced by exactly one [`unpin_region`], so
    /// overlapping JNI critical sections on the same region pin/unpin
    /// independently.
    ///
    /// gci i16/pl (finding 8 of
    /// `gci-i15ra-g1-evacuation-additions-review-findings-FIXED-20261016`): a pin
    /// on a region that is ALREADY pinned is one atomic increment under the
    /// shared regions guard (`G1Region::critical_extra_pins`), as HotSpot's
    /// `G1HeapRegion::increment_pinned_object_count` is; only the first pin
    /// (which flips `pinned`, read by every CSet filter) takes the write
    /// guard. A pause holds the write guard for its whole CSet choice, so no
    /// shared-guard pin can land inside one.
    pub fn pin_region(&self, region_idx: usize) {
        {
            let regions = self.regions.read();
            if region_idx >= regions.len() {
                return;
            }
            if regions[region_idx].try_pin_shared() {
                return;
            }
        }
        let mut regions = self.regions.write();
        if region_idx < regions.len() {
            regions[region_idx].pin_exclusive();
        }
    }

    /// Unpin a region, allowing it to be collected again once its pin count
    /// returns to zero. Saturating: a call with the count already zero is a
    /// no-op. It does NOT tell sections apart -- an extra call while two
    /// sections hold the region drops the OTHER section's pin (gci i15/ra
    /// finding 6). The JNI critical path never makes one: a release unpins
    /// only after taking its own record (`vm/src/native/jni.rs`
    /// `take_critical_pin`), so a stray double `Release` finds no record and
    /// unpins nothing. Any new caller must keep that pairing.
    ///
    /// gci i16/pl (finding 8): an unpin that is NOT the region's last is one
    /// atomic decrement under the shared regions guard; the last one (which
    /// clears `pinned`) takes the write guard. A shared-guard unpin never
    /// takes the total to zero, so a region is never unpinned without the
    /// write guard and no CSet filter can see a half-released pin.
    pub fn unpin_region(&self, region_idx: usize) {
        {
            let regions = self.regions.read();
            if region_idx >= regions.len() {
                return;
            }
            if regions[region_idx].try_unpin_shared() {
                return;
            }
        }
        let mut regions = self.regions.write();
        if region_idx < regions.len() {
            regions[region_idx].unpin_exclusive();
        }
    }

    /// Pin the region backing the object at `addr` and return its index for the
    /// matching [`unpin_region`], or `None` if `addr` is not in any region.
    /// Refcounted via [`pin_region`]. No bound check of the object's extent:
    /// a JNI critical section uses [`Self::pin_array_payload_for_critical`].
    ///
    /// gci i14/pn (docs corrected): every G1 path that moves or frees an
    /// object honours the pin, by region. The young / mixed CSet filters
    /// (`young_collection_serial`, `young_collection_parallel`,
    /// `mixed_collection`, `mixed_collection_parallel`, `choose_old_cset`) and
    /// the full-compaction pause's old half (`full_compaction_candidates`,
    /// armed by [`Self::arm_full_compaction`]) skip a pinned region; the
    /// humongous compaction's planners (`plan_humongous_compaction`,
    /// `plan_humongous_staging`) refuse a span with a pinned region; the
    /// humongous reclaimers (`reclaim_dead_humongous_spans_locked`,
    /// `eager_reclaim_humongous_locked`) and `cleanup` never free one; the
    /// evacuation-failure drain (`retry_after_evacuation_failure`) only re-runs
    /// regions of a CSet, which never holds one; and the Phase-4 fix-up walks a
    /// pinned region (`phase4_regions_to_walk`). The production caller is
    /// `GetPrimitiveArrayCritical` / `GetStringCritical` under
    /// `CRATONVM_JNI_CRITICAL_G1_PIN` (`vm/src/native/jni.rs`); without it the
    /// JNI critical functions hand out a detached copy and pin nothing (gc-common
    /// w2-c), and this function has only test callers.
    pub fn pin_region_for_addr(&self, addr: usize) -> Option<usize> {
        let idx = self.lookup_region_for_addr(addr)?;
        self.pin_region(idx);
        Some(idx)
    }

    /// gci i14/pn (`CRATONVM_JNI_CRITICAL_G1_PIN`): the G1 half of a JNI
    /// critical section that hands native code the array's own body, as
    /// HotSpot 25 G1 does (JEP 423: pin the region, `isCopy` false).
    ///
    /// `obj_addr` is the array's address; its payload is the
    /// `payload_bytes` bytes at `obj_addr + payload_offset` (the caller's
    /// `VmHeap::array_data_ptr`). Pins the region holding the object's header
    /// and returns its index for the matching [`Self::unpin_region`] -- in ONE
    /// write-locked step with the check that the payload is one contiguous
    /// block the pin covers:
    ///
    /// * an Eden / Survivor / Old object: the payload ends inside the header's
    ///   region (every object of those regions does; the check refuses a
    ///   corrupt length rather than pinning half an array);
    /// * a humongous object (`HumongousStart`): every region the payload
    ///   reaches is a `HumongousContinuation` of the span (the arena is one
    ///   block, so the span is contiguous). Pinning the start region is enough:
    ///   every humongous mover and reclaimer tests the whole span for a pin.
    ///
    /// `None` (nothing pinned) for an address outside the arena, a Free or
    /// continuation region, or a payload the checks refuse: the caller then
    /// falls back to the detached copy. The caller must hold the array
    /// reachable (a JNI global ref) and must not be at a safepoint between
    /// resolving `obj_addr` and this call (a JNIEnv function runs as a
    /// counted mutator: no pause can move the array in between).
    pub fn pin_array_payload_for_critical(
        &self,
        obj_addr: usize,
        payload_offset: usize,
        payload_bytes: usize,
    ) -> Option<usize> {
        let idx = self.lookup_region_for_addr(obj_addr)?;
        let start = obj_addr.checked_add(payload_offset)?;
        let end = start.checked_add(payload_bytes)?;
        if end > self.arena_end {
            return None;
        }
        // gci i16/pl (finding 8): a region another section already pins is
        // pinned again under the SHARED guard -- the contiguity check reads
        // only region types (atomic) and the arena geometry, and a pause,
        // which is what could change a region's type under a live array,
        // cannot run while the shared guard is held.
        {
            let regions = self.regions.read();
            if critical_payload_is_covered(&regions, idx, self.arena_base, self.region_shift, end)
                && regions[idx].try_pin_shared()
            {
                return Some(idx);
            }
        }
        let mut regions = self.regions.write();
        let contiguous = critical_payload_is_covered(
            &regions,
            idx,
            self.arena_base,
            self.region_shift,
            end,
        );
        if !contiguous {
            return None;
        }
        regions[idx].pin_exclusive();
        Some(idx)
    }

    /// Check if a region is pinned.
    pub fn is_pinned(&self, region_idx: usize) -> bool {
        let regions = self.regions.read();
        region_idx < regions.len() && regions[region_idx].pinned
    }

    // -----------------------------------------------------------------------
    // String Deduplication
    // -----------------------------------------------------------------------

    /// Attempt to deduplicate a String object. The hash is used to look up
    /// a canonical instance.
    ///
    /// Returns:
    /// - `Some(canonical_addr)` on a HIT: the caller should redirect its
    ///   pointer at `addr` to `canonical_addr` and discard the duplicate.
    /// - `None` on a MISS (or when dedup is disabled): the caller's
    ///   `addr` was registered as the canonical instance for `hash`;
    ///   nothing to redirect.
    ///
    /// Audit fix (HIGH): the previous implementation ignored the canonical
    /// address on a hit and only returned a `bool`, so the caller had no
    /// way to actually perform the dedup. Returning the canonical address
    /// makes the function correct on its own terms.
    ///
    /// # G1CORE-6 — CLOSED (2026-08-18)
    ///
    /// The table stores RAW canonical object addresses, and until this date no
    /// collection path remapped or purged them: a canonical String registered
    /// in Eden was evacuated or freed by the very next young pause, after which
    /// a HIT returned a dangling from-space address the caller would install as
    /// a live reference (UAF). That is why the API carried a "DO NOT WIRE UP"
    /// warning; `string_dedup_enabled` plumbs from
    /// `-XX:+UseStringDeduplication` but nothing invoked it.
    ///
    /// The two missing halves now exist and run unconditionally:
    ///
    /// * [`Self::remap_string_dedup_table`] — every evacuation pause (serial
    ///   and parallel, young and mixed) re-keys survivors through the pointer
    ///   map and DROPS entries whose object died in the collection set, at the
    ///   same protocol point as the monitor-table and referent-skip remaps;
    /// * [`Self::purge_dead_string_dedup_entries`] — `cleanup` drops entries
    ///   whose canonical object's region it just freed in place, which is the
    ///   one death a pause leaves no forwarding entry for.
    ///
    /// A caller is therefore safe to add. What it still owes: dedup only
    /// Strings whose `value` array it is willing to share, and re-check the
    /// returned canonical is still the string it wants (the table is keyed by
    /// hash alone, so a HIT is a hash match, NOT an equality proof).
    pub fn deduplicate_string(&self, hash: u64, addr: usize) -> Option<usize> {
        if !self.config.string_dedup_enabled {
            return None;
        }
        let mut table = self.string_dedup_table.lock();
        if let Some(&canonical) = table.get(&hash) {
            // Hit: caller should redirect `addr` -> `canonical`.
            // If `addr` happens to already be the canonical (idempotent
            // caller), still return Some so the API stays uniform.
            Some(canonical)
        } else {
            // Miss: register `addr` as the canonical instance.
            table.insert(hash, addr);
            None
        }
    }

    // -----------------------------------------------------------------------
    // GC Logging
    // -----------------------------------------------------------------------

    /// Ten-findings item 10 — the copy-time budget for the OLD half of a mixed
    /// CSet: the pause goal minus the fix-up walk's expected cost
    /// (`fixup_ns_ema`). The selectors always take at least one old region for
    /// forward progress, so a budget of zero still makes progress; it just
    /// stops padding a pause the walk has already spent.
    ///
    /// # LANE E wave 2 — and minus the YOUNG half, under the cost model flag
    ///
    /// `lane-e-the-pause-goal-still-does-not-price-the-fixup-walk-it-causes.md`
    /// records a second, sharper gap than the one item 10 closed: **the young
    /// half of the collection set is not priced at all.** Every Eden and
    /// Survivor region is in the CSet unconditionally, so the copy this budget
    /// is spending the whole goal on is the *second* copy of the pause. On a
    /// large young generation a mixed pause therefore budgets its entire goal
    /// for old regions it cannot afford, and overruns by whatever the young
    /// half cost.
    ///
    /// Under `CRATONVM_G1_PAUSE_COST_MODEL` the young half is charged FIRST,
    /// from `young_copy_ns_ema` — measured on young-only pauses, where the
    /// young half is the whole of the copying and so the only place it is
    /// separable. The budget floors at zero and the selectors still take one
    /// region unconditionally, so a goal already spent means "one region, for
    /// forward progress", never "no mixed phase ever".
    pub fn old_cset_copy_budget_ns(&self) -> u64 {
        self.old_cset_copy_budget_ns_within(g1_pause_cost_model())
    }

    /// [`Self::old_cset_copy_budget_ns`] with the model as a parameter.
    pub fn old_cset_copy_budget_ns_within(&self, model: bool) -> u64 {
        let goal = self.config.max_gc_pause_ms.saturating_mul(1_000_000);
        let after_fixup = goal.saturating_sub(self.fixup_ns_ema.load(Ordering::Relaxed));
        if !model {
            return after_fixup;
        }
        after_fixup.saturating_sub(self.young_copy_ns_ema.load(Ordering::Relaxed))
    }

    /// LANE E wave 2 — what ONE more walked region costs the fix-up phase, in
    /// nanoseconds.
    ///
    /// `fixup_ns_ema / fixup_regions_ema` — the mean over the recent pauses, as
    /// `lane-e-the-pause-goal-still-does-not-price-the-fixup-walk-it-causes.md`
    /// proposes. Zero until both EMAs are warm, which makes the marginal model
    /// degrade exactly into the fixed-budget model rather than into something
    /// new: an unmeasured cost is charged at nothing, the same assumption the
    /// pre-wave-2 selector made about all of it.
    ///
    /// # Why a mean per region and not something better
    ///
    /// The honest answer is that the walk's cost is proportional to the
    /// OCCUPANCY of the regions walked, not to their count, and this prices
    /// them all at the average. Lane W2-B is reworking Phase 4 this wave
    /// (`lane-b-phase4-rewrite-half-is-redundant-with-phase-2.md`) and its
    /// conclusion changes what the right denominator is: if the (A) rewrite
    /// half becomes card-screened, a walked region's cost stops tracking its
    /// occupancy and starts tracking its DIRTY CARD COUNT, and a per-byte model
    /// built now would be wrong in a new way the moment that lands. A mean per
    /// region survives that change; it is simply less sharp. `phases.fixup_bytes`
    /// is already published per pause, so the per-byte refinement is a
    /// measurement away whenever Phase 4 settles.
    pub fn fixup_ns_per_region(&self) -> u64 {
        let total = self.fixup_ns_ema.load(Ordering::Relaxed);
        let regions = self.fixup_regions_ema.load(Ordering::Relaxed);
        if total == 0 || regions == 0 {
            return 0;
        }
        total / regions
    }

    /// Step 7 — refresh the rolling `evac_ns_per_byte` calibration from a
    /// completed mixed collection. Observed cost = whole pause / bytes copied
    /// (deliberately includes the fixed scan overhead, so it slightly
    /// *over*-estimates → a smaller, safer collection set). Smoothed with a
    /// slow EMA (1/8 weight on the new sample) and clamped to a sane band so a
    /// single anomalous cycle cannot wreck the estimate. No-op when nothing was
    /// copied (no signal).
    pub fn update_evac_cost(&self, pause_ns: u64, bytes_copied: usize) {
        if bytes_copied == 0 || pause_ns == 0 {
            return;
        }
        let observed = (pause_ns / bytes_copied as u64).clamp(1, 4096);
        let prev = self.evac_ns_per_byte.load(Ordering::Relaxed).max(1);
        let next = (prev.saturating_mul(7).saturating_add(observed)) / 8;
        self.evac_ns_per_byte.store(next.max(1), Ordering::Relaxed);
    }

    /// LANE W5-A — fold one per-byte copy-cost observation into `term`.
    ///
    /// The shared body of both calibrations. Three things differ from
    /// [`Self::update_evac_cost`], and each is a separate defect it was
    /// carrying:
    ///
    /// **1. The numerator is the CLOSURE, not the pause.** The budget this
    /// term is spent against is `goal - fixup_ns_ema - young_copy_ns_ema`
    /// ([`Self::old_cset_copy_budget_ns`]): the fixed and separately-priced
    /// phases have ALREADY been subtracted before a single byte is priced. A
    /// per-byte term derived from the whole pause therefore charges the roots
    /// scan, the remembered-set walk, the fix-up and the free phase a second
    /// time, amortised over whatever the pause happened to copy — so the
    /// inflation is not a conservative margin, it is an arbitrary multiplier
    /// set by the pause's own copy/overhead ratio. On `HumongousChurn`, where a
    /// young pause copies almost nothing and walks the heap wide, that
    /// multiplier is what took the W4-B probe's reading to 38 ns/byte. 38 is
    /// not the cost of copying a byte on this host; it is the cost of a pause
    /// divided by a small number.
    ///
    /// **2. The first real sample is adopted WHOLE.** `update_evac_cost` starts
    /// from the constructor value and moves an eighth of the way per sample, so
    /// reaching a true value seven times higher takes ~20 mixed pauses. A mixed
    /// PHASE is typically a handful of pauses and most runs have none at all,
    /// which is why W4-B's probe was still climbing at the last pause of the
    /// run: the smoothing could not converge faster than the workload ended.
    /// A 1/8 EMA is the right answer for tracking drift between samples and the
    /// wrong answer for the step from "a guess" to "a measurement", and those
    /// are different operations. `samples` is what tells them apart.
    ///
    /// **3. A sample needs a real denominator.** See
    /// [`G1_EVAC_COST_MIN_SAMPLE_BYTES`]. Refusals are counted, not clamped —
    /// and this is the change with the largest measured effect of the three,
    /// which was not expected. Replaying four probes' `[GC-STAT]` streams
    /// offline through W4-B's ungated estimator ends at 651, 1352 and 640
    /// ns/byte, two to three orders of magnitude above those workloads' real
    /// copy cost. The mechanism is visible in the trajectory: the estimate sits
    /// near 3 for a dozen pauses and then steps to ~513 in ONE pause, because a
    /// pause that copied a few hundred bytes produces a ratio the `4096` clamp
    /// caps rather than rejects, and `(prev * 7 + 4096) / 8` is ~512 whatever
    /// `prev` was. It never comes back, because the recovery is also an eighth
    /// per sample and the run ends first. A clamp turns a meaningless sample
    /// into a large plausible one; only a refusal keeps it out.
    ///
    /// The live binary reproduces it on two workloads, which is what makes this
    /// a defect in the estimator rather than an artefact of the replay:
    /// `evac_ns_per_byte` at exit under `CRATONVM_G1_EVAC_COST_FROM_YOUNG=1`
    /// reads `409, 528, 530` on `HumongousRealChurn` (real copy cost 3) and
    /// `591, 606` on `G1OldBurstProbe`.
    ///
    /// That also retro-explains W4-B's own `4 -> 5 -> 6 -> 23 -> 38`
    /// trajectory: the 6 -> 23 step is this clamp event, smaller only because
    /// the run had five pauses rather than thirty. So `27` was never "the
    /// truth, still climbing" — it was a runaway that had not finished.
    ///
    /// The clamp band `[1, 4096]` is kept from `update_evac_cost`: a term that
    /// reads zero would let a collection set take the whole heap, and one that
    /// reads above 4096 ns/byte (~244 KB/s) is measuring something that is not
    /// copying.
    pub fn fold_evac_cost(&self, term: &AtomicU64, samples: &AtomicU64, copy_ns: u64, bytes: usize) {
        if copy_ns == 0 {
            return;
        }
        if bytes < G1_EVAC_COST_MIN_SAMPLE_BYTES {
            self.evac_cost_samples_refused
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        let observed = (copy_ns / bytes as u64).clamp(1, 4096);
        let taken = samples.fetch_add(1, Ordering::Relaxed);
        let next = if taken == 0 {
            observed
        } else {
            let prev = term.load(Ordering::Relaxed).max(1);
            (prev.saturating_mul(7).saturating_add(observed)) / 8
        };
        term.store(next.max(1), Ordering::Relaxed);
    }

    /// LANE W5-A — the mixed drivers' door to the copy-cost calibration, with
    /// both definitions of the observation behind one call.
    ///
    /// `pause_ns` is the whole pause (the pre-wave-5 numerator) and `closure_us`
    /// is Phase 3 alone (the wave-5 one). Both are passed so that the OFF arm
    /// is the previous arithmetic exactly — `elapsed.as_nanos()` and not
    /// `pause_us * 1000`, which differs from it by up to a microsecond and
    /// therefore by up to one whole ns/byte on a pause that copied little.
    /// "Byte-for-byte unchanged with the flag off" is a claim this round has
    /// had to retract measurements over; it is cheaper to pass two numbers.
    pub fn update_evac_cost_for_mixed(&self, pause_ns: u64, closure_us: u64, bytes_copied: usize) {
        if !g1_evac_cost_real() {
            self.update_evac_cost(pause_ns, bytes_copied);
            return;
        }
        self.fold_evac_cost(
            &self.evac_ns_per_byte,
            &self.evac_cost_samples,
            closure_us.saturating_mul(1_000),
            bytes_copied,
        );
    }

    /// LANE W5-A — the price, in nanoseconds per live byte, that a mixed
    /// collection set charges a candidate OLD region.
    ///
    /// The one read site for `evac_ns_per_byte`, so that the bootstrap below
    /// cannot be applied on one selector and forgotten on the other — the two
    /// mixed drivers and the public `select_old_regions_for_mixed_gc` each
    /// loaded the field directly, which is three places for a policy to drift
    /// apart in.
    ///
    /// # Why the young term may stand in for the old one, and only then
    ///
    /// `update_evac_cost` is reached only from a mixed collection, and a mixed
    /// collection needs a completed mark cycle first. So on the pause where the
    /// budget matters most — the FIRST mixed pause after a mark cycle, when the
    /// candidate set is largest and nothing has been retired yet — the term has
    /// never once been written. That is not "most runs never calibrate it"; it
    /// is strictly worse, because it holds even on a workload that does run
    /// mixed collections. Every mixed phase begins with the pause goal priced
    /// by a constructor value.
    ///
    /// The young calibration is available at that moment (every young pause
    /// takes a sample) and it is a measurement of THIS host copying THIS
    /// workload's objects, which a compile-time 4 is not. It is not the same
    /// quantity — see [`Self::young_evac_ns_per_byte`] for the three ways the
    /// populations differ — so it is used as a starting point and replaced by
    /// the old-copy measurement the instant one exists, rather than blended
    /// into it forever.
    ///
    /// The direction of the substitution's error is worth stating because it is
    /// the one that matters for a pause GOAL. Old-region copying is priced per
    /// LIVE byte while the copy also steps over the region's garbage, and the
    /// candidates are selected for having a lot of it; young copying is dense.
    /// So the young term is expected to be a LOWER bound on the old one, and
    /// substituting it still under-prices the collection set — just by a factor
    /// this collector can measure and shrink rather than by a factor nobody had
    /// looked at. It is a floor under the defect, not a closure of it, and the
    /// next step (separating the two halves of a mixed closure using this term)
    /// is written up in `w5a-the-constant-nothing-writes.md`.
    pub fn old_evac_ns_per_byte(&self) -> u64 {
        let old = self.evac_ns_per_byte.load(Ordering::Relaxed);
        if !g1_evac_cost_real() {
            return old.max(1);
        }
        if self.evac_cost_samples.load(Ordering::Relaxed) > 0 {
            return old.max(1);
        }
        let young = self.young_evac_ns_per_byte.load(Ordering::Relaxed);
        if young > 0 {
            return young.max(1);
        }
        old.max(1)
    }

    /// LANE W5-A — the copy-cost calibration state, as one tuple:
    /// `(old_ns_per_byte, old_samples, young_ns_per_byte, young_samples,
    /// refused, effective_old_price)`.
    ///
    /// Public because the whole finding is that a number printed without its
    /// sample count cannot be read. `[GC-STAT]`, the `[GC] g1 young-sizing:`
    /// summary and the tests all render the same tuple.
    pub fn evac_cost_state(&self) -> (u64, u64, u64, u64, u64, u64) {
        (
            self.evac_ns_per_byte.load(Ordering::Relaxed),
            self.evac_cost_samples.load(Ordering::Relaxed),
            self.young_evac_ns_per_byte.load(Ordering::Relaxed),
            self.young_evac_cost_samples.load(Ordering::Relaxed),
            self.evac_cost_samples_refused.load(Ordering::Relaxed),
            self.old_evac_ns_per_byte(),
        )
    }

    /// Account for one completed STW collection with NO phase breakdown.
    ///
    /// **Test convenience only since F-07.** Every production evacuation driver
    /// — both young, both mixed, and the kept-region drain — now measures its
    /// own phases and calls [`Self::record_collection_with_phases`] directly. A
    /// production caller here would record an all-zero breakdown, which the
    /// `[GC-STAT]` renderer suppresses entirely, so the pause would silently
    /// stop being attributable. That is the state the parallel driver (the
    /// DEFAULT one) was in until F-07.
    ///
    /// `pause_us` is `Instant::elapsed().as_micros()` — see `G1PauseRecord`.
    pub fn record_collection(&self, collection_type: G1CollectionType, pause_us: u64, stats: &GcStats) {
        self.record_collection_with_phases(
            collection_type,
            pause_us,
            stats,
            G1PausePhases::default(),
        )
    }

    /// [`Self::record_collection`] plus the per-phase breakdown (G1AUD-10).
    /// The evacuation paths call this; everything else keeps the plain form,
    /// which records an all-zero breakdown meaning "not instrumented" rather
    /// than "took no time".
    pub fn record_collection_with_phases(
        &self,
        collection_type: G1CollectionType,
        pause_us: u64,
        stats: &GcStats,
        phases: G1PausePhases,
    ) {
        self.record_collection_with_phases_within(
            collection_type,
            pause_us,
            stats,
            phases,
            true,
            g1_evac_cost_real(),
        )
    }

}
