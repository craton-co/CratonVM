#[allow(unused_imports)]
use super::*;

impl GenerationalHeap {
    /// Non-moving young-generation mark-sweep, used when JIT frames are
    /// active and a moving (Cheney) collection would be unsafe.
    ///
    /// Unlike [`Self::collect_garbage_inner`]'s copying collector, this
    /// **never relocates an object**. Survivors keep their exact
    /// addresses, so every raw pointer held in a JIT spill slot or
    /// register stays valid regardless of whether the GC could describe
    /// it precisely. Dead young objects are reclaimed into the
    /// from-space arena's free list (see [`Arena::add_free_block`]).
    ///
    /// ## Correctness
    ///
    /// Marking must be **complete** — a non-moving sweep frees every
    /// young object that is not marked, so a missed root would free a
    /// live object. The root set is:
    ///
    ///   * `roots` — every precise VM root the caller gathered
    ///     (interpreter frame locals/stacks, statics, JNI handles, class
    ///     locks, autobox caches, …) **plus** the conservative
    ///     JIT-frame roots that `collect_roots` already appended via
    ///     `conservative_roots::scan_active_jit_frames`.
    ///   * old→young references discovered by scanning dirty cards.
    ///   * `finalizer_addrs` — finalizable objects kept alive so their
    ///     `finalize()` can run.
    ///
    /// A conservative false-positive root only over-retains an object;
    /// it can never cause a live object to be freed. Because nothing
    /// moves, a stack word that merely *looks* like a pointer is never
    /// rewritten, so the mutator's non-pointer data is never corrupted.
    ///
    /// Returns an empty pointer map (no object moved) and the list of
    /// resurrected dead-finalizer addresses (unchanged, since they too
    /// stay in place).
    pub fn sweep_young_non_moving(
        &self,
        roots: &[ObjectRef],
        finalizer_addrs: &[usize],
        promote_pressure: bool,
    ) -> (GcResult, Vec<usize>) {
        SP_CENSUS.sweeps.fetch_add(1, Ordering::Relaxed);
        let phase_diag = gc_flags().dbg_gcphase;
        let phase_start = std::time::Instant::now();
        let mut phase_last = phase_start;
        let mut report_phase = |name: &str| {
            if phase_diag {
                let now = std::time::Instant::now();
                eprintln!(
                    "[gcphase] {name}: phase={}ms total={}ms",
                    now.duration_since(phase_last).as_millis(),
                    now.duration_since(phase_start).as_millis(),
                );
                phase_last = now;
            }
        };
        if watchref_dbg() {
            eprintln!("[watchref] sweep_young_non_moving ENTRY (non-moving path taken)");
        }
        let mut young_from = self.lock_young_from();
        let mut old_gen = self.old_gen.lock();
        old_gen.note_collection_prologue();
        debug_assert_young_arena_is_low_only(&young_from, "sweep_young_non_moving");

        // "Zeroed-a-live-object" detector cycle stamp (CRATONVM_DBG_SWEEP_ZERO).
        let sweep_zero_cycle = SWEEP_ZERO_CYCLE.fetch_add(1, Ordering::Relaxed) as u32;

        // Fold every mutator's thread-local card buffer into the bitmap
        // before scanning dirty cards (same protocol as the moving path).
        self.card_table.flush_all();
        self.card_table.drain_pending();

        // Seed: old→young references from dirty cards. Reuse the existing
        // dirty-card scanner, which yields (old_obj, slot_idx, _) tuples;
        // the mark reads the referenced young object out of each slot below.
        //
        // gen r5w2/roots6: scanned HERE, before the conservative-candidate
        // oracle is built (it used to run just after the root seed). The scan
        // also returns the young values it found in old-gen WALK GAPS -- bytes
        // behind a header no walk can size (`CardScanGapRoots`, gen
        // r5w1/oldgen5). Each is a CONSERVATIVE root: kept alive through the
        // interior-aware `mark_young` (so it must be among the oracle's
        // candidates, or a gap word naming an object in a proved span would be
        // dropped as "gap space"), pinned against selective promotion, never
        // rewritten, and its card kept dirty. Empty on a sane heap. Nothing
        // between here and the old position touches a card or old gen.
        let mut extra_roots: Vec<(ObjectRef, usize, usize)> = Vec::new();
        let card_gap_roots =
            Self::scan_dirty_cards(&self.card_table, &old_gen, &young_from, &mut extra_roots);

        let bytes_before = young_from.used();
        // The live occupancy at entry (`used` less the holes earlier sweeps
        // left), which the promote-on-pressure verdict below judges survival
        // against.
        let occupied_before = bytes_before.saturating_sub(young_from.free_list_bytes());
        // gen r4w2/youngmark: the promotion counters at entry, so the result can
        // report what THIS cycle copied (selective promotion is the only copy
        // on this path). Net of unwound candidates — `unwind_evac` retracts them.
        let promoted_before = (
            self.stats.objects_promoted.load(Ordering::Relaxed),
            self.stats.bytes_promoted.load(Ordering::Relaxed),
        );
        let from_base = young_from.base_ptr() as usize;
        let from_end = from_base + young_from.used();

        // BUG-03 — reserved tails of forcibly-stopped in-JIT peers' TLABs,
        // as young-from offsets. Empty on the normal path. Every linear
        // from-space walk below merges these into its free-block skip list so
        // an un-retired tail is neither walked as objects nor reclaimed.
        let jit_skips = self.jit_tlab_skip_offsets(from_base, from_end);
        // ----- Invariant: a published skip span holds NO ROOT ---------------
        //
        // A root pointing into a skip span DISPROVES the span; see
        // `cratonvm_gc_core::heap::skip_spans_hold_no_root` for why, and for why it
        // reports rather than repairs. `jit_skips` are young-from OFFSETS and
        // the shared check takes absolute `[start, end)` pairs, so convert.
        if !jit_skips.is_empty() {
            let abs: Vec<(usize, usize)> = jit_skips
                .iter()
                .map(|&(soff, ssz)| (from_base + soff, from_base + soff + ssz))
                .collect();
            cratonvm_gc_core::heap::skip_spans_hold_no_root(&abs, roots, "generational");
        }
        // Merge a sorted free-block list with `jit_skips` (both ascending,
        // disjoint — a TLAB tail is reserved, never on the free list). Cheap;
        // the skip list has at most one entry per live thread.
        let merge_skips = |free: Vec<(usize, usize)>| -> Vec<(usize, usize)> {
            if jit_skips.is_empty() {
                return free;
            }
            let mut v = free;
            v.extend_from_slice(&jit_skips);
            v.sort_by_key(|&(off, _)| off);
            v
        };

        // Helper: is `addr` the start of a young from-space object?
        let in_young =
            |addr: usize| -> bool { addr >= from_base && addr < from_end && (addr & 0x7) == 0 };

        // Build exact young-object bases before marking. The stack/JIT root
        // scan is conservative and can yield aligned interior words; treating
        // those as objects makes the side-mark channel retain the interior
        // address rather than its containing object, so the later sweep can
        // reclaim the real object.
        // PERF (perf/halfgap-20260717): the exact-base oracle exists for
        // CONSERVATIVE candidates only — the aligned interior words the
        // stack/register/pin scans produce (`roots`, `finalizer_addrs`).
        // Precise heap edges (the BFS `for_each_ref_slot` values, dirty-card
        // slot reads, overlay/loader/mirror side channels) hold object BASES
        // by construction and never needed interior-pointer mapping — they
        // take `mark_young_precise` below, exactly the pre-oracle behavior.
        //
        // That containment makes the oracle cheap to build: only ranges
        // covering an actual candidate are materialized (a handful per
        // cycle), and the builder walk EARLY-EXITS past the last candidate.
        // The previous shape materialized a `(start, end)` pair for EVERY
        // young object and routed every BFS edge through a binary search of
        // that Vec — on a 2 GiB young gen that is a ~50M-entry, ~800 MB Vec
        // rebuilt per collection plus a cache-hostile log2(50M) probe per
        // edge, which dominated the whole mark phase.
        let mut conservative_candidates: Vec<usize> = roots
            .iter()
            .map(|r| r.as_ptr() as usize)
            .chain(finalizer_addrs.iter().copied())
            // gen r5w2/roots6: the dirty-card scan's walk-gap words.
            .chain(card_gap_roots.young_words.iter().copied())
            .filter(|&a| in_young(a))
            .collect();
        conservative_candidates.sort_unstable();
        conservative_candidates.dedup();

        let free_at_start = young_from.free_blocks_sorted();
        // gcj j2/yg2 (`CRATONVM_GEN_YOUNG_KNOWN_ZERO_CARRY`): the bytes of the
        // previous known-zero set still on the free list. Taken here, before
        // this sweep's publication (`add_free_block`) forgets the set; nothing
        // between here and the install below writes a free-list byte (the
        // walk skips the free list, promotion copies into the old generation,
        // and the mutators are stopped). Empty with the switch off.
        let carried_known_zero = if self.young_known_zero_carry {
            young_from.known_zero_within(&free_at_start)
        } else {
            Vec::new()
        };
        let exact_skips = merge_skips(free_at_start);
        let used_bytes = young_from.used();

        // ----- Object-grid anchors -------------------------------------------
        //
        // perf/gc-oracle-anchors (2026-07-25). Both consumers of an "object
        // grid split point" -- the parallel sweep's chunk boundaries and this
        // oracle's exact-base lookup -- used to get them from ONE sequential
        // linear header chase over the whole from-space, right here. On the
        // bintrees-18 profile (`-Xmx8g`, a full 2 GiB from-space, 8 threads)
        // that chase measured 233-262 ms and was the largest single item left
        // in the young pause -- larger than the parallel sweep it fed (68 ms)
        // and larger than mark-card-seed + mark-closure combined.
        //
        // It was also redundant: the ALLOCATOR already knows every boundary it
        // hands out. Anchors are now collected from three O(1)-per-event
        // sources instead of being rediscovered by chasing
        // `gen_object_total_size` over ~36M objects:
        //
        //   1. offset 0 -- where any linear from-space walk starts, valid even
        //      if the arena opens with a free block (every walker skips those).
        //   2. `Arena::take_alloc_anchors` -- bases the allocator handed out
        //      during the epoch that just ended: one per bucket per epoch, both
        //      TLAB starts and non-TLAB young objects. See the field comment on
        //      `Arena::alloc_anchors` for why a TLAB start is an object base.
        //   3. The END of every pre-existing free/TLAB-skip block. A sweep
        //      coalesces dead spans up TO a survivor and never past one, so the
        //      byte after a free block is that survivor's base. This is the
        //      source that covers regions which survived an EARLIER collection
        //      and were therefore never re-handed-out by (2) -- without it,
        //      anchor coverage would decay to "this epoch's allocations only".
        //   4. `used` -- the bump frontier, so the final chunk has an upper
        //      bound to chain onto and the sequential sweep tail is empty.
        //
        // WHY A BAD ANCHOR CANNOT CORRUPT. Every consumer re-proves an anchor
        // by chaining onto the NEXT one: the sweep's `sweep_chunk` returns
        // `None` unless its chain lands exactly on its upper bound (and the
        // whole parallel attempt is then discarded, writing nothing), and the
        // per-interval walk below discards its own ranges on the same test.
        // So a MISSING anchor only lengthens a chunk, and a WRONG anchor only
        // costs a fallback to the sequential/pre-oracle path.
        //
        // WHY DIRECTION "just walk the whole arena unconditionally" WAS NOT
        // TAKEN. The old loop also carried `&& cand_idx < candidates.len()`, so
        // its anchor coverage was workload-dependent. Dropping that condition
        // was the obvious fix -- but measurement (2026-07-25, bintrees-18)
        // showed the condition never binds on the workload that matters: the
        // 126 conservative candidates spanned the full 2 GiB, the walk already
        // reached `used`, and `sweep-walk` (the sequential tail) was already
        // 0 ms. Dropping the condition would have bought exactly nothing there
        // while leaving the 262 ms in place. Sourcing anchors from the
        // allocator delivers unconditional full-arena coverage anyway -- the
        // anchor list no longer has any dependence on the candidate set at all
        // -- so that direction is subsumed, not skipped.
        let mut grid_anchors: Vec<usize> = Vec::with_capacity(exact_skips.len() + 64);
        if used_bytes > 0 {
            grid_anchors.push(0);
            grid_anchors.extend(young_from.take_alloc_anchors());
            grid_anchors.extend(
                exact_skips
                    .iter()
                    .filter_map(|&(off, sz)| off.checked_add(sz)),
            );
            grid_anchors.retain(|&o| o < used_bytes);
            grid_anchors.sort_unstable();
            grid_anchors.dedup();
            // Drop anything that lands INSIDE a free/TLAB-skip block. Source
            // (3) can mint one directly: two adjacent uncoalesced free blocks
            // put A's end exactly at B's start, and a walk that starts there
            // resyncs to B's end -- which may be past its own upper bound, so
            // the chunk (and with it the whole parallel sweep attempt) would
            // abort. Correct either way, but pointlessly slow. Offset 0 is
            // exempt: it is the walk's start, not a split point, and every
            // walker already skip-resyncs from it (unchanged from the
            // perf/parallel-young-gc anchor list this replaces).
            {
                let mut blocks = exact_skips.iter().peekable();
                grid_anchors.retain(|&o| {
                    while blocks.peek().is_some_and(|&&(off, sz)| off + sz <= o) {
                        blocks.next();
                    }
                    o == 0 || !blocks.peek().is_some_and(|&&(off, _)| off <= o)
                });
            }
            grid_anchors.push(used_bytes);
        } else {
            young_from.clear_alloc_anchors();
        }

        // Sweep chunk boundaries: subsample the grid at `sweep_anchor_stride()`
        // so the chunk count stays in the "far more chunks than workers, but
        // not one per TLAB" band the sweep was tuned for. `used` is always the
        // terminal entry so the sequential tail below has nothing left to do.
        let anchor_stride = sweep_anchor_stride();
        let mut sweep_anchors: Vec<usize> = Vec::new();
        let mut next_anchor = 0usize;
        for &a in &grid_anchors {
            if a >= next_anchor {
                sweep_anchors.push(a);
                next_anchor = a - a % anchor_stride + anchor_stride;
            }
        }
        if used_bytes > 0 && sweep_anchors.last() != Some(&used_bytes) {
            sweep_anchors.push(used_bytes);
        }

        // ----- Exact-base oracle for CONSERVATIVE candidates -----------------
        //
        // Only anchor intervals that actually CONTAIN a candidate are walked;
        // an interval with no candidate in it can produce no range, so walking
        // it was always pure loss. With allocator anchors at TLAB granularity
        // (256 KiB - 1 MiB) that turns "chase every object up to the last
        // candidate" into "chase one TLAB per candidate cluster".
        let mut young_object_ranges: Vec<(usize, usize)> = Vec::new();
        // Absolute [start, end) spans whose object grid this collection PROVED
        // (a chain that landed exactly on the interval's upper anchor). This
        // replaces the single `oracle_trusted_abs` frontier and preserves its
        // 2026-07-18 semantics exactly: inside a proved span, a candidate with
        // no covering range is free/gap space and is dropped; anywhere else the
        // candidate falls back to direct validation of its own address (the
        // pre-oracle behaviour) rather than being silently dropped, which is
        // what caused the trigger-ON bt18 676xxxxx under-count family.
        // Over-retention-safe, never a header write through an unproved base.
        let mut verified_spans: Vec<(usize, usize)> = Vec::new();
        let mut ci = 0usize;
        while ci < conservative_candidates.len() {
            let cand_off = conservative_candidates[ci] - from_base;
            // `grid_anchors[0] == 0` and the list ends at `used`, and
            // `in_young` already bounded every candidate below `from_end`, so
            // the containing interval always exists.
            let ai = grid_anchors.partition_point(|&a| a <= cand_off) - 1;
            let lo = grid_anchors[ai];
            let hi = grid_anchors[ai + 1];

            let ranges_before = young_object_ranges.len();
            let mut exact_cursor = lo;
            let fi = exact_skips.partition_point(|&(off, sz)| off + sz <= lo);
            let mut exact_free_iter = exact_skips[fi..].iter().peekable();
            let mut cand_idx = ci;
            // gen r5w1/young5: the grid rules are `probe_grid_at`'s, with this
            // walk's historical zero-run behaviour (`Parse`) and its anomaly
            // policy (warn and discard the interval) kept.
            let exact_rules = GridRules {
                base: from_base,
                used: used_bytes,
                zero_run_hi: used_bytes,
                side_sorted: &[],
                zero_runs: ZeroRunRule::Parse,
                bad_gap_filler_is_anomaly: true,
                phantom: false,
            };
            let mut no_phantom_probe = 0usize;
            while exact_cursor < hi {
                if skip_free_blocks(&mut exact_cursor, &mut exact_free_iter).0 {
                    continue;
                }
                if exact_cursor >= hi {
                    break;
                }
                let ptr = (from_base + exact_cursor) as *mut u8;
                let next_free = exact_free_iter.peek().map(|&&(off, _)| off);
                // Free/TLAB ranges were skipped; this cursor is on an
                // allocator-written object boundary inside `used`.
                let total = match probe_grid_at(
                    exact_cursor,
                    next_free,
                    &exact_rules,
                    &mut no_phantom_probe,
                ) {
                    // A GAP-filler sentinel (Bug-D, 2026-06-12): its layout
                    // overlays a raw gap length at offset 4, not real header
                    // fields.
                    GridProbe::Gap { len } => {
                        exact_cursor += len;
                        continue;
                    }
                    GridProbe::Object { size } => size,
                    GridProbe::BadGapFiller => {
                        tracing::warn!(
                            exact_cursor,
                            "GC: exact young-object walk found an implausible GAP-filler sentinel"
                        );
                        break;
                    }
                    GridProbe::CrossesFreeBlock { free_off: off, .. } => {
                        tracing::warn!(
                            exact_cursor,
                            off,
                            "GC: exact young-object walk crossed a free/TLAB range"
                        );
                        break;
                    }
                    // `Parse` yields no zero-run verdicts and `phantom` is
                    // off, so this arm is the implausible extent.
                    GridProbe::BadSize { .. }
                    | GridProbe::EmptyRun { .. }
                    | GridProbe::ZeroSpan { .. }
                    | GridProbe::Phantom { .. } => {
                        tracing::warn!(
                            exact_cursor,
                            used = used_bytes,
                            "GC: exact young-object walk stopped at an implausible extent"
                        );
                        break;
                    }
                };
                // Record this object's range only if it covers a conservative
                // candidate; a candidate that fell into free/gap space below it
                // resolves to "not an object" in the oracle — identical to the
                // full-walk behavior, where a non-covered address failed the
                // range probe and was dropped.
                let start_addr = ptr as usize;
                let end_addr = start_addr + total;
                while cand_idx < conservative_candidates.len()
                    && conservative_candidates[cand_idx] < start_addr
                {
                    cand_idx += 1;
                }
                if cand_idx < conservative_candidates.len()
                    && conservative_candidates[cand_idx] < end_addr
                {
                    young_object_ranges.push((start_addr, end_addr));
                    while cand_idx < conservative_candidates.len()
                        && conservative_candidates[cand_idx] < end_addr
                    {
                        cand_idx += 1;
                    }
                }
                exact_cursor += total;
            }
            if exact_cursor == hi {
                // Chain landed exactly on the next anchor: `lo` really was an
                // object base and every range collected in between is exact.
                // Merge with the previous span when they abut so the lookup
                // list stays a handful of entries.
                let (a, b) = (from_base + lo, from_base + hi);
                match verified_spans.last_mut() {
                    Some(last) if last.1 == a => last.1 = b,
                    _ => verified_spans.push((a, b)),
                }
            } else {
                // Unproved interval: discard its ranges. Its candidates now
                // take the direct-validation fallback in `mark_young`.
                young_object_ranges.truncate(ranges_before);
            }
            // Every candidate below `hi` has been resolved one way or the other.
            while ci < conservative_candidates.len() && conservative_candidates[ci] - from_base < hi
            {
                ci += 1;
            }
        }
        if phase_diag {
            let walked: usize = verified_spans.iter().map(|&(a, b)| b - a).sum();
            eprintln!(
                "[gcphase] oracle: cands={} ranges={} grid-anchors={} sweep-anchors={} \
                 walked={walked} used={used_bytes} verified-spans={}",
                conservative_candidates.len(),
                young_object_ranges.len(),
                grid_anchors.len(),
                sweep_anchors.len(),
                verified_spans.len(),
            );
        }
        // CRATONVM_DBG_SWEEP_LIVENESS: snapshot the oracle's products so the
        // root-hit classifier below can say what the mark phase saw. Cheap and
        // flag-gated; avoids borrowing them out from under `mark_young`.
        let (liveness_ranges, liveness_verified) = if gc_flags().dbg_sweep_liveness {
            (young_object_ranges.clone(), verified_spans.clone())
        } else {
            (Vec::new(), Vec::new())
        };

        report_phase("mark-oracle-walk");

        // ----- Mark phase -------------------------------------------------
        //
        // BFS over young-gen objects. The worklist holds young object
        // pointers that have been marked but not yet scanned. Marking an
        // old-gen object is unnecessary for a young collection, but we
        // still traverse *through* an old-gen object if a dirty card says
        // it may reference young gen (handled below via the dirty-card
        // seed). Marking uses `GC_FLAG_MARKED` in the object header.
        let mut worklist: Vec<usize> = Vec::new();

        // xt-hardening (2026-07-03): per-cycle SIDE mark set for candidates
        // that must never be header-written. The mark write
        // (`gc_flags |= GC_FLAG_MARKED` at candidate+21) through a
        // conservative FALSE POSITIVE is itself a heap corruptor: a candidate
        // at (live_object_start - 8) — trivially plausible whenever the
        // preceding 8 bytes are zero and the victim's identity hash is
        // un-minted — lands the write at victim+13, flipping bit 9 of the
        // victim's `array_length` to EXACTLY 512 (the observed
        // "kind=Object but array_length=512" corrupt-header face, whose
        // volume exploded when the xt cross-thread takeover started feeding
        // whole frozen-peer register files and stack bands into the root
        // set). Real objects always have a non-zero first header word
        // (class_id|kind|element_type); the only legal zero-word0 shape is a
        // fresh zero-hash `ClassId(0)` container, which the side set handles
        // correctly (pinned + traced, never written). Zero-word0 candidates
        // are the overwhelming false-positive volume — routing them here
        // removes the writer from the entire zeroed-span/misalign family.
        //
        // perf/parallel-young-gc (2026-07-25): the side channel is now a
        // lock-free BITMAP over from-space (one bit per 8 bytes) instead of an
        // `FxHashSet<usize>`. Same semantics (a marked address is retained and
        // never header-written), three wins: the claim is a single atomic
        // `fetch_or`, so several marker threads can share it; `side_sorted`
        // falls out of a bitmap scan instead of an O(n log n) sort of millions
        // of addresses; and the per-object hash+probe disappears.
        let side_bits = cratonvm_gc_core::young_mark::YoungMarkBits::new(from_base, from_end - from_base);
        // Everything the precise marker needs that is constant for the whole
        // collection. The pin registries are snapshotted HERE, once, rather
        // than locked per marked object: the mutators are stopped, so a
        // snapshot is exactly equivalent, and a per-object `Mutex` would
        // serialise the parallel drain below.
        let mark_ctx = YoungMarkCtx {
            from_base,
            from_end,
            // HIB-CV-24: a live object keeps its class's defining ClassLoader
            // alive (instance->loader). No-op for the common
            // no-custom-loader case.
            loader_pin_on: cratonvm_types::loader_pin::loader_pinning_enabled(),
            // gen r4/mark: FxHashSet, see `YoungMarkCtx::overlay_owners`.
            overlay_owners: cratonvm_gc_core::external_roots::external_owner_addrs()
                .map(|owners| owners.into_iter().collect()),
            metadata_pins: cratonvm_types::metadata_pin::snapshot(),
            // Snapshotted once per collection; see `YoungMarkCtx::mark_why`.
            mark_why: cratonvm_gc_core::heap::young_mark_watch(),
            // gen r4/mark: per-collection snapshots replacing per-object
            // registry lookups -- see the two fields' docs.
            // gc-common w36-d: per class id, the row inside this from-space
            // when VMs share the id (the only loaders this marker marks).
            loader_pins: if cratonvm_types::loader_pin::loader_pinning_enabled() {
                cratonvm_types::loader_pin::snapshot_where(|a| a >= from_base && a < from_end)
            } else {
                None
            },
            mirror_pins: cratonvm_types::mirror_pin::snapshot(),
        };
        // ----- Oracle resolution accounting (H2-CID0, 2026-08-01) -----------
        //
        // `mark_young` resolves a CONSERVATIVE candidate to the BASE of the
        // object containing it, because the sweep matches side marks against
        // object STARTS: a side mark on an interior address retains nothing.
        // When the anchor oracle cannot resolve a candidate (its interval's
        // header chain failed to land on the upper anchor) the code below falls
        // back to side-marking the RAW candidate address — which is correct
        // only if the candidate happens to BE a base. For an interior
        // candidate the containing object is left unmarked and the sweep
        // reclaims it **while a conservative root still points into it**,
        // silently inverting the collector's documented "a conservative false
        // positive can only over-retain, never free a live object" invariant.
        //
        // Count the three outcomes, and remember every fallback address so the
        // late-resolution pass after the mark closure can finish the job
        // against the arena's own object grid (the same grid the sweep walks,
        // and therefore the only authority on what is about to be freed).
        let oracle_resolved = std::cell::Cell::new(0usize);
        let oracle_dropped = std::cell::Cell::new(0usize);
        let oracle_unresolved: std::cell::RefCell<Vec<usize>> = std::cell::RefCell::new(Vec::new());
        // H2-CID0-BLOCKED attribution: the ADDRESSES the verified-span branch
        // concludes are gap space, so the root-in-dead-span invariant at the
        // bottom of this function can say whether a root it caught was dropped
        // there, left unresolved, or resolved and then lost somewhere else.
        // Bounded so a pathological cycle cannot grow it without limit.
        let oracle_dropped_addrs: std::cell::RefCell<Vec<usize>> =
            std::cell::RefCell::new(Vec::new());
        // mark_if_young: mark a candidate young pointer and enqueue it.
        // SAFETY contract: `ptr` is only dereferenced after `in_young`
        // confirms it lands inside the live from-space region.
        //
        // Returns `true` exactly when this call NEWLY claimed a young object —
        // i.e. nothing had marked it yet. Every seeding caller ignores the
        // value; the finalizer-resurrection phase below is the one consumer,
        // because "was this candidate already marked by the root closure?" is
        // precisely the question that separates a still-reachable finalizable
        // object from a dead one (see that phase for why it must run last).
        // r23 w4 review (R-L4z item 2): the from-space's lazy-dirty mirror, for
        // the fallback arm below (a candidate outside every proved span is
        // validated by its header bytes alone, and a dead payload a pinned
        // in-place cycle left unzeroed can decode as a plausible header).
        let sweep_lazy_mirror = young_from.lazy_dirty_mirror();
        let mark_young = |ptr: *mut u8,
                          worklist: &mut Vec<usize>,
                          bits: &cratonvm_gc_core::young_mark::YoungMarkBits|
         -> bool {
            let addr = ptr as usize;
            // The conservative arm: a root-vector entry or a finalizable
            // address. Reported BEFORE base resolution, because "a root
            // pointed INTO this object" and "a root pointed AT it" are
            // different answers and only the raw address distinguishes them.
            if mark_ctx.mark_why != 0 && addr == mark_ctx.mark_why {
                report_young_mark_why(addr, "conservative-root");
            }
            if !in_young(addr) {
                return false;
            }
            let insertion = young_object_ranges.partition_point(|(start, _)| *start <= addr);
            let covering = insertion
                .checked_sub(1)
                .and_then(|idx| young_object_ranges.get(idx))
                .filter(|&&(_, end)| addr < end);
            let (addr, ptr) = match covering {
                Some(&(base, _end)) => {
                    oracle_resolved.set(oracle_resolved.get() + 1);
                    (base, base as *mut u8)
                }
                // Inside a PROVED span the walk was verified end to end: an
                // uncovered candidate is free/gap space — not an object.
                None if verified_spans
                    .partition_point(|&(s, _)| s <= addr)
                    .checked_sub(1)
                    .is_some_and(|i| addr < verified_spans[i].1) =>
                {
                    oracle_dropped.set(oracle_dropped.get() + 1);
                    {
                        let mut d = oracle_dropped_addrs.borrow_mut();
                        if d.len() < 8192 {
                            d.push(addr);
                        }
                    }
                    return false;
                }
                // Outside every proved span (no anchor interval held this
                // candidate, or that interval's chain failed to land) — fall
                // back to validating the candidate address directly, the
                // pre-oracle behaviour. See the `verified_spans` note above.
                //
                // H2-CID0: record it. If this address is NOT an object base the
                // mark below retains nothing, so the late-resolution pass after
                // the closure re-resolves it against the arena object grid.
                None => {
                    // r23 w4 review (R-L4z item 2): inside a lazy-dirty span
                    // there is no object (the mirror never covers one).
                    if sweep_lazy_mirror.as_deref().is_some_and(|m| m.contains(addr)) {
                        oracle_dropped.set(oracle_dropped.get() + 1);
                        return false;
                    }
                    oracle_unresolved.borrow_mut().push(addr);
                    (addr, ptr)
                }
            };
            // SAFETY: `in_young` confirmed `addr` is an 8-byte-aligned
            // address inside the live from-space region, so reading an
            // ObjectHeader there is valid.
            let header = unsafe { &mut *(ptr as *mut ObjectHeader) };
            // Reject implausible headers — a conservative root may point
            // at a non-object word. `is_object_address`-style sanity.
            // Multi-array reloc fix (2026-05-22): `num_slots` mirrors
            // `array_length` for arrays, so a legitimate 256 MB int[] has
            // num_slots = 2^26 > 1<<24 and would be skipped (then swept as
            // garbage, despite being a live root). Gate num_slots on
            // non-arrays; bound array_length at the JVM ceiling.
            let kind_byte = ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed));
            let is_array = header.kind() == ObjectKind::Array;
            if kind_byte > 1
                || (!is_array && header.num_slots() > (1 << 24))
                || (is_array && header.array_length() > i32::MAX as u32)
            {
                // DBG (CRATONVM_DBG_SWEEP_CENSUS): a candidate whose header
                // is IMPLAUSIBLE gets dropped from marking entirely — if it
                // was actually a live object with a scarred header, the
                // sweep will reclaim it. Surface the drop (bounded).
                if gc_flags().dbg_sweep_census {
                    let n = SWEEP_BAD_EXTENT_HITS.load(Ordering::Relaxed);
                    if n < 12 {
                        eprintln!(
                                "[mark-reject] candidate {ptr:p} kind=0x{kind_byte:02x} ns={} alen={} cid={:#x} — dropped from marking (shape)",
                                header.num_slots(), header.array_length(), header.class_id.as_u32(),
                            );
                    }
                }
                return false;
            }
            // DoHead comb-7 fix (2026-07-03): also validate the object's
            // EXTENT. The field bounds above still admit a corrupt header
            // claiming millions of elements (array_length is only capped at
            // i32::MAX), and the BFS scan (`for_each_ref_slot`) iterates
            // that count with the header as its ONLY bound — a claimed
            // extent past from-space ran the scan off the mapped arena
            // (observed: main-vm SIGSEGV at the region boundary, corrupt
            // header claiming array_length=7,775,429 — the JIT inline-alloc
            // kind/array_length fault family). A real object's extent always
            // fits inside the arena it was allocated from, so an
            // out-of-extent header is definitionally corrupt: never mark or
            // scan it (its "referents" would be garbage reads anyway).
            let total = gen_object_total_size(header);
            if total < HEADER_SIZE || addr + total > from_end {
                let n = SWEEP_BAD_EXTENT_HITS.fetch_add(1, Ordering::Relaxed);
                if emit_conservative_candidate_diagnostic(n, cratonvm_gc_core::a2dbg::enabled()) {
                    // Attribution diagnostic: dump the words around the
                    // rejected "header" so the upstream corruptor face is
                    // identifiable (stale packed-pointer reuse shows heap
                    // pointers; a clobbered real header shows a torn mix).
                    let lo = addr.saturating_sub(32).max(from_base);
                    let mut hex = String::new();
                    let mut w = lo;
                    while w + 8 <= (addr + 48).min(from_end) {
                        // SAFETY: `[from_base, from_end)` is mapped arena
                        // memory and `w` is 8-aligned within it.
                        let v = unsafe { *((w & !7) as *const u64) };
                        hex.push_str(&format!("{:#x}:{:016x} ", w & !7, v));
                        w += 8;
                    }
                    tracing::warn!(
                        "mark_young: ignoring conservative candidate at {:#x} with implausible extent \
                         {} (kind={}, array_len={}, num_slots={}); safe reject, \
                         not marked/scanned; context {}",
                        addr,
                        total,
                        kind_byte,
                        header.array_length(),
                        header.num_slots(),
                        hex,
                    );
                    // A2 forensic probe (CRATONVM_DBG_A2): correlate this
                    // rejected address against the allocation breadcrumb
                    // ring — was this slot EVER header-written by an
                    // allocator (interpreter TLAB / gen_heap / JIT
                    // inline-alloc / TLAB tail-filler), and with what
                    // real size/class? Distinguishes "never allocated
                    // here" (stale conservative root over reused/free
                    // memory) from "allocated then clobbered" (a real
                    // header-write race) from "allocated exactly this
                    // shape, walker/mark logic disagrees" (a formula bug).
                    match cratonvm_gc_core::a2dbg::lookup_at(addr) {
                            Some(r) => tracing::warn!(
                                "  [A2] BREADCRUMB exact-addr alloc: class_id={} kind={} et={} alen={} ns={} REAL_size={} seq={}",
                                r.class_id, r.kind, r.element_type, r.array_length, r.num_slots, r.size, r.seq,
                            ),
                            None => match cratonvm_gc_core::a2dbg::lookup_covering(addr) {
                                Some(r) => tracing::warn!(
                                    "  [A2] BREADCRUMB covered by alloc start={:#x} class_id={} kind={} et={} alen={} ns={} REAL_size={} seq={} (mid-object offset={})",
                                    r.addr, r.class_id, r.kind, r.element_type, r.array_length, r.num_slots, r.size, r.seq, addr - r.addr,
                                ),
                                None => tracing::warn!(
                                    "  [A2] BREADCRUMB — NO allocation record covers {:#x} (never header-written here, or freed+reused past the ring)",
                                    addr,
                                ),
                            },
                        }
                }
                return false;
            }
            // Family-A fix (2026-07-03): EVERY conservative root candidate
            // takes the SIDE path now — alive and traced, but the header is
            // NEVER written through. Previously only a zero-first-word
            // candidate was side-marked; any OTHER candidate that merely
            // passed the kind/num_slots/extent plausibility checks above
            // (bit-plausible but not necessarily the true start of a live
            // object) fell through to `header.gc_flags |= GC_FLAG_MARKED`
            // below, an unconditional write through the candidate pointer.
            //
            // The checks above bound `class_id`/`kind`/`num_slots`/extent to
            // "looks like it could be a real header" — they do NOT prove the
            // candidate is the actual start address a real allocator wrote a
            // header at. A conservative root scan (register/stack scan, or
            // — at much higher volume — the cross-thread `xt_root_scan`
            // OS-suspend takeover, which floods this seed with thousands of
            // raw register/stack words from EVERY other live thread) can
            // easily produce an address that is NOT a real object start but
            // decodes as one anyway: e.g. the 16-byte-aligned `Value` cells
            // of a live `Object[]` all carry `VTAG_OBJECT = 4` as their
            // discriminant word, so landing on ANY element's disc word reads
            // `class_id=4, kind=Object` — and 16 bytes later, the NEXT
            // element's disc word reads as a matching, equally-plausible
            // `num_slots=4`. Writing `GC_FLAG_MARKED` (0x02) through such a
            // false-positive candidate lands the byte write inside a
            // genuinely live neighboring object's real header — bit 9 of
            // `array_length` (byte offset 13, 8 bytes past `gc_flags` at
            // offset 21 minus the header's own +8 alignment window) is
            // exactly the observed "kind=Object but array_length=512/513"
            // corruption face this whole family is named for; other offsets
            // hit `num_slots`, `gc_age`, or the low byte of `forwarding_ptr`
            // depending on the candidate's exact false-positive offset.
            //
            // `side_marks` was already proven safe and sufficient for the
            // zero-word0 case (over-retention only, per the comment above);
            // extending it to every candidate closes the entire
            // write-through class at the cost of pure over-retention (a
            // false-positive candidate keeps its neighbor pinned instead of
            // corrupting it — the collector's own documented safety
            // invariant: "a conservative false-positive root only
            // over-retains, it can never cause a live object to be freed OR
            // its non-pointer data to be corrupted").
            // SAFETY: `addr` is 8-aligned inside mapped from-space.
            //
            // Merge note (2026-07-04): dev independently landed a NARROWER
            // mitigation for this same non-zero-word0 hazard
            // (`header_reserved_fields_plausible` — reject a candidate
            // whose always-zero padding/reserved bytes or undefined
            // gc_flags bits are set, ~1/2^29 false-negative rate on
            // garbage) and still header-wrote through anything that
            // passed it. That check is real and kept (used elsewhere by
            // dev's other hardening below), but it does NOT catch this
            // fix's target case: a genuine live `Object[]` element cell,
            // whose bytes are NOT garbage — `_padding`/`_gc_reserved`
            // read 0 legitimately (they alias the high bytes of an
            // adjacent element's pointer payload, which is frequently
            // 0 on a 48-bit address space) and `gc_flags` reads 0 too.
            // Such a candidate sails through
            // `header_reserved_fields_plausible` and still gets
            // header-written. Unconditional side-marking (this fix)
            // has no such gap: every candidate is treated as
            // never-write-through, full stop.
            if bits.try_mark(addr) {
                worklist.push(addr);
                return true;
            }
            false
        };

        // Precise-edge marker (perf/halfgap-20260717): for values read out of
        // actual reference slots — BFS `for_each_ref_slot` referents,
        // dirty-card slot reads, overlay/loader/mirror side channels. These
        // are object BASES by construction (a store wrote a real reference
        // there), so the conservative interior-pointer oracle above is a
        // semantic no-op for them and its per-edge range probe was pure
        // overhead. Keeps the same header-plausibility rejection and the
        // same never-write-through side-mark channel as `mark_young`; the
        // only difference is skipping base resolution. This also restores
        // the pre-oracle robustness property that a truncated oracle walk
        // (corrupt header mid-arena) cannot silently unroot every precise
        // edge above the truncation point.
        // (What used to be the `mark_young_precise` closure is now the free
        // function `mark_edge_precise` at the bottom of this module. A closure
        // capturing `&mut` marker state cannot be shared by the parallel mark
        // workers; a free function taking `&YoungMarkCtx` + `&YoungMarkBits`
        // can. The body is unchanged: same header-plausibility rejection, same
        // extent check, same never-write-through side-mark channel, and it
        // still skips the conservative interior-pointer oracle because a value
        // read out of a real reference slot is an object BASE by
        // construction.)

        // Seed: precise + conservative roots gathered by the caller.
        for root in roots.iter() {
            mark_young(root.as_ptr(), &mut worklist, &side_bits);
        }
        // gen r5w2/roots6: plus the dirty-card scan's walk-gap words, through
        // the same interior-aware conservative marker (see the scan above).
        for &w in &card_gap_roots.young_words {
            mark_young(w as *mut u8, &mut worklist, &side_bits);
        }

        // NOTE: `finalizer_addrs` is deliberately NOT seeded here. Finalizable
        // objects are seeded by the resurrection phase AFTER the root closure
        // is complete, because seeding them alongside the real roots makes the
        // two indistinguishable in the mark set — and "reachable from a real
        // root" vs "present in the candidate list" is exactly the question
        // that decides whether `finalize()` may run. See that phase below.
        report_phase("mark-root-seed");

        // DIAG/EXPERIMENT (`CRATONVM_GC_FULL_RSET_SCAN=1`, which
        // `CRATONVM_CARD_TABLE_ONLY=1` overrides back off — see
        // `full_old_rset_scan_enabled`): seed old→young edges
        // by walking EVERY old-gen object's reference slots instead of trusting
        // the dirty-card table. If a workload that corrupts under the card seed
        // runs clean under the full scan, an old→young edge is provably being
        // lost by the card path (barrier miss, drain loss, or scan consumption)
        // — the young-GC live-object-reclaim investigation's discriminator.
        //
        // This line used to name `CRATONVM_SWEEP_FULL_OLD_SCAN`, a spelling no
        // code in the tree has ever read: an operator following the comment set
        // it, got the default behaviour, and would have concluded the
        // discriminator had ruled the card path OUT.
        let full_old_scan = Self::full_old_rset_scan_enabled();
        if full_old_scan {
            for (optr, _sz) in old_gen.walk_objects() {
                // SAFETY: `walk_objects` yields valid live old-gen object starts.
                let oh = unsafe { &*(optr as *const ObjectHeader) };
                // SAFETY: `optr`/`oh` form a valid live object.
                unsafe {
                    for_each_ref_slot(optr, oh, |raw, _slot| {
                        mark_edge_precise(
                            raw as usize,
                            &mark_ctx,
                            &side_bits,
                            &mut worklist,
                            "full-old-scan",
                        );
                    });
                }
            }
        }

        // Seed: old→young references from dirty cards -- `extra_roots`, which
        // the `scan_dirty_cards` call at the top of this function filled (gen
        // r5w2/roots6 moved it there; see that call).
        // gc-genpause F2: same audit on the non-moving path. This sweep does
        // not relocate, so a missed edge here is a premature RECLAIM rather
        // than a dangling rewrite -- the same defect, one step later.
        if gc_flags().verify_rset {
            let _ =
                Self::verify_old_to_young_rset(&old_gen, &young_from, &extra_roots, "non-moving");
        }
        // `scan_dirty_cards` *consumes* the dirty cards (their bitmap bytes
        // are reset to clean; there is no tracking list since gcd d1/d). Since
        // this collection does not move young objects, every old→young
        // reference it found is still valid and the card covering it must
        // stay dirty for the NEXT collection. Re-dirty each old object's
        // card after we have read its slots below.
        let mut redirty_cards: Vec<usize> = Vec::with_capacity(extra_roots.len());
        // gen r5w2/roots6: a walk-gap word's card stays dirty too -- its young
        // target stays where it is (marked and pinned above), so the edge
        // survives this sweep exactly as a precise holder's does.
        redirty_cards.extend_from_slice(&card_gap_roots.card_addrs);
        // gen r5w6/old10 (cards3 item 3 row b, old9's cross-lane request):
        // an element-precise table re-dirties a reference-array hit's ELEMENT
        // card, as the moving path's `seed_dirty_card_roots` defers it. Before,
        // every hit came back as a HEADER mark, so the next pause scanned the
        // whole array (`scan_dirty_cards_inner` reads an array with a dirty
        // header card whole) instead of the one card the barrier had dirtied.
        // Same edge set either way: the element's card is dirty, and the scan's
        // window arm reads exactly the elements whose slot starts in it.
        let precise_array_cards = self.card_table.precise_ref_array_marks();
        // gch h1/yy (`CRATONVM_GEN_SWEEP_PARALLEL_CARD_SEED`, default ON since
        // gci i1/yg, `=0` kill): the
        // seed's per-pair work is a random read of the young referent's header
        // (`mark_edge_precise`), ~250k of them per cycle on
        // `GenR4W4EvacThroughputProbe`, all on this thread before the parallel
        // closure opens. With the switch the pairs are split into contiguous
        // slices seeded on the heap's pool, each into its own worklist and
        // card list; both are concatenated here in slice order, so
        // `redirty_cards` is the serial list exactly, and the worklist holds
        // the same claimed set (each object is pushed by the one claim that
        // won; which pair won, and so the order, may differ -- the drain's
        // result does not depend on it).
        let seed_threads = cratonvm_gc_core::young_mark::young_gc_threads(bytes_before);
        let par_card_seed = seed_threads > 1
            && extra_roots.len() >= PAR_CARD_SEED_MIN_PAIRS;
        if par_card_seed {
            let pool = self.evac_pool(seed_threads);
            let pairs: &[(ObjectRef, usize, usize)] = &extra_roots;
            let nslices = (seed_threads * 4).min(pairs.len());
            let per = pairs.len().div_ceil(nslices);
            let slices: Vec<std::sync::Mutex<(Vec<usize>, Vec<usize>)>> = (0..nslices)
                .map(|_| std::sync::Mutex::new((Vec::new(), Vec::new())))
                .collect();
            let next_slice = AtomicUsize::new(0);
            let seed_worker = || loop {
                let i = next_slice.fetch_add(1, Ordering::Relaxed);
                if i >= nslices {
                    return;
                }
                let lo = (i * per).min(pairs.len());
                let hi = ((i + 1) * per).min(pairs.len());
                let mut cards: Vec<usize> = Vec::with_capacity(hi - lo);
                let mut work: Vec<usize> = Vec::new();
                for &(old_obj, slot_idx, _) in &pairs[lo..hi] {
                    cards.push(seed_one_card_root(
                        old_obj,
                        slot_idx,
                        precise_array_cards,
                        &mark_ctx,
                        &side_bits,
                        &mut work,
                    ));
                }
                *slices[i].lock().unwrap_or_else(|e| e.into_inner()) = (cards, work);
            };
            run_chunk_workers(pool, seed_threads, &seed_worker);
            for slice in slices {
                let (cards, work) = slice.into_inner().unwrap_or_else(|e| e.into_inner());
                redirty_cards.extend_from_slice(&cards);
                worklist.extend_from_slice(&work);
            }
            self.young_premium_census[YP_PAR_CARD_SEEDS].fetch_add(1, Ordering::Relaxed);
        } else {
            for &(old_obj, slot_idx, _) in &extra_roots {
                redirty_cards.push(seed_one_card_root(
                    old_obj,
                    slot_idx,
                    precise_array_cards,
                    &mark_ctx,
                    &side_bits,
                    &mut worklist,
                ));
            }
        }

        // Restore the dirty cards consumed by `scan_dirty_cards` so the
        // next collection still sees these old→young references.
        self.card_table.mark_dirty_bulk(&redirty_cards);

        // Overlay-backed collections have old→young edges that do not occupy
        // a Java heap slot, so no card can describe them. A minor collection
        // leaves old gen intact and cannot decide which old owners will later
        // be dead; retain edges of every current old owner here just as the
        // card scan does. The major marker below applies the precise
        // owner-reachable rule before it compacts old space.
        for overlay_ref in
            cratonvm_gc_core::external_roots::external_roots_for_matching_owners(&|owner_addr| {
                old_gen.contains(owner_addr as *mut u8)
            })
        {
            mark_edge_precise(
                overlay_ref.as_ptr() as usize,
                &mark_ctx,
                &side_bits,
                &mut worklist,
                "overlay-old-owner",
            );
        }
        // gen r4w2/youngmark (2026-09-23): the same rule for `mirror_pin`, the
        // other side-table edge no card can describe. `VmHeap::mirror_pin_deferrable`
        // leaves a YOUNG mirror out of the root set on exactly the cycles that
        // take this marker, on the promise that it is reached from its loader —
        // and `scan_young_object` follows that edge only from a loader it SCANS,
        // i.e. a young one. A mirror whose defining loader is OLD was reached by
        // nothing and freed while live. See `seed_mirror_pins_of_old_owners`.
        let mirror_old_owner_edges = seed_mirror_pins_of_old_owners(
            &mark_ctx,
            &|owner: usize| old_gen.contains(owner as *const u8),
            &side_bits,
            &mut worklist,
        );
        if phase_diag && mirror_old_owner_edges > 0 {
            eprintln!("[gcphase] mirror-old-owner: edges={mirror_old_owner_edges}");
        }
        // gc-common w36-d: and for `metadata_pin`, the loader-conditional edge
        // `VmHeap::metadata_pin_deferrable` now defers YOUNG values to on the
        // cycles that take this marker (`ClassValue` / `ClassLoaderValue`
        // results, statics, class locks, condy values...). A young value whose
        // loader is OLD is reached by nothing else. See
        // `seed_metadata_pins_of_old_owners`.
        let metadata_old_owner_edges = seed_metadata_pins_of_old_owners(
            &mark_ctx,
            &|owner: usize| old_gen.contains(owner as *const u8),
            &side_bits,
            &mut worklist,
        );
        if phase_diag && metadata_old_owner_edges > 0 {
            eprintln!("[gcphase] metadata-old-owner: edges={metadata_old_owner_edges}");
        }

        // DBG (CRATONVM_DBG_SEED_ALL_OLD): decisive test for the sweep-edges
        // verdict that bt18's leak is an old→young CLEAN-CARD miss. Seed the mark
        // from EVERY old-gen object's young references (not just dirty cards). If
        // this corrects the checksum, the bug is a card/barrier gap (a promotion
        // that failed to dirty the promoted object's card); if it does NOT, the
        // missed reference is not a clean-card old→young edge (older verdict).
        if gc_flags().dbg_seed_all_old {
            for (op, _sz) in old_gen.walk_objects() {
                // SAFETY: `op` is a live old-gen object header from walk_objects.
                let oh = unsafe { &*(op as *const ObjectHeader) };
                // SAFETY: `op`/`oh` are a valid live old-gen object.
                unsafe {
                    for_each_ref_slot(op, oh, |r, _| {
                        mark_edge_precise(
                            r as usize,
                            &mark_ctx,
                            &side_bits,
                            &mut worklist,
                            "full-old-scan",
                        )
                    });
                }
            }
        }

        report_phase("mark-card-seed");

        // ----- Transitive closure ------------------------------------------
        //
        // perf/parallel-young-gc (2026-07-25): the mark closure is drained by
        // `young_gc_threads` workers. This is sound because the phase is a
        // PURE, READ-ONLY transitive closure: every mutator is stopped, object
        // bodies are only read (`for_each_ref_slot`), the sole write is the
        // atomic bit claim in `YoungMarkBits::try_mark`, and an object is
        // enqueued only by the worker whose claim won -- so each object is
        // scanned exactly once and the final marked set is independent of
        // traversal order. The drain's barrier (`EvacPool::scope` on the heap's
        // pool since gce e1/y) joins every worker before `collect_marked` reads
        // the bitmap.
        //
        // `young_gc_threads` returns 1 for a small young gen (thread setup
        // would dominate) or when `CRATONVM_GC_PAR_THREADS` is 0/1, and
        // `drain_parallel` then runs a plain loop on this thread -- the
        // pre-parallel code path, unchanged.
        let mark_threads = cratonvm_gc_core::young_mark::young_gc_threads(bytes_before);
        // gce e1/y: every drain and the span zeroing below take their helpers
        // from the heap's persistent pool instead of spawning `mark_threads - 1`
        // fresh OS threads per call (up to five drains and one zeroing per
        // cycle; see `young_mark::run_phase_workers`). No pool is built for a
        // single-worker cycle.
        let mark_pool = (mark_threads > 1).then(|| self.evac_pool(mark_threads));
        cratonvm_gc_core::young_mark::drain_parallel_on(
            std::mem::take(&mut worklist),
            mark_threads,
            mark_pool,
            |addr, work| scan_young_object(addr, &mark_ctx, &side_bits, work, false),
        );
        report_phase("mark-closure");

        // ----- Late base resolution (H2-CID0, 2026-08-01) -------------------
        //
        // Every conservative candidate the anchor oracle could not resolve was
        // side-marked at its RAW address. That retains the object only if the
        // address happens to be a base — the sweep matches side marks against
        // object STARTS. Resolve the remainder against the arena's own object
        // grid (chained from offset 0, free-block aware: exactly the walk the
        // sweep performs, and thus the authority on what it will reclaim),
        // mark the covering bases, and drain the closure again so those
        // objects' own references are traced too. Retaining an object without
        // tracing it would only move the dangling reference one level down.
        //
        // Zero-cost on a healthy cycle: the walk runs only when the oracle
        // actually left something unresolved.
        //
        // Whatever the grid walk itself cannot reach (it desynced before
        // getting there) is handed to the sweep as `late_pins`: spans it must
        // not reclaim. That is strictly over-retention, the safe direction.
        let mut late_pins: Vec<usize> = Vec::new();
        // gen r4/sweep (2026-09-23): the object BASES the two late passes below
        // resolved from CONSERVATIVE candidates. Selective promotion must pin
        // them exactly as it pins the bases `young_object_ranges` resolves —
        // see the pin set there for why a base resolved here was promotable.
        let mut late_conservative_bases: Vec<usize> = Vec::new();
        let mut late_resolved_bases = 0usize;
        let mut late_interior_cleared = 0usize;
        // H2-CID0-BLOCKED attribution, read by the root-in-dead-span invariant.
        let mut unresolved_snapshot: Vec<usize> = Vec::new();
        let mut dropped_snapshot: Vec<usize> =
            std::mem::take(&mut *oracle_dropped_addrs.borrow_mut());
        dropped_snapshot.sort_unstable();
        dropped_snapshot.dedup();
        {
            let mut unresolved = std::mem::take(&mut *oracle_unresolved.borrow_mut());
            unresolved_snapshot = unresolved.clone();
            unresolved_snapshot.sort_unstable();
            unresolved_snapshot.dedup();
            if phase_diag {
                eprintln!(
                    "[gcphase] oracle-outcome: resolved={} dropped-as-gap={} unresolved={}",
                    oracle_resolved.get(),
                    oracle_dropped.get(),
                    unresolved.len(),
                );
            }
            if !unresolved.is_empty() {
                unresolved.sort_unstable();
                unresolved.dedup();
                let (bases, leftover, interior) =
                    resolve_candidate_bases(from_base, used_bytes, &exact_skips, &unresolved);
                late_resolved_bases = bases.len();
                late_conservative_bases.extend_from_slice(&bases);
                for base in bases {
                    mark_edge_precise(base, &mark_ctx, &side_bits, &mut worklist, "late-base");
                }
                // Retire the RAW marks this pass just superseded. Each address
                // in `interior` was proved to sit strictly inside an object by
                // the same linear chain the sweep walks, so it is provably not
                // an object start: the mark retained nothing (the sweep matches
                // marks against STARTS), and the covering base was marked
                // above, so the object is retained either way. What the mark
                // DID do was make `side_sorted` -- which the phantom-extent
                // guard and the parallel `sweep_chunk` both read as "live
                // BASES, and live objects never nest" -- carry an address
                // interior to a valid object. The guard then fires on that
                // object, unwinds every reclamation since the last anchor
                // (sequential walk) or abandons the whole parallel attempt
                // (`chunk_bail(5)`). Clearing them makes the guard's premise
                // TRUE by construction rather than by assumption, which is what
                // the 2026-08-13 `phantom_extents` finding turned on.
                late_interior_cleared = interior
                    .iter()
                    .filter(|&&addr| side_bits.unmark(addr))
                    .count();
                if late_interior_cleared > 0 {
                    LATE_RESOLVE_RAW_INTERIOR_CLEARED
                        .fetch_add(late_interior_cleared as u64, Ordering::Relaxed);
                }
                if !worklist.is_empty() {
                    cratonvm_gc_core::young_mark::drain_parallel_on(
                        std::mem::take(&mut worklist),
                        mark_threads,
                        mark_pool,
                        |addr, work| scan_young_object(addr, &mark_ctx, &side_bits, work, false),
                    );
                }
                late_pins = leftover;
                if late_resolved_bases > 0 || !late_pins.is_empty() {
                    LATE_RESOLVE_CYCLES.fetch_add(1, Ordering::Relaxed);
                    LATE_RESOLVE_BASES.fetch_add(late_resolved_bases as u64, Ordering::Relaxed);
                    LATE_RESOLVE_PINS.fetch_add(late_pins.len() as u64, Ordering::Relaxed);
                    let n = LATE_RESOLVE_REPORTS.fetch_add(1, Ordering::Relaxed);
                    if n < 8 {
                        tracing::warn!(
                            target: "cratonvm::gc::guard",
                            unresolved = unresolved.len(),
                            resolved_bases = late_resolved_bases,
                            pinned = late_pins.len(),
                            raw_interior_marks_cleared = late_interior_cleared,
                            "young non-moving sweep: the anchor oracle left conservative root \
                             candidate(s) unresolved; they were side-marked at their RAW \
                             address, which retains nothing when the address is object-INTERIOR. \
                             Re-resolved against the arena object grid. Before this pass the \
                             containing object(s) would have been reclaimed while a conservative \
                             root pointed into them.",
                        );
                    }
                }
            }
        }
        report_phase("mark-late-resolve");

        // EXPERIMENT (`CRATONVM_GC_LATE_RESOLVE_DROPPED=1`, default OFF): give
        // the DROPPED candidates the same late-resolution pass the UNRESOLVED
        // ones get.
        //
        // `mark_young` has two ways to decline a conservative candidate the
        // object grid does not cover, and only one of them has a second chance.
        // A candidate OUTSIDE every proved span is recorded as `unresolved` and
        // re-resolved above. A candidate INSIDE a proved span is `dropped`: the
        // anchor walk was verified end to end, so an address it does not cover
        // "is free/gap space -- not an object", and the closure returns having
        // marked nothing and recorded nothing for the pass above to look at.
        //
        // That reasoning is sound exactly as far as the proof is. This arm
        // exists to price the assumption rather than argue with it: run the
        // dropped addresses through the SAME grid resolver, and mark whatever
        // it says is a real object base. If every proof holds, the resolver
        // finds no base, nothing is marked, and the arm is free; if one does
        // not, the object is retained instead of freed. Over-retention only --
        // it can never free something the default arm keeps.
        //
        // Opened for `io.netty.util.internal.ObjectCleanerTest`, which reclaims
        // a live lambda in its FIRST young collection under both
        // `-XX:+UseGenerationalGC` and `-XX:+UseG1GC` (5/5 each) and never under
        // ZGC, with `[sweep-edges]` reporting `root=11 ... REACHABLE NODE WILL
        // BE SWEPT` in 10 of 10 failing runs and `root=0` in the one that
        // passed. Every other explanation has been measured and refused -- see
        // the known-issues page. This is the one path left by which an address
        // the sweep-edges scan still sees as a root becomes an object the sweep
        // frees.
        if gc_flags().late_resolve_dropped && !dropped_snapshot.is_empty() {
            let (bases, leftover, _interior) =
                resolve_candidate_bases(from_base, used_bytes, &exact_skips, &dropped_snapshot);
            let recovered = bases.len();
            late_conservative_bases.extend_from_slice(&bases);
            for base in bases {
                mark_edge_precise(base, &mark_ctx, &side_bits, &mut worklist, "late-dropped");
            }
            if !worklist.is_empty() {
                cratonvm_gc_core::young_mark::drain_parallel_on(
                    std::mem::take(&mut worklist),
                    mark_threads,
                    mark_pool,
                    |addr, work| scan_young_object(addr, &mark_ctx, &side_bits, work, false),
                );
            }
            if recovered > 0 || !leftover.is_empty() {
                LATE_RESOLVE_DROPPED_RECOVERED.fetch_add(recovered as u64, Ordering::Relaxed);
                let n = LATE_RESOLVE_DROPPED_REPORTS.fetch_add(1, Ordering::Relaxed);
                if n < 8 {
                    tracing::warn!(
                        target: "cratonvm::gc::guard",
                        dropped = dropped_snapshot.len(),
                        recovered_bases = recovered,
                        still_unresolvable = leftover.len(),
                        "young non-moving sweep: a candidate the anchor oracle dropped as \
                         free/gap space INSIDE a proved span resolved to a real object base \
                         after all. The proof was wrong, or the span was not the one the \
                         candidate is in. Marked rather than freed.",
                    );
                }
            }
        }
        report_phase("mark-late-resolve-dropped");

        // ----- loader_pin from OLD instances (gen r4w2/youngmark) ------------
        //
        // The instance -> defining-loader edge (`loader_pin`) is followed by
        // `scan_young_object` from every SCANNED instance, i.e. young ones. An
        // OLD instance is never scanned by this cycle, so a YOUNG loader whose
        // only live instances were tenured was reached by nothing: with loader
        // unloading on (the default) loaders are not rooted, and the sweep
        // freed a live `ClassLoader` together with every young mirror and
        // metadata root it pins. Minor-GC semantics treat the old generation
        // as live, so an old instance keeps its loader alive.
        //
        // Priced, not free: finding "does any old object have one of these
        // class ids" is an old-generation walk. It runs ONLY when a young
        // pinned loader is still unmarked at this point — every earlier mark
        // (roots, cards, closure, both late passes) is in — which is the
        // cycle in which such a loader is about to be swept, so a healthy
        // cycle pays one map scan over the (small) loader registry. It sits
        // before finalizer resurrection because this is root-closure
        // reachability, which that phase's "already marked?" test must see.
        let unreached_loaders = unreached_young_pinned_loaders(&mark_ctx, &side_bits);
        if !unreached_loaders.is_empty() {
            let candidates = unreached_loaders.len();
            let old_class_ids = old_gen.walk_objects().into_iter().map(|(optr, _)| {
                // SAFETY: `walk_objects` yields the start of each allocated
                // old-gen object, so `optr` is a readable `ObjectHeader`.
                unsafe { (*(optr as *const ObjectHeader)).class_id.as_u32() }
            });
            let rescued = seed_loaders_of_old_instances(
                &mark_ctx,
                unreached_loaders,
                old_class_ids,
                &side_bits,
                &mut worklist,
            );
            if !worklist.is_empty() {
                cratonvm_gc_core::young_mark::drain_parallel_on(
                    std::mem::take(&mut worklist),
                    mark_threads,
                    mark_pool,
                    |addr, work| scan_young_object(addr, &mark_ctx, &side_bits, work, false),
                );
            }
            if phase_diag {
                eprintln!(
                    "[gcphase] loader-old-instance: unreached-class-rows={candidates} \
                     loaders-rescued={rescued}"
                );
            }
        }
        report_phase("mark-loader-old-instance");

        // ----- Finalizer resurrection --------------------------------------
        //
        // Runs HERE — after the root closure AND both late-resolution passes —
        // because "unmarked" only means "unreachable" once every root-derived
        // mark is in.
        //
        // Until 2026-09-10 this function seeded `finalizer_addrs` alongside the
        // real roots and then returned `finalizer_addrs.to_vec()` — the entire
        // candidate list, unconditionally — as its dead set. Callers treat that
        // list as "these are dead, run `finalize()`" (`force_gc_from_native`
        // enqueues every entry on the finalizer thread), and `System.gc()`
        // routed Generational through this sweep via `explicit_full_gc` (it
        // still does by default; since gen r4w2/youngpolicy
        // `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG` may send it down the moving
        // path instead), so one `System.gc()` finalized every pending
        // finalizable object in the process whether or not anything still
        // referenced it. Holding an object
        // in a static field or a live local was enough:
        // `probes/ReachableFinalizeProbe.java` measured 11 of 11 strongly
        // reachable objects finalized under `-XX:+UseGenerationalGC` and 0 under
        // G1, ZGC and HotSpot.
        //
        // The other three implementations of this phase already ask the question
        // this one now asks, and skip a candidate the ordinary mark reached:
        // `pointer_map.contains_key` in the moving Cheney path's Phase 2.5 and
        // in `G1Collector::resurrect_dead_finalizers`, `mark_is_set` in ZGC's.
        //
        // `mark_young` reports whether it NEWLY claimed the object, which is
        // that same "was it already marked?" test expressed through the exact
        // base resolution and header screening every other mark goes through —
        // so a candidate is judged by the identical machinery that decides
        // retention, not by a second, divergent notion of liveness.
        //
        // The drain is deliberately AFTER the loop, not inside it: two
        // finalizable objects that reference only each other are both
        // unreachable and must both be enqueued (`reference.rs`'s
        // `mutually_reachable_finalizers_both_enqueued`). Draining inside the
        // loop would let the first one's closure mark the second and hide it.
        let mut dead_finalizers: Vec<usize> = Vec::new();
        for &addr in finalizer_addrs {
            if !in_young(addr) {
                // Old gen, or not a heap address: not this collection's to
                // judge, so it stays registered rather than being finalized —
                // exactly what the moving path's `if !young_from.contains(..)`
                // does with it.
                continue;
            }
            if mark_young(addr as *mut u8, &mut worklist, &side_bits) {
                // Nothing had marked it, so no root reaches it: it is dead. The
                // mark just made keeps it — and, once the drain below runs,
                // everything it references — alive for `finalize()` to use.
                dead_finalizers.push(addr);
            }
        }
        // gci i47w/ec47 (`CRATONVM_DBG_GEN_FIN_CHILD_CENSUS`, default OFF):
        // the non-moving door's `[gen-fin-child]` census, before the drain
        // below marks the dead finalizables' subgraphs, so `marked` is the
        // strong closure's answer alone. Read only on a cycle that claimed a
        // dead finalizable.
        if !dead_finalizers.is_empty() && fin_child_census_enabled() {
            let marked = |a: usize| side_bits.contains(a);
            let in_old = |a: usize| old_gen.contains(a as *const u8);
            for line in non_moving_fin_child_census_lines(
                &dead_finalizers,
                &in_young,
                &in_old,
                &marked,
                roots,
            ) {
                eprintln!("{line}");
            }
        }
        if !worklist.is_empty() {
            // gc-common w36-d — the depth-2 set (`take_resurrection_closure`).
            // The strong closure had drained this worklist empty, and every
            // push site claims before it pushes, so each object this drain
            // scans was marked by the resurrection round and NOT by the strong
            // closure: exactly the objects reachable only through a dead
            // finalizable. Recorded at their PRE-collection addresses (this
            // sweep marks in place; a later selective promotion does not
            // change the key). One uncontended push per object, and only on a
            // cycle that resurrected something.
            let closure = Mutex::new(Vec::new());
            cratonvm_gc_core::young_mark::drain_parallel_on(
                std::mem::take(&mut worklist),
                mark_threads,
                mark_pool,
                |addr, work| {
                    closure.lock().push(addr);
                    scan_young_object(addr, &mark_ctx, &side_bits, work, false)
                },
            );
            self.note_resurrection_closure(&closure.into_inner());
        }
        report_phase("finalizer-resurrect");

        // Sorted view of the side mark set for O(1)-amortized lockstep checks
        // in the linear walks below (same pattern as the free-block skip).
        // ABSOLUTE addresses.
        // Ascending by construction: iterating the bitmap replaces both the
        // hash-set iteration and its O(n log n) sort.
        let side_sorted: Vec<usize> = side_bits.collect_marked();
        report_phase("mark");

        // ----- Selective promotion -----------------------------------------
        //
        // ✅ CORRECT (Fix A, 2026-06-05). Enabled under `CRATONVM_SELECTIVE_PROMOTE`
        // OR `CRATONVM_SHADOW_STACK` (see `selective_on` below); opt out with
        // `CRATONVM_NO_SELECTIVE_PROMOTE`. It hits the HotSpot-verified checksums on
        // bintrees10/14/16/18 — including **bt18 = 68332206**, which is the TRUE
        // value (`java -cp bench BenchSuite bintrees18`). The old "golden 67674804"
        // was a CratonVM UNDER-COUNT bug, so the earlier "EXPERIMENTAL/KNOWN-BUGGY —
        // 68332206 is WRONG" verdict here was INVERTED: this path is the *correct*
        // one and the moving Cheney (67674804) is the buggy one. `SP_VERIFY` already
        // showed MISSED=0 + ALIASING=0 — the fixup was always correct; only the
        // reference value was wrong. It also eliminates young-gen exhaustion (the
        // non-moving sweep alone walls on bt18's long-lived set).
        //
        // The non-moving sweep keeps every survivor in young in place, so a
        // workload whose live young set approaches young capacity (bintrees18's
        // long-lived tree) cannot drain young — the sweep reclaims ~nothing and
        // the VM thrashes or exhausts young. The moving collector avoids this by
        // tenuring survivors to old gen, but it cannot run while conservative
        // JIT roots are live (it would relocate a JIT-held pointer it cannot
        // safely rewrite).
        //
        // Selective promotion threads the needle: evacuate to OLD GEN exactly
        // those marked survivors NOT pinned by any root value. PIN = every young
        // address that appears as a root or finalizer value — which INCLUDES
        // conservative JIT false-positives, because we pin by the raw slot
        // value. An evacuated object's address can therefore never equal any
        // conservative slot value, so neither this fixup nor the VM-level
        // pointer_map remap (which conservatively rescans JIT slots) can rewrite
        // a non-pointer slot. Objects reachable only via precise heap edges (the
        // tree's interior nodes) are movable and get tenured, draining young
        // while the few pinned objects stay put.
        // gcf f3/q (r15w5/gcpage patch 1): read once for every linear walk of
        // this sweep (walk (2), fix-up 3a, the parallel chunks, the
        // sequential walk); each walk owns its own memo.
        let size_memo_on = gen_walk_size_memo_enabled();
        // gcg g4/yg: the opt-in pre-reserve of this map (gcf f3/q GP-3,
        // `CRATONVM_GEN_SWEEP_EVAC_RESERVE`) is deleted -- triage tg1
        // measured no gain.
        let mut evac_map: cratonvm_types::PointerMap = cratonvm_types::PointerMap::default();
        // Fix A: selective promotion is the proven-correct drain for the
        // JIT-active non-moving sweep (bt18 = 68332206 = HotSpot), so it is now
        // DEFAULT-ON. Opt out with `CRATONVM_NO_SELECTIVE_PROMOTE` to get the pure
        // (walling, under-counting) non-moving sweep for debugging. The legacy
        // `CRATONVM_SELECTIVE_PROMOTE` gate is now redundant (always-on) but kept
        // accepted for compatibility. This path runs only here, in the JIT-active
        // non-moving sweep, so it never affects the no-JIT moving Cheney.
        // xt-hardening (2026-07-03): do NOT evacuate on cycles that scanned
        // UN-REWRITABLE PEER STATE — a forcibly OS-suspended in-JIT peer, or a
        // blocked peer's JIT helper window. Such a peer's registers can hold
        // ONLY a derived/interior pointer to an object whose base is reachable
        // via precise heap edges: the base is not pin-by-value protected
        // (interior addresses don't resolve to roots), gets evacuated, its young
        // source zeroed and re-served, and the resumed peer keeps loading /
        // storing through the stale derived pointer. Retention for one cycle is
        // always safe; the flag is per-cycle so ordinary (single-threaded /
        // cooperative) collections keep the bt18-critical drain.
        //
        // HIB-GCOVERHEAD-HALFFULL.1 (2026-07-31): this gate used to read
        // `moving_young_coverage_incomplete()`, which in July 2026 had exactly
        // one caller — the cross-thread takeover path above — and so meant the
        // same thing. It does not any more. arch-2026-07-26 reused that flag for
        // the entirely separate question "may the COPYING collector relocate
        // this cycle?", and once moving-young became the default it was set on
        // essentially every JIT-active collection (`UNPUBLISHED_FRAME_OOP`,
        // `MISSING_EXACT_RBP`, `unregistered-jit-frame-on-stack`, …). Selective
        // promotion — the non-moving sweep's ONLY young→old drain — therefore
        // switched itself off VM-wide, and since the non-moving sweep is also
        // the default collector under a live JIT frame, the young generation
        // could never drain at all. Young filled with live objects, every forced
        // GC freed a sliver, the GC-overhead streak latched, and the VM raised
        // `OutOfMemoryError` on a 49 %-full heap with 570 MB free.
        //
        // Those coverage reasons are NOT this hazard: they describe frames the
        // conservative scan still walks on this thread's stack, and selective
        // promotion is safe under exactly that regime because it pins by raw
        // slot VALUE (a conservative false positive pins a random object; it can
        // never mis-relocate one). Read the narrow predicate instead — see
        // `gc_quiescence::unrewritable_peer_state` for the full fault line.
        // No object can satisfy `gc_age + 1 >= PROMOTION_AGE` until it has
        // survived `PROMOTION_AGE - 1` prior minor collections, so PROMOTION is
        // off at heap startup. (gen r4w2/youngmark: the walk itself is not a
        // no-op there — it is what ages survivors — and still runs, age-only;
        // see `age_only` below.)
        //
        // gen r4w6/young6: the promotion age is the heap's effective one
        // (`PROMOTION_AGE` unless adaptive tenuring is engaged), read once for
        // this sweep. gen r4w6 (orchestrator): the walk below now also fills
        // the age table from the survivors it ages and recomputes the
        // threshold, as the moving cycle does, for the NEXT cycle.
        // gc wave 9 (lane gcC): on a cycle the Serial-eden aging epoch holds,
        // `promotion_age` is `NON_AGING_PROMOTION_AGE` (no age-based tenure)
        // and `age_step` 0 (no survivor ages); reachability, which gates the
        // walk and its forced tenures, still reads the real age.
        let promotion_age = self.cycle_promotion_age();
        // gci i1/yg (`CRATONVM_GEN_SERIAL_EDEN_AGE_CLOCK`): how many HotSpot
        // young collections this sweep stands for (1 when the clock is
        // unarmed, so every line below reads as before), and the
        // `should_tenure` age that makes a survivor aged by that step tenure
        // exactly when HotSpot's would. `promotion_age_reachable` (below)
        // only decides whether the walk runs, never what it tenures.
        // merge-dev-20261015: the two age sources folded (`merged_age_step`).
        let age_step = merged_age_step(promotion_age, self.age_clock_step());
        let tenure_age = clock_promotion_age(promotion_age, age_step);
        let promotion_age_reachable = self.stats.minor_gc_count.load(Ordering::Relaxed)
            >= u64::from(self.effective_promotion_age().saturating_sub(1));
        // gce e2/o: gcd d5/s's opt-in promotion hold on a requested major
        // (`CRATONVM_GEN_REQUESTED_MAJOR_NO_PROMOTE`) is removed -- gcd d9/a's
        // wide true-root seed keeps the seed on a promoting cycle, so the hold
        // bought nothing (`e1-p-triage.md` table 2B).
        // gci i47w/sg47 (`CRATONVM_GC_SYSTEM_GC_TENURES_ALL`, default ON since the 2026-10-17 flip): this
        // sweep is the young half of a `System.gc()`-door full collection
        // that ends with every movable live young object tenured, as HotSpot
        // Serial's mark-compact leaves young empty. The walk then runs from
        // the first cycle (`promotion_age_reachable` only says no object can
        // be old enough yet, which a forced tenure ignores; it was never a
        // safety gate) and forces the tenure below. The safety gates stay:
        // a frozen peer's un-rewritable state or the debug opt-out still
        // keep survivors young, as does a root pin or a full old gen.
        let system_gc_tenure = cratonvm_gc_core::gc_quiescence::system_gc_tenure_requested();
        let selective_on = (promotion_age_reachable || system_gc_tenure)
            && !gc_flags().no_selective_promote
            && !cratonvm_gc_core::gc_quiescence::unrewritable_peer_state();
        // gen r4w2/youngmark (2026-09-23): AGE-ONLY mode of the walk below.
        //
        // Since the Family-A hardening every young survivor is side-marked, and
        // this walk's deferred `age_bumps` is the ONLY place a side-marked
        // survivor is aged (the main sweep ages header-marked survivors only,
        // an arm that is dead on this path). Gating the whole walk on
        // `selective_on` therefore froze every survivor's age on the cycles the
        // gate refuses: the first `PROMOTION_AGE - 1` cycles (the comment above
        // called those walks "guaranteed no-op" — they were not, they are the
        // aging), and every `unrewritable_peer_state()` cycle, which on a
        // multi-threaded JIT workload can be most of them — leaving the
        // defragmentation escalation (promote everything, age-blind) as the
        // only drain. See
        // `docs/feature-designs/generative-gc-architecture.md`.
        //
        // On those cycles the same walk now runs with promotion OFF: no pin
        // decisions are acted on, nothing is allocated in old gen, no
        // forwarding pointer is installed, `evac_map` stays empty (so every
        // fixup pass below is skipped), and the age bumps it collects are
        // applied under the exact anchor-verified deferral discipline — unwind
        // truncates them — that already governs them on a promoting cycle.
        // Aging is a header write the peer-state gate has no quarrel with: that
        // gate exists because EVACUATION moves an object out from under a
        // frozen peer's derived pointer; an age byte moves nothing.
        // `CRATONVM_NO_SELECTIVE_PROMOTE` (the "pure non-moving sweep" debug
        // opt-out) keeps its old meaning: no walk at all.
        let age_only = !selective_on && !gc_flags().no_selective_promote;
        // gen r4w4/young4: the promote-on-pressure arm this cycle's divert
        // consumed (see the divert in `collect_garbage_inner`). Honoured only
        // on a PROMOTING walk — an age-only walk moves nothing — and counted
        // either way: consumed when it tenures, dropped when it cannot, so the
        // census still accounts for every arm.
        // gci i47w/sg47: a tenure-all `System.gc()` sweep forces the tenure
        // through the same slot; the promote-on-pressure census below still
        // counts the arm alone.
        let promote_pressure_tenure = (promote_pressure || system_gc_tenure) && selective_on;
        if promote_pressure {
            let slot = if selective_on { 1 } else { 3 };
            self.promote_pressure_census[slot].fetch_add(1, Ordering::Relaxed);
        }
        // gcf f2/y (`gcf-f1y-header-marked-sweep-survivors-age-outside-the-tenuring-age-table-20261001.md`):
        // the tenuring age table of this sweep, hoisted out of the
        // selective-promotion block. That block fills it from the side-marked
        // survivors it ages (the deferred `age_bumps`); the main sweep walk
        // below adds the HEADER-marked survivors it ages in place, which the
        // table used to miss because `adapt` ran inside the block, before the
        // walk. The threshold is now recomputed after the walk
        // (`sweep_ages_filled`), from every survivor that stayed young, as
        // HotSpot's `AgeTable::add` counts them. It is only read by the NEXT
        // cycle, so computing it later in the same pause changes nothing else.
        let sweep_tenuring_engaged = self.tenuring.engaged(adaptive_tenuring_flag());
        let sweep_tenuring_print = self.tenuring.print_requested()
            || (sweep_tenuring_engaged && self.verbose_gc_enabled());
        let sweep_age_census =
            sweep_tenuring_engaged || sweep_tenuring_print || self.tenuring.xlog_requested();
        let mut sweep_ages = [0u64; tenuring::AGE_TABLE_LEN];
        let mut sweep_ages_filled = false;
        // Wave 6 (`CRATONVM_GEN_SWEEP_FUSED_WALK`): the sweep walk's chunk
        // results, decided by the parallel promotion walk in the same pass,
        // with the empty-run bytes that pass counted; consumed by the sweep's
        // parallel prefix below.
        let mut fused_sweep: Option<(Vec<SweepChunkResult>, u64)> = None;
        if selective_on || age_only {
            let is_y = |a: usize| -> bool { a >= from_base && a < from_end && (a & 0x7) == 0 };

            // PERF: compute the sorted young free-block view (and `used`) ONCE
            // per sweep and reuse it across every selective-promotion pass.
            // `free_blocks_sorted()` collect-and-sorts the WHOLE free list, and
            // it was previously rebuilt at least three times per non-moving
            // sweep (evacuate pass (2), fixup pass (3a), and SP_VERIFY). The
            // `young_from` lock is held for the entire function and NOTHING in
            // this block mutates its free list or `used()` — all free-list
            // mutations (`add_free_block` / `clear_free_list` / `reset`) and any
            // `young_from` allocation happen strictly AFTER the `selective_on`
            // block, and evacuation only allocates into `old_gen`. So this
            // single snapshot is valid for every pass below; behavior is
            // identical, we just skip the redundant collect+sort each pass.
            //
            // gen r4/sweep (2026-09-23): and the same argument reaches further
            // back. Nothing between the oracle's `exact_skips` (built at the
            // top of this function from the identical expression) and here
            // mutates the free list either — the only `young_from` writes in
            // between are `take_alloc_anchors` / `clear_alloc_anchors`, which
            // touch the anchor slots alone — so this was a SECOND collect+sort
            // of the whole free list (and the sweep phase's `existing_free` a
            // THIRD) producing the same vector. On the fragmented arenas this
            // path exists for that list runs to hundreds of thousands of
            // entries. Borrow the one snapshot instead.
            let sweep_free_blocks: &[(usize, usize)] = &exact_skips;
            let sweep_used = young_from.used();
            debug_assert_eq!(
                sweep_used, used_bytes,
                "young `used` moved inside the sweep"
            );

            // ----- Defragmentation escalation (SB-LOADER-ZIPCONTENT, 2026-08-04)
            //
            // The age gate below is right for a nursery that gets compacted:
            // short-lived survivors stay in young, die there, and the *next*
            // moving cycle slides everything back together. This sweep never
            // slides anything. So on a process that stays on the non-moving
            // path — which is every process with a live JIT frame the coverage
            // proof cannot clear — young degenerates monotonically:
            //
            //   * the bump cursor reaches the top once and never resets (a
            //     survivor anywhere in the arena forbids a reset), so every
            //     later allocation is a free-list carve;
            //   * each cycle's survivors are a DIFFERENT few MB scattered over
            //     the whole arena, and almost none of them live to
            //     `PROMOTION_AGE`, so `promoted` stays at 0 forever;
            //   * the coalescer merges only ADJACENT holes, and a survivor
            //     between two holes is a wall it cannot cross.
            //
            // Measured on `ZipContentTests` (`CRATONVM_DBG=gc-overhead`): after
            // ~700 cycles the young from-space read `used=536,866,904`
            // (= capacity), `free_list=529,026,112` — **98.5 % of the arena
            // free** — and `largest_free=9,608`. Not a full heap; a heap whose
            // largest contiguous hole had fallen below the 8 KB array that
            // assertj allocates once per ZIP entry. Every such allocation then
            // forced a GC that freed ~10 KB, and the run either crawled or
            // (before the `try_alloc_array_full` fix in `jit/helpers.rs`) died
            // with `OutOfMemoryError` next to a gigabyte of old-gen headroom.
            //
            // The escalation: once the arena is bump-exhausted AND its largest
            // hole can no longer serve a modest contiguous request, tenure
            // every unpinned survivor regardless of age. That is the drain
            // selective promotion already implements and already argues safe —
            // pinning is BY RAW SLOT VALUE, so a conservative false positive
            // pins some object rather than mis-relocating one, and the age
            // gate was never part of that safety argument. With the survivors
            // gone the coalescer's walls come down and the free list collapses
            // back to a few huge spans.
            //
            // Self-limiting by construction: after one escalated cycle
            // `largest_free_block` is enormous, so the predicate is false again
            // — it cannot degenerate into promote-everything-always, which
            // would flood old gen with objects that die immediately.
            // gen r4w2/youngmark: `selective_on &&` — an age-only walk promotes
            // nothing, so it must neither escalate (an escalated `aged` is true
            // for every survivor, which would also suppress its age bump) nor
            // be counted as a selective sweep in `SP_CENSUS`.
            let defrag_escalate = selective_on
                && !gc_flags().no_defrag_promote
                && young_arena_needs_defragmentation(
                    sweep_used,
                    young_from.capacity(),
                    young_from.largest_free_block(),
                );
            if selective_on {
                SP_CENSUS.sweeps_selective.fetch_add(1, Ordering::Relaxed);
            }
            if defrag_escalate {
                SP_CENSUS.sweeps_defrag.fetch_add(1, Ordering::Relaxed);
            }

            // (1) Pin set: every root / finalizer value that lands in young.
            //
            // Stage B (precise oop maps, B-K relocation track): EXCLUDE addresses
            // published as movable precise-JIT roots, so the sweep can drain a
            // shadow-published oop instead of pinning it. A "movable" root is a
            // CLAIM that something will rewrite the frame's copy after the move —
            // the shadow-stack remap plus the JIT's post-safepoint reload.
            //
            // That claim is only good on a cycle that PROVED complete
            // moving-young coverage, so the exclusion is now conditioned on the
            // proof (`honour_movable`). The original code applied it
            // unconditionally, above a comment asserting the movable set is
            // "empty unless precise relocation is engaged, so byte-identical on
            // the default path". That was true when it was written and is FALSE
            // now: `conservative_roots::shadow_stack_enabled()` lost its
            // `moving_young_enabled() &&` term on 2026-07-31 and defaults to
            // TRUE, so `roots.rs` publishes every shadow-stack oop as movable on
            // EVERY collection.
            //
            // Left unconditional it is an active corruption hazard, and
            // specifically one that HIB-GCOVERHEAD-HALFFULL.1's fix would have
            // armed: that fix re-enables selective promotion on
            // coverage-INCOMPLETE cycles, which are exactly the cycles where a
            // frame could not prove it publishes and can rewrite all of its
            // oops. Evacuating a root on the word of a proof that just failed
            // inverts the sweep's whole safety argument — pin-by-value, which
            // over-retains and can never dangle.
            //
            // Pinning a root for one cycle costs nothing but a deferred
            // promotion: it is a root, so it is not the interior heap object the
            // drain is actually for. The bt18-critical drain is unaffected.
            let honour_movable = !cratonvm_gc_core::gc_quiescence::moving_young_coverage_incomplete();
            let mut pinned: FxHashSet<usize> = FxHashSet::default();
            // A conservative root is frequently an INTERIOR word (a field
            // address, a derived pointer, a spilled register mid-object), and
            // `mark_young` retains the object it points into by resolving it
            // through the exact-base oracle. Pinning only the raw address
            // therefore leaves that resolved base promotable — and promotion
            // moves it, while the raw stack/register word that kept it alive
            // cannot be rewritten. The source is then freed and zeroed, and
            // the next dereference through the root reads an all-zero header
            // (`class_id=ClassId(0)`, `num_slots=0`). Pin the base too.
            //
            // Measured before this fix on `bench/BinTreesClassic 18` under
            // `CRATONVM_NO_MOVING_YOUNG=1`: 13 of 16 runs had at least one root
            // pointing into a span the sweep was about to zero, every one of
            // them `side_marked_base=true header_marked=false forwarded=true`
            // with a non-zero interior offset. Zero after it.
            //
            // Over-retention only: the object stays in young one more cycle.
            let pin_base_of = |a: usize, pinned: &mut FxHashSet<usize>| {
                pinned.insert(a);
                // Same lookup `mark_young` performs. `young_object_ranges`
                // holds a range only for objects that cover a conservative
                // candidate, i.e. exactly these addresses.
                if let Some(&(base, end)) = young_object_ranges
                    .partition_point(|(start, _)| *start <= a)
                    .checked_sub(1)
                    .and_then(|idx| young_object_ranges.get(idx))
                {
                    if a < end {
                        pinned.insert(base);
                    }
                }
            };
            // `is_movable_jit_root` is a claim about ONE slot and the pin set is
            // keyed by OBJECT, so a single rewritable channel naming an address
            // licensed moving it out from under every OTHER word that also held
            // it — including a compiled frame's callee-saved register image,
            // which `band_slot_is_verifiable` refuses to inspect and no channel
            // rewrites. `is_unrewritable_jit_root` is the veto: the band scan
            // publishes there when a word in one of those regions resolves to a
            // live object, and it outranks any movable claim.
            //
            // gcd d2/g (`CRATONVM_GEN_PRECISE_ROOT_PROMOTE`, opt-in): a young
            // value EVERY occurrence of which in `roots` came from a precise
            // table the post-collection fix-up rewrites (statics, interned
            // strings, class mirrors, JNI globals) is not pinned either; see
            // `tenuring::precise_only_root_values`. Empty (nothing exempt)
            // unless the VM's root scan published counts on this thread.
            //
            // gcd d5/r (cross-lane, applied by d5/s): the SEALED take, which
            // refuses a table that does not describe this sweep's root list
            // (`tenuring::take_sealed_precise_root_values`).
            //
            // JIT r20 wave 18, lane gcI: the counting rule sees only the words
            // that ENTERED `roots`. A compiled frame's word the band scan left
            // out on a deadness claim (map liveness, the dead-spill cursor,
            // `spill_written`), or one no scan reads, still names the object --
            // and under the default every root value is pinned, so such a word
            // never mattered. Exempting a static's value moved the object out
            // from under it: gm-I4 on w16a printed four `[root-remap]` findings
            // in `scan_section=14` (the conservative JIT spill scan) and then
            // SIGSEGV'd in compiled code. The exemption is a relocation licence,
            // so it takes the young pin ledger's word the way the pinned copy
            // does (gen r5w4/pin8: a licensing ledger trusts no deadness
            // claim): an INCOMPLETE ledger exempts nothing, and any ledger word
            // inside the object vetoes it.
            let precise_only: FxHashSet<usize> =
                tenuring::take_sealed_precise_root_values(roots.len())
                    .map(|precise| {
                        // gci i6/pn (conc10): a value any published
                        // conservative JIT pin names is never exempt, even
                        // when that pin never entered `roots` (counted
                        // `prp_conservative_vetoed`). Built only on a sweep
                        // that took a table, i.e. only under the switch.
                        let conservative: FxHashSet<usize> =
                            cratonvm_gc_core::gc_quiescence::pinned_jit_roots_snapshot()
                                .into_iter()
                                .collect();
                        // gc lane I (wave 18): a value the young pin ledger
                        // names is never exempt either; an unreadable ledger
                        // exempts nothing.
                        match cratonvm_gc_core::gc_quiescence::pause_young_pin_word_ranges() {
                            Some(names) => tenuring::precise_only_root_values_vetoing_conservative(
                                roots,
                                &precise,
                                &is_y,
                                |a| {
                                    cratonvm_gc_core::gc_quiescence::is_unrewritable_jit_root(a)
                                        || cratonvm_gc_core::gc_quiescence::ledger_names_young_object(
                                            &names, a,
                                        )
                                },
                                &conservative,
                            ),
                            None => {
                                tenuring::precise_only_root_values(roots, &precise, &is_y, |_| true)
                            }
                        }
                    })
                    .unwrap_or_default();
            // gci i6/pn (conc10): with the root-remap audit armed, record
            // which young root values left the pin set and through which
            // exemption, so the audit can label each finding
            // (`tenuring::take_young_root_exemptions`). Off: no allocation.
            let mut movable_exempt: Option<FxHashSet<usize>> =
                tenuring::young_root_exemptions_wanted().then(FxHashSet::<usize>::default);
            for r in roots.iter() {
                let a = r.as_ptr() as usize;
                let movable = honour_movable
                    && cratonvm_gc_core::gc_quiescence::is_movable_jit_root(a)
                    && !cratonvm_gc_core::gc_quiescence::is_unrewritable_jit_root(a);
                if is_y(a) && !movable && !precise_only.contains(&a) {
                    pin_base_of(a, &mut pinned);
                } else if movable && is_y(a) {
                    if let Some(set) = movable_exempt.as_mut() {
                        set.insert(a);
                    }
                }
            }
            if let Some(movable) = movable_exempt {
                tenuring::record_young_root_exemptions(tenuring::YoungRootExemptions {
                    prp: precise_only.clone(),
                    movable,
                });
            }
            // gcd d9/e (`gcd-d9e-non-moving-sweep-pins-every-live-finalizable-young-DONE-20260929.md`):
            // under `CRATONVM_GEN_PROMOTE_LIVE_FINALIZABLES` (default on since
            // gce, 2026-09-29; `=0` restores pinning every young registered
            // finalizable, unless the remark hook is on), a finalizable the
            // strong closure reached (not in `dead_finalizers`) is an ordinary
            // survivor: its processor row, the finalizer queue and the
            // concurrent finalizer channel all follow `evac_map` through the
            // pause's pointer map. Only the ones this sweep resurrected stay
            // pinned: `dead_finalizers` reports them at their pre-collection
            // addresses.
            // gce e1/c: the remark hook implies the promotion (d9/e behaviour
            // note).
            let promote_live_fin = true || cratonvm_gc_core::gc_flags().gen_conc_remark_refproc_hook;
            let dead_fin: FxHashSet<usize> = if promote_live_fin {
                dead_finalizers.iter().copied().collect()
            } else {
                FxHashSet::default()
            };
            for &a in finalizer_addrs.iter() {
                if is_y(a) && (!promote_live_fin || dead_fin.contains(&a)) {
                    pin_base_of(a, &mut pinned);
                }
            }
            // gen r5w2/roots6: a walk-gap word from the dirty-card scan can
            // never be rewritten (it may not be a reference), so the object it
            // names must not move -- pinned like any conservative root.
            for &a in &card_gap_roots.young_words {
                if is_y(a) {
                    pin_base_of(a, &mut pinned);
                }
            }
            // gen r4/sweep (2026-09-23): the bases the LATE resolution passes
            // proved from conservative candidates. `pin_base_of` can only see
            // bases the anchor ORACLE resolved (`young_object_ranges`), so a
            // conservative INTERIOR word whose interval the oracle discarded
            // pinned only its raw address — while its base, marked by the late
            // pass, stayed promotable. Evacuating it moves the object out from
            // under a stack/register word that cannot be rewritten, and the
            // sweep then zeroes the source: exactly the hazard the comment
            // above describes, reached through the other resolution path. (The
            // root-in-dead-span invariant cannot catch it either: it resolves
            // the root through the same `young_object_ranges`, falls back to
            // the span head, and counts a forwarded head as a benign
            // evacuation.) Over-retention only, and only on cycles where the
            // oracle left something unresolved (`LATE_RESOLVE_CYCLES`).
            for &b in &late_conservative_bases {
                pinned.insert(b);
            }

            // (2) Evacuate non-pinned marked survivors to old gen. Record
            // young→old in `evac_map` (returned for the VM-level remap). Stop
            // if old gen fills (leave the remainder in young — correctness
            // over completeness).
            //
            // DoHead walk-desync hardening (2026-07-02): forwarding-pointer
            // installs into the young sources are DEFERRED and only applied
            // for candidates collected on ANCHOR-VERIFIED stretches of the
            // walk grid. The old exact-match free-block skip (`cursor == off`)
            // wedged after a single overshoot and then strode every later
            // free block as phantom zeroed "objects" — a phantom that
            // happened to read as marked+aged was "evacuated" with a
            // forwarding pointer written INTO a live object's interior, which
            // the main sweep then zeroed and freed via `is_forwarded()` (the
            // fatal mid-live-object reclaim). Now: robust skip; every anomaly
            // (zero span, implausible header, free-block overshoot, span
            // crossing a free hole) UNWINDS the candidates collected since
            // the last trustworthy anchor (a free-block boundary) — their
            // already-copied old-gen bytes become unreachable garbage for the
            // next major GC — and the walk re-anchors at the next free block.
            // Candidates between two clean anchors (or between the last
            // anchor and a clean end-of-walk) are grid-verified and their
            // installs proceed. This bounds phantom installs without
            // starving promotion when a persistent unparseable span exists
            // (only a moving cycle reclaims those). Survivors inside skipped
            // stretches are handled by pass 3a's conservative rewrite.
            let mut evacuated: Vec<*mut u8> = Vec::new();
            let mut fwd_installs: Vec<(usize, *mut u8)> = Vec::new();
            // Deferred survivor age bumps (applied with the forwarding
            // installs below, under the same anchor-verified discipline).
            // The unconditional side-marking hardening removed every
            // mark-time header write — which silently stopped survivor
            // aging, and with it this whole pass (no object could ever
            // reach PROMOTION_AGE, so a fully-live young could never drain
            // to old gen and the heap wedged into the old-gen-spill → abort
            // path). Aging now happens here, where the walk grid validates
            // each header before any write lands on it.
            let mut age_bumps: Vec<usize> = Vec::new();
            // gcf f4/y (f3/q review finding 4): the four diagnostic gates the
            // promotion copy below consults, read ONCE per walk instead of
            // once per promoted object (each was a `gc_flags()` snapshot or a
            // `OnceLock` load). All four are fixed for the process (env at
            // start), so the answers are the same.
            let promo_cell_diag = cell_corrupt_diag_enabled();
            let promo_cell_watch = cratonvm_gc_core::heap::cell_watch_addr();
            let promo_obj_watch = obj_watch_pattern().is_some();
            let promo_audit = cratonvm_gc_core::root_write_audit::enabled();
            // gcf f3/q (r15w5/gcpage patch 3, `CRATONVM_GEN_SWEEP_3A_REPLAY`):
            // what walk (2) saw, so fix-up 3a can visit `side_sorted` instead
            // of re-walking the arena. `grid_clean` stays true only if no step
            // of the walk was an anomaly (overshoot, zero span, bad size, bad
            // gap filler, hole crossing -- which also covers the walk's only
            // early `break`); the two counts are over the grid objects the
            // walk parsed.
            let mut grid_clean = true;
            let mut grid_side_marked = 0usize;
            let mut grid_header_marked = 0usize;
            // gch h1/yy: `CRATONVM_DBG_GCPHASE=1` splits this block's one
            // `[gcphase] selective-promotion:` figure into walk (2), the
            // installs + age bumps, and fix-ups 3a / 3b / 3c, on a separate
            // `[gcphase] selective-split:` line (the existing phase lines are
            // unchanged). `sp_split_marks[i]` is the end of part `i`; a part
            // that did not run (no promotion) keeps `None` and prints 0.
            let sp_split_t0 = phase_diag.then(std::time::Instant::now);
            let mut sp_split_marks: [Option<std::time::Instant>; 5] = [None; 5];
            let mut sp_split_par_walk = false;
            let mut sp_split_3c_holders = 0usize;
            let mut sp_split_3c_bytes = 0usize;
            {
                YOUNG_WALK_ENTRIES[0].fetch_add(1, Ordering::Relaxed);
                // gcf f3/q (r15w5/gcpage patches 1 and 2): one size memo for
                // the walk, and the census summed here and published once
                // after it (`batch`) instead of locked RMWs per object.
                let mut size_memo = GridSizeMemo::new(size_memo_on);
                let batch = gen_sweep_batch_census_enabled();
                let mut tally = EvacWalkTally::default();
                // PERF: reuse the once-computed sorted free-block snapshot.
                let mut free_iter = sweep_free_blocks.iter().peekable();
                let used = sweep_used;
                let mut cursor = 0usize;
                let mut old_full = false;
                // gen r4/sweep (2026-09-23): monotone cursor into `late_pins`
                // (ascending — `resolve_candidate_bases` returns a suffix of its
                // sorted input). This walk only ever moves `cursor` forward.
                let mut late_pin_idx = 0usize;
                // Candidate counts at the last trustworthy anchor.
                let mut fwd_wm = 0usize;
                let mut evac_wm = 0usize;
                let mut age_wm = 0usize;
                // Drop candidates collected since the last anchor (suspect
                // stretch): remove them from evac_map, orphan their copies.
                //
                // gen r4/sweep (2026-09-23): also RETRACTS the promotion
                // accounting the copy loop booked for them. `bytes_promoted` /
                // `objects_promoted` (the GC-overhead productivity input) and
                // `SP_CENSUS.evacuated` were bumped at copy time and never
                // corrected, so every unwound candidate was reported as a
                // promotion that did useful work while its copy was orphaned
                // garbage and its source stayed in young.
                #[allow(clippy::too_many_arguments)]
                fn unwind_evac(
                    fwd_installs: &mut Vec<(usize, *mut u8)>,
                    evacuated: &mut Vec<*mut u8>,
                    evac_map: &mut cratonvm_types::PointerMap,
                    age_bumps: &mut Vec<usize>,
                    fwd_wm: usize,
                    evac_wm: usize,
                    age_wm: usize,
                    stats: &HeapStats,
                    // gcf f3/q: the walk's batched census when
                    // `CRATONVM_GEN_SWEEP_BATCH_CENSUS` is on; the unwound
                    // copies were added to it, not to the atomics.
                    tally: Option<&mut EvacWalkTally>,
                ) {
                    age_bumps.truncate(age_wm);
                    if fwd_installs.len() > fwd_wm {
                        EVAC_UNWIND_CANDIDATES
                            .fetch_add((fwd_installs.len() - fwd_wm) as u64, Ordering::Relaxed);
                        let n = SWEEP_PROMOTION_ABORT_HITS.fetch_add(1, Ordering::Relaxed);
                        if n < 8 {
                            tracing::warn!(
                                "selective promotion: unwound {} candidate(s) collected \
                                 on a suspect walk stretch (grid anomaly since last anchor)",
                                fwd_installs.len() - fwd_wm,
                            );
                        }
                        for (src, _) in fwd_installs.drain(fwd_wm..) {
                            evac_map.remove(&src);
                        }
                        let unwound = evacuated.len().saturating_sub(evac_wm);
                        let mut unwound_bytes = 0u64;
                        for &dst in evacuated.iter().skip(evac_wm) {
                            // SAFETY: every `evacuated` entry is an old-gen copy
                            // this walk allocated and wrote in full (header
                            // included) earlier in this same pause; nothing has
                            // written to it since.
                            let h = unsafe { &*(dst as *const ObjectHeader) };
                            unwound_bytes += gen_object_total_size(h) as u64;
                        }
                        match tally {
                            Some(t) => t.retract(unwound as u64, unwound_bytes),
                            None => {
                                stats
                                    .bytes_promoted
                                    .fetch_sub(unwound_bytes, Ordering::Relaxed);
                                stats
                                    .objects_promoted
                                    .fetch_sub(unwound as u64, Ordering::Relaxed);
                                SP_CENSUS
                                    .evacuated
                                    .fetch_sub(unwound as u64, Ordering::Relaxed);
                            }
                        }
                        evacuated.truncate(evac_wm);
                    }
                }
                // gch h1/yy (`CRATONVM_GEN_SWEEP_PARALLEL_PROMOTION_WALK`,
                // default ON since gch h3/yc): decide the walk on the pool as anchored chunks
                // (`parallel_promotion_walk`: the serial walk's grid and
                // decisions, refused on any anomaly), then copy the candidates
                // here, in ascending order, exactly as the loop below does --
                // so the old-gen destinations, `fwd_installs`, `evac_map`,
                // `evacuated`, `age_bumps`, the census and the 3a proof come
                // out as the serial walk leaves them on a clean grid. Taken
                // only when nothing the chunks do not model is armed: a late
                // pin (the serial walk's lockstep), the unbatched census, the
                // four per-promotion diagnostics. On success `cursor = used`
                // and the serial loop has nothing left to walk; on a refusal
                // the serial walk runs from scratch (the chunks wrote nothing).
                let par_walk = batch
                    && late_pins.is_empty()
                    && !promo_cell_diag
                    && promo_cell_watch == 0
                    && !promo_obj_watch
                    && !promo_audit
                    // gch h3/yc (with the default flip): at least TWO chunks.
                    // One chunk is the serial walk recomputed into vectors and
                    // replayed -- the same decisions at an extra cost (and a
                    // pool dispatch for one worker).
                    && sweep_anchors.len() >= 3
                    && sweep_anchors[0] == 0
                    && sweep_anchors[sweep_anchors.len() - 1] == used
                    && sweep_anchors.windows(2).all(|w| w[0] < w[1]);
                if par_walk {
                    // Wave 6 (`CRATONVM_GEN_SWEEP_FUSED_WALK`): decide the
                    // sweep walk's chunks in this pass too, when the sweep's
                    // parallel prefix would run on the same anchors (its
                    // preconditions, re-checked where the result is used).
                    let fused_watched = {
                        cratonvm_gc_core::gc_quiescence::watched_referents_snapshot_for(
                            self.watched_set_key(),
                        )
                    };
                    let fuse_ok = sweep_anchors.len() > 2
                        && !(sweep_zero_enabled()
                            || cratonvm_gc_core::a2dbg::enabled()
                            || gc_flags().dbg_sweep_census)
                        && !gc_flags().dbg_a2
                        && !watchref_dbg()
                        && !gc_flags().dbg_sweep_liveness;
                    let fuse = fuse_ok.then(|| {
                        let (old_lo, old_hi) = old_gen.extent();
                        FusedSweepCtx {
                            old_lo,
                            old_hi,
                            watched: fused_watched.as_ref(),
                        }
                    });
                    let empty_runs_before = EMPTY_RUN_BYTES_CYCLE.load(Ordering::Relaxed);
                    let walk_ctx = PromotionWalkCtx {
                        from_base,
                        used,
                        free_blocks: sweep_free_blocks,
                        side_bits: &side_bits,
                        side_sorted: &side_sorted,
                        pinned: &pinned,
                        selective_on,
                        force_tenure: defrag_escalate || promote_pressure_tenure,
                        // gci i1/yg: the age-clock-adjusted tenuring age.
                        promotion_age: tenure_age,
                        size_memo: size_memo_on,
                        fuse,
                    };
                    let walk_threads = cratonvm_gc_core::young_mark::young_gc_threads(bytes_before);
                    let walk_pool = (walk_threads > 1).then(|| self.evac_pool(walk_threads));
                    // Wave 8 (`CRATONVM_GEN_SWEEP_DEAD_CHUNK_SKIP`, proposal
                    // 14): on the fused walk, a chunk with no live data is
                    // declared dead whole instead of walked
                    // (`dead_chunk_sweep`). Declined while an in-JIT peer's
                    // un-retired TLAB tail is published: the bytes just below
                    // such a tail may hold an object its owner is still
                    // writing, which only the walk's zero-run rule keeps.
                    let dead_skip_on = fuse.is_some() && gen_sweep_dead_chunk_skip_enabled();
                    if dead_skip_on && !jit_skips.is_empty() {
                        self.young_premium_census[YP_DEAD_CHUNK_DECLINED]
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    let dead_keep: Vec<usize> = if dead_skip_on && jit_skips.is_empty() {
                        let mut keep: Vec<usize> = conservative_candidates
                            .iter()
                            .chain(unresolved_snapshot.iter())
                            .chain(pinned.iter())
                            .copied()
                            .collect();
                        keep.sort_unstable();
                        keep.dedup();
                        keep
                    } else {
                        Vec::new()
                    };
                    let dead_screen = (dead_skip_on && jit_skips.is_empty())
                        .then_some(DeadChunkScreen { keep: &dead_keep });
                    // Wave 8 (`CRATONVM_GEN_SWEEP_DEAD_CHUNK_FINE`): with the
                    // screen, walk on the finer grid (`fine_dead_chunk_anchors`),
                    // so a chunk the live data touches costs about a TLAB,
                    // not 8 MiB. Same certified sources, same ends.
                    let fine_anchors: Option<Vec<usize>> = (dead_screen.is_some())
                    .then(|| fine_dead_chunk_anchors(&grid_anchors, used))
                    .filter(|a| a.len() > sweep_anchors.len());
                    let walk_anchors: &[usize] = fine_anchors.as_deref().unwrap_or(&sweep_anchors);
                    match parallel_promotion_walk_screened(
                        &walk_ctx,
                        walk_anchors,
                        walk_threads,
                        walk_pool,
                        dead_screen.as_ref(),
                    ) {
                        Some(mut chunks) => {
                            self.young_premium_census[YP_PAR_WALKS].fetch_add(1, Ordering::Relaxed);
                            sp_split_par_walk = true;
                            // Wave 6: the fused sweep result is usable only if
                            // this cycle promotes nothing (no forwarding
                            // install will land between here and the sweep).
                            // The empty-run figure the fused pass added is the
                            // one the sweep's own pass would add (same rules,
                            // same grid); the sweep resets the counter before
                            // its walk and restores this delta.
                            if fuse.is_some()
                                && chunks
                                    .iter()
                                    .all(|c| c.candidates.is_empty() && c.sweep.is_some())
                            {
                                // Wave 8: the dead-whole chunks this sweep takes.
                                let (dead_n, dead_b) = chunks
                                    .iter()
                                    .filter(|c| c.dead_chunk_bytes > 0)
                                    .fold((0u64, 0u64), |(n, b), c| {
                                        (n + 1, b + c.dead_chunk_bytes as u64)
                                    });
                                if dead_n > 0 {
                                    self.young_premium_census[YP_DEAD_CHUNKS]
                                        .fetch_add(dead_n, Ordering::Relaxed);
                                    self.young_premium_census[YP_DEAD_CHUNK_BYTES]
                                        .fetch_add(dead_b, Ordering::Relaxed);
                                }
                                let swept: Vec<SweepChunkResult> =
                                    chunks.iter_mut().filter_map(|c| c.sweep.take()).collect();
                                let runs = EMPTY_RUN_BYTES_CYCLE
                                    .load(Ordering::Relaxed)
                                    .saturating_sub(empty_runs_before);
                                fused_sweep = Some((swept, runs));
                            }
                            for chunk in chunks {
                                grid_side_marked += chunk.side_marked;
                                grid_header_marked += chunk.header_marked;
                                size_memo.hits += chunk.memo_hits;
                                tally.candidates += chunk.census_candidates;
                                tally.unaged += chunk.census_unaged;
                                tally.pinned += chunk.census_pinned;
                                age_bumps.extend_from_slice(&chunk.age_bumps);
                                for &(addr, total_size) in &chunk.candidates {
                                    // A full old gen stops ALLOCATING, as in
                                    // the loop below; the remaining candidates
                                    // stay young.
                                    if old_full {
                                        break;
                                    }
                                    let src = addr as *mut u8;
                                    match old_gen.alloc(total_size, 8) {
                                        Some(dst) => {
                                            // SAFETY: src/dst are valid,
                                            // non-overlapping, total_size bytes
                                            // (the chunk sized `src` on the
                                            // verified grid; `dst` was just
                                            // allocated at that size).
                                            unsafe {
                                                std::ptr::copy_nonoverlapping(src, dst, total_size)
                                            };
                                            // The loop's one-store mark word
                                            // (gcf f1/y), verbatim.
                                            // SAFETY: both headers are fully
                                            // written.
                                            let src_mark = unsafe {
                                                (*(src as *const ObjectHeader))
                                                    .mark_word
                                                    .load(Ordering::Relaxed)
                                            };
                                            // SAFETY: dst is a freshly written
                                            // object header.
                                            let dhdr = unsafe { &mut *(dst as *mut ObjectHeader) };
                                            dhdr.mark_word.store(
                                                ObjectHeader::mark_with_gc_flags(
                                                    src_mark,
                                                    GC_FLAG_OLD_GEN,
                                                ),
                                                Ordering::Relaxed,
                                            );
                                            if ObjectHeader::gc_flags_of(src_mark) & GC_FLAG_MARKED
                                                != 0
                                            {
                                                dhdr.clear_gc_flags(GC_FLAG_MARKED);
                                            }
                                            fwd_installs.push((addr, dst));
                                            evac_map.insert(addr, dst as usize);
                                            evacuated.push(dst);
                                            tally.promoted_bytes += total_size as u64;
                                            tally.promoted_objects += 1;
                                            tally.evacuated += 1;
                                        }
                                        None => {
                                            tally.old_full += 1;
                                            old_full = true;
                                        }
                                    }
                                }
                            }
                            cursor = used;
                        }
                        None => {
                            self.young_premium_census[YP_PAR_WALK_FALLBACKS]
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                // gen r4/sweep (2026-09-23): the walk no longer STOPS when old
                // gen fills (`&& !old_full` used to end this loop). Stopping
                // mid-stretch left every candidate collected since the last
                // anchor installed WITHOUT the anchor verification the header
                // comment promises ("between two clean anchors, or between the
                // last anchor and a clean end-of-walk") — the exact
                // phantom-forwarding-pointer shape the unwind discipline exists
                // for — and it also silently skipped the deferred age bump for
                // every survivor past the stop. Now `old_full` only stops
                // ALLOCATING; the walk runs to its clean end (or unwinds) exactly
                // as on a cycle whose old gen had room.
                while cursor < used {
                    {
                        let (resynced, overshot) = skip_free_blocks(&mut cursor, &mut free_iter);
                        if overshot {
                            // The stride that crossed into the block was
                            // mis-sized: candidates since the last anchor are
                            // suspect. The cursor now sits at the block end —
                            // a fresh anchor.
                            grid_clean = false;
                            EVAC_UNWIND_REASONS[0].fetch_add(1, Ordering::Relaxed);
                            unwind_evac(
                                &mut fwd_installs,
                                &mut evacuated,
                                &mut evac_map,
                                &mut age_bumps,
                                fwd_wm,
                                evac_wm,
                                age_wm,
                                &self.stats,
                                if batch { Some(&mut tally) } else { None },
                            );
                        }
                        if resynced {
                            fwd_wm = fwd_installs.len();
                            evac_wm = evacuated.len();
                            age_wm = age_bumps.len();
                            continue;
                        }
                    }
                    let src = (from_base + cursor) as *mut u8;
                    // gen r5w1/young5: the grid rules are `probe_grid_at`'s,
                    // with this walk's policy kept verbatim below.
                    //
                    // Bug-D fix (2026-06-12): a GAP-filler sentinel (a
                    // sub-`HEADER_SIZE` TLAB tail) is strided before any size
                    // read. This selective-promotion walk runs before the main
                    // sweep reclaims gaps, so sentinels are still in place here.
                    // A malformed one is parsed as a header (the size and hole
                    // checks then judge it), as it always was here.
                    //
                    // All-zero header word: an unlisted zeroed span (never a
                    // walkable object unless the zero run is shorter than a
                    // header — the real `ClassId(0)` container case, which
                    // parses normally). A run of EMPTY objects is not a
                    // desync. Stepping over it is doubly safe here: an empty
                    // object has no fields, so there is nothing in the run to
                    // promote and nothing to age. Unwinding instead discards
                    // every promotion candidate collected since the last
                    // anchor — 69 194 of them in one measured run at
                    // `--Xmx 320m` — and under a permanent non-moving sweep
                    // promotion is the young generation's ONLY exit for live
                    // data. `unresolved: None`: the proved-base resume (which
                    // needs the unresolved-mark set to tell a proved base from
                    // a raw conservative interior mark) is enabled only on the
                    // main sweep's sequential walk, the one measured for it; a
                    // refusal here unwinds to the last anchor exactly as it
                    // did before.
                    let next_free = free_iter.peek().map(|&&(off, _)| off);
                    let rules = GridRules {
                        base: from_base,
                        used,
                        zero_run_hi: used,
                        side_sorted: &side_sorted,
                        zero_runs: ZeroRunRule::StepOver {
                            unresolved: None,
                            count_empty_runs: true,
                        },
                        bad_gap_filler_is_anomaly: false,
                        phantom: false,
                    };
                    let mut no_phantom_probe = 0usize;
                    // `cursor < used`; from-space is mapped.
                    // gcf f3/q: sized through the walk's memo (patch 1).
                    let verdict = probe_grid_at_sized(
                        cursor,
                        next_free,
                        &rules,
                        &mut no_phantom_probe,
                        &mut size_memo,
                    );
                    let total_size = match verdict {
                        GridProbe::Object { size } => size,
                        // Not produced under `phantom: false`; kept as an
                        // object as before, but it voids the 3a replay proof.
                        GridProbe::Phantom { size, .. } => {
                            grid_clean = false;
                            size
                        }
                        GridProbe::Gap { len } => {
                            cursor += len;
                            continue;
                        }
                        GridProbe::EmptyRun { resume } => {
                            cursor = resume;
                            continue;
                        }
                        // Anomaly: unwind candidates since the last anchor
                        // and re-anchor at the next free block. Reason 1 is
                        // the unlisted zero span, 2 the implausible header
                        // (`BadGapFiller` is not produced under these rules;
                        // it is the implausible-header family either way).
                        GridProbe::ZeroSpan { .. }
                        | GridProbe::BadSize { .. }
                        | GridProbe::BadGapFiller => {
                            grid_clean = false;
                            let zero_span = matches!(verdict, GridProbe::ZeroSpan { .. });
                            EVAC_UNWIND_REASONS[if zero_span { 1 } else { 2 }]
                                .fetch_add(1, Ordering::Relaxed);
                            unwind_evac(
                                &mut fwd_installs,
                                &mut evacuated,
                                &mut evac_map,
                                &mut age_bumps,
                                fwd_wm,
                                evac_wm,
                                age_wm,
                                &self.stats,
                                if batch { Some(&mut tally) } else { None },
                            );
                            if resync_to_next_free_block(&mut cursor, &mut free_iter) {
                                continue;
                            }
                            break;
                        }
                        // Over-sized-header clamp (mirrors the main walk): an
                        // object can never span a pre-existing free hole. Do
                        // not evacuate through such a header; candidates since
                        // the last anchor are suspect.
                        GridProbe::CrossesFreeBlock { free_off, .. } => {
                            grid_clean = false;
                            EVAC_UNWIND_REASONS[3].fetch_add(1, Ordering::Relaxed);
                            unwind_evac(
                                &mut fwd_installs,
                                &mut evacuated,
                                &mut evac_map,
                                &mut age_bumps,
                                fwd_wm,
                                evac_wm,
                                age_wm,
                                &self.stats,
                                if batch { Some(&mut tally) } else { None },
                            );
                            cursor = free_off;
                            continue;
                        }
                    };
                    // SAFETY: `probe_grid_at` parsed a plausible header at this
                    // in-bounds, on-grid offset of mapped from-space.
                    let header = unsafe { &*(src as *const ObjectHeader) };
                    let addr = src as usize;
                    // Only tenure objects that have survived enough GCs
                    // (gc_age+1 >= PROMOTION_AGE), exactly like the moving
                    // collector. Promoting on the first survival would flood old
                    // gen with objects that die almost immediately (bintrees'
                    // short-lived trees), defeating the drain and starving old
                    // gen. Short-lived survivors stay in young and are reclaimed
                    // normally; only the genuinely long-lived set (the
                    // persistent tree) ages out and promotes.
                    // Survivor liveness comes from EITHER mark channel: the
                    // legacy header mark, or the side-mark set. Since the
                    // unconditional side-marking hardening, the marker never
                    // header-writes, so `side_marks` is the only live channel
                    // — requiring the header mark alone made this whole pass
                    // inert (nothing evacuated, nothing aged; a fully-live
                    // young then wedged the heap into the old-gen-spill →
                    // abort path that the native-alloc boundary GC exists to
                    // relieve).
                    // `should_tenure` is the single implementation of this
                    // rule, shared with the serial and parallel moving
                    // evacuators; `defrag_escalate` occupies the
                    // `force_promote_all` slot here. Its doc explains why the
                    // `saturating_add` inside it is load-bearing.
                    // gen r4w4/young4: so does a consumed promote-on-pressure
                    // arm (`promote_pressure_tenure`) — the moving evacuators'
                    // own reading of that slot.
                    let aged = should_tenure(
                        header.gc_age(),
                        defrag_escalate || promote_pressure_tenure,
                        tenure_age, // gci i1/yg: `promotion_age` at step 1
                    );
                    let header_marked = header.gc_flags() & GC_FLAG_MARKED != 0;
                    // gcf f3/q (patch 3): `contains` has no side effect, so
                    // asking it for a header-marked object too changes nothing
                    // but lets the walk count both channels for 3a's proof.
                    let side_marked = side_bits.contains(addr);
                    let marked = header_marked || side_marked;
                    grid_side_marked += usize::from(side_marked);
                    grid_header_marked += usize::from(header_marked);
                    // gen r4/sweep (2026-09-23): an object whose extent holds a
                    // LATE PIN — a conservative candidate neither the oracle nor
                    // the late grid walk could place — is pinned too. The sweep
                    // already refuses to reclaim such an object (`late_pinned`),
                    // but it tests `is_forwarded()` FIRST, so an object this
                    // walk evacuated was reclaimed and zeroed under the very
                    // root that pin exists for. This walk can reach stretches
                    // the late walk could not (it resyncs at free blocks, the
                    // late walk stops), which is how such an object can be a
                    // promotion candidate at all. Free when `late_pins` is
                    // empty, which is every healthy cycle.
                    let late_pinned_here = if late_pins.is_empty() {
                        false
                    } else {
                        while late_pin_idx < late_pins.len() && late_pins[late_pin_idx] < addr {
                            late_pin_idx += 1;
                        }
                        late_pins
                            .get(late_pin_idx)
                            .is_some_and(|&p| p < addr + total_size)
                    };
                    // gcf f3/q (patch 2): `is_pinned` is read only under
                    // `marked` (the census and the promotion gate below), and
                    // `pinned.contains` has no side effect, so a dead object
                    // skips the hash probe when batching.
                    let is_pinned =
                        late_pinned_here || ((marked || !batch) && pinned.contains(&addr));
                    // Census (see `SP_CENSUS`): classify every marked survivor
                    // the walk reaches, so a `promoted=0` run says WHICH of the
                    // three rejections it was.
                    // (Not on an age-only walk: the census classifies
                    // PROMOTION candidates, and there are none.)
                    if marked && selective_on {
                        if batch {
                            tally.candidates += 1;
                            if !aged {
                                tally.unaged += 1;
                            } else if is_pinned {
                                tally.pinned += 1;
                            }
                        } else {
                            SP_CENSUS.candidates.fetch_add(1, Ordering::Relaxed);
                            if !aged {
                                SP_CENSUS.unaged.fetch_add(1, Ordering::Relaxed);
                            } else if is_pinned {
                                SP_CENSUS.pinned.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    // gcf f3/q (f2/y review finding 4): a survivor that is
                    // side-marked AND carries a stale `GC_FLAG_MARKED` is aged
                    // here too. The main walk tests the side mark FIRST and its
                    // side arm writes nothing, so before this such an object
                    // was aged by neither pass until the clear-marks re-walk
                    // dropped the stale bit. The main walk still ages (and
                    // counts in the age table) exactly the header-marked
                    // survivors that are NOT side-marked, so nothing is aged
                    // twice.
                    if marked && !aged && (!header_marked || side_marked) {
                        // Deferred, anchor-verified age bump — applied with
                        // the forwarding installs below. Header-marked
                        // survivors are excluded: the main sweep still ages
                        // those in place.
                        age_bumps.push(addr);
                    }
                    // `!old_full`: see the loop head — a full old gen stops
                    // promotion, not the walk. `selective_on`: an age-only walk
                    // (gen r4w2/youngmark, see `age_only`) never promotes.
                    if selective_on && marked && aged && !is_pinned && !old_full {
                        match old_gen.alloc(total_size, 8) {
                            Some(dst) => {
                                // gcstress face-1 hunt (no-op unless gated):
                                // validate the SOURCE body before promoting —
                                // catching a corrupt cell here pins the
                                // corruption to BEFORE promotion and reports
                                // the victim's stable young address.
                                if promo_cell_diag {
                                    validate_copy_source_cells(src, header, "promote-src");
                                }
                                {
                                    let w = promo_cell_watch;
                                    let d = dst as usize;
                                    if w != 0 && d <= w && w.wrapping_sub(d) < total_size {
                                        let src_at = src as usize + (w - d);
                                        // SAFETY: src object spans total_size bytes.
                                        let pair =
                                            unsafe { std::ptr::read(src_at as *const [u64; 2]) };
                                        cratonvm_gc_core::heap::cell_watch_check(
                                            d,
                                            total_size,
                                            "selective-promote-evac",
                                            &format!(
                                                "src=0x{:x} src[watch]=0x{:016x},0x{:016x}",
                                                src as usize, pair[0], pair[1]
                                            ),
                                        );
                                    }
                                }
                                if promo_obj_watch {
                                    note_obj_watch_copy(header, src, dst, total_size, true);
                                }
                                // gen r5w2/roots6: `CRATONVM_DBG=root-write-audit`
                                // checks the destination is old gen (aborts
                                // at this writer otherwise).
                                if promo_audit {
                                    cratonvm_gc_core::root_write_audit::heap_write_in_bounds(
                                        cratonvm_gc_core::root_write_audit::HeapWriteKind::PromotionCopy,
                                        dst as usize,
                                        total_size,
                                        &[old_gen.extent()],
                                    );
                                }
                                // SAFETY: src/dst are valid, non-overlapping, total_size bytes.
                                unsafe { std::ptr::copy_nonoverlapping(src, dst, total_size) };
                                // Replicate the atomic mark_word through atomic ops
                                // (the bulk copy of an AtomicU64 is otherwise UB).
                                //
                                // gcf f1/y (item 4 of the pin10 round-three
                                // proposal): `GC_FLAG_OLD_GEN` is folded into
                                // that one store (`mark_with_gc_flags`, the word
                                // `add_gc_flags` leaves) instead of a locked
                                // `fetch_or` on a header only this thread can
                                // see, and `GC_FLAG_MARKED` is cleared (a
                                // `fetch_and`) only when the source carried it.
                                // Same final word.
                                // SAFETY: both headers are fully written.
                                let src_mark = unsafe {
                                    (*(src as *const ObjectHeader))
                                        .mark_word
                                        .load(Ordering::Relaxed)
                                };
                                // SAFETY: dst is a freshly written object header.
                                let dhdr = unsafe { &mut *(dst as *mut ObjectHeader) };
                                dhdr.mark_word.store(
                                    ObjectHeader::mark_with_gc_flags(src_mark, GC_FLAG_OLD_GEN),
                                    Ordering::Relaxed,
                                );
                                if ObjectHeader::gc_flags_of(src_mark) & GC_FLAG_MARKED != 0 {
                                    dhdr.clear_gc_flags(GC_FLAG_MARKED);
                                }
                                // Forwarding-pointer install into the young
                                // source is deferred until the walk validates.
                                fwd_installs.push((addr, dst));
                                evac_map.insert(addr, dst as usize);
                                evacuated.push(dst);
                                // Promotion accounting (mirrors the moving
                                // collector's `forward_object`): powers the
                                // GC-overhead productivity metric — a
                                // promotion-only cycle conserves live bytes
                                // but very much did useful work (it drained
                                // young), and must not count as "thrashing".
                                if batch {
                                    tally.promoted_bytes += total_size as u64;
                                    tally.promoted_objects += 1;
                                    tally.evacuated += 1;
                                } else {
                                    self.stats
                                        .bytes_promoted
                                        .fetch_add(total_size as u64, Ordering::Relaxed);
                                    self.stats.objects_promoted.fetch_add(1, Ordering::Relaxed);
                                    SP_CENSUS.evacuated.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            None => {
                                if batch {
                                    tally.old_full += 1;
                                } else {
                                    SP_CENSUS.old_full.fetch_add(1, Ordering::Relaxed);
                                }
                                old_full = true
                            }
                        }
                    }
                    cursor += total_size;
                }
                // gcf f3/q (patch 2): publish the walk's census once. Still
                // inside the pause, before the forwarding installs and before
                // anything reads `objects_promoted` / `bytes_promoted`.
                if batch {
                    tally.publish(&self.stats);
                    self.young_premium_census[YP_BATCH_WALKS].fetch_add(1, Ordering::Relaxed);
                }
                if size_memo.hits != 0 {
                    self.young_premium_census[YP_MEMO_HITS]
                        .fetch_add(size_memo.hits, Ordering::Relaxed);
                }
            }
            if phase_diag {
                sp_split_marks[0] = Some(std::time::Instant::now());
            }
            // gen r5w2/roots6 (`CRATONVM_DBG=root-write-audit`): from here to
            // the end of fixup pass 3c nothing inserts into or removes from
            // `evac_map`, so its fingerprint must hold at every checkpoint
            // below. The netty SIGSEGV
            // (`docs/feature-designs/generative-gc-architecture.md`)
            // read its table pointer as `0x1_0000_0000` inside
            // `rewrite_stretch`; a moved fingerprint names the window the
            // corrupting store fell in, and whether it hit the map's handle
            // (this frame) or its tables (the allocator heap). Off: `None`.
            let audit = cratonvm_gc_core::root_write_audit::enabled();
            let mut evac_watch = audit
                .then(|| cratonvm_gc_core::root_write_audit::MapWatch::arm(&evac_map, "evacuation walk end"));
            // The young headers the deferred installs below may write.
            let young_extent = [(from_base, from_end)];
            // Install forwarding pointers for the surviving candidates —
            // every entry left in `fwd_installs` was collected on a stretch
            // whose strides were verified against the next anchor (or a
            // clean end-of-walk); suspect stretches were unwound above.
            for &(src_addr, dst) in &fwd_installs {
                if audit {
                    cratonvm_gc_core::root_write_audit::heap_write_in_bounds(
                        cratonvm_gc_core::root_write_audit::HeapWriteKind::SweepHeaderWrite,
                        src_addr,
                        HEADER_SIZE,
                        &young_extent,
                    );
                }
                // SAFETY: `src_addr` is a live young object header on an
                // anchor-verified stretch; writing its forwarding_ptr field
                // is the install the copy loop deferred.
                unsafe {
                    (*(src_addr as *const ObjectHeader)).set_forwarding_address(dst);
                }
            }
            if let Some(w) = evac_watch.as_mut() {
                w.check(&evac_map, "forwarding installs");
            }
            // Age the not-yet-tenurable survivors — same anchor-verified
            // deferral contract as the forwarding installs above. This is the
            // aging the marker's unconditional side-marking removed (it never
            // header-writes); without it no survivor can ever reach
            // PROMOTION_AGE and this pass stays permanently inert.
            // gen r4w6 (orchestrator): the same survivors fill the tenuring
            // age table, at their NEW age, so adaptive tenuring and
            // `-XX:+PrintTenuringDistribution` see the non-moving cycles too.
            // A JIT-warm program's young cycles are all non-moving (term 4)
            // unless the pinned copy is on; before this they never adapted
            // the threshold and printed nothing.
            // gcf f2/y: the table is `sweep_ages`, declared before this
            // block; `adapt` runs after the main sweep walk (see there).
            sweep_ages_filled = true;
            for &src_addr in &age_bumps {
                if audit {
                    cratonvm_gc_core::root_write_audit::heap_write_in_bounds(
                        cratonvm_gc_core::root_write_audit::HeapWriteKind::SweepHeaderWrite,
                        src_addr,
                        HEADER_SIZE,
                        &young_extent,
                    );
                }
                // SAFETY: `src_addr` is a live young object header on an
                // anchor-verified stretch (identical contract to the
                // forwarding-pointer install above); bumping its age byte is
                // the write the walk deferred.
                //
                // gcf f1/y (item 4 of the pin10 round-three proposal): one
                // load and one plain store of the folded word, not
                // `set_gc_age`'s compare-exchange loop -- the pause is stopped
                // the world and this loop is the only writer of these headers
                // (the moving path's `forward_object_impl` relies on the same
                // fact since gen r5w6/pin10). `mark_with_gc_age` saturates at
                // `MAX_GC_AGE` exactly as `set_gc_age` does, so the word is
                // bit-identical.
                unsafe {
                    let h = &mut *(src_addr as *mut ObjectHeader);
                    let m = h.mark_word.load(Ordering::Relaxed);
                    // gci i1/yg: by the cycle's age step (1 unless an age
                    // source is armed; 0 rewrites the same word).
                    let new_age = ObjectHeader::gc_age_of(m).saturating_add(age_step);
                    h.mark_word.store(
                        ObjectHeader::mark_with_gc_age(m, new_age),
                        Ordering::Relaxed,
                    );
                    if sweep_age_census {
                        let size = gen_object_total_size(h);
                        tenuring::add_to_age_table(&mut sweep_ages, new_age, size);
                    }
                }
            }
            if let Some(w) = evac_watch.as_mut() {
                w.check(&evac_map, "age bumps and tenuring");
            }
            if phase_diag {
                sp_split_marks[1] = Some(std::time::Instant::now());
            }

            // DBG (CRATONVM_SP_STATS): per-GC pin/evac counts, printed
            // unconditionally (even when nothing evacuated) so we can confirm
            // whether selective promotion is actually evacuating at a given
            // heap size, or whether the only active effect is the free-block
            // coalescing below.
            if gc_flags().sp_stats {
                eprintln!(
                    "[sp-stats] gc: pinned={} evac={} aged={}",
                    pinned.len(),
                    evac_map.len(),
                    age_bumps.len(),
                );
            }

            // DBG (CRATONVM_SP_TRACE): directly test the wrong-address/aliasing
            // hypothesis for the bintrees18 bug. Two young objects copied to
            // OVERLAPPING old-gen destinations (an old_gen.alloc collision) would
            // corrupt one copy's field data → a child reference reads the wrong
            // subtree → inflated check() count. Detect overlapping dst ranges and
            // duplicate dst values among this GC's evacuations.
            if gc_flags().sp_trace && !evacuated.is_empty() {
                let mut ranges: Vec<(usize, usize)> = evacuated
                    .iter()
                    .map(|&d| {
                        // SAFETY: d is a live old-gen object just written.
                        let sz = gen_object_total_size(unsafe { &*(d as *const ObjectHeader) });
                        (d as usize, sz)
                    })
                    .collect();
                ranges.sort_by_key(|&(d, _)| d);
                let mut overlaps = 0usize;
                for w in ranges.windows(2) {
                    let (d0, s0) = w[0];
                    let (d1, _) = w[1];
                    if d0 + s0 > d1 {
                        overlaps += 1;
                        if overlaps <= 8 {
                            eprintln!(
                                "[sp-trace] DST OVERLAP #{}: {:#x}+{} > {:#x}",
                                overlaps, d0, s0, d1
                            );
                        }
                    }
                }
                let uniq: FxHashSet<usize> = evacuated.iter().map(|&d| d as usize).collect();
                let dups = evacuated.len() - uniq.len();
                eprintln!(
                    "[sp-trace] evac={} dst_overlaps={} dst_dups={} dst_min={:#x} dst_max={:#x} oldused={}M",
                    evacuated.len(),
                    overlaps,
                    dups,
                    ranges.first().map(|r| r.0).unwrap_or(0),
                    ranges.last().map(|r| r.0).unwrap_or(0),
                    old_gen.used() / 1_048_576,
                );
            }

            // (3) Fix up every reference to an evacuated object (follow the
            // forwarding pointers installed above), then dirty cards for the new
            // old→young edges the evacuated copies introduce.
            if !evac_map.is_empty() {
                let fwd_of = |target: usize| -> Option<usize> {
                    if !is_y(target) {
                        return None;
                    }
                    // SAFETY: `is_y` confirmed an 8-aligned young address.
                    let h = unsafe { &*(target as *const ObjectHeader) };
                    if h.is_forwarded() {
                        // DoHead walk-desync hardening (2026-07-02): only
                        // follow forwarding pointers that actually land in
                        // old gen — selective promotion never forwards
                        // anywhere else. A corrupt/phantom forwarding field
                        // (e.g. a span retained by the main sweep's
                        // bad-forward check in an earlier cycle) must not
                        // rewrite live references to a garbage target.
                        let fwd = h.forwarding_address();
                        if old_gen.contains(fwd) {
                            Some(fwd as usize)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                };

                // (3a) References inside surviving (pinned / non-evacuated) young
                // objects → rewrite to the evacuated copies in old gen.
                //
                // DoHead walk-desync hardening (2026-07-02): this pass runs
                // only after the evacuation walk validated the whole grid, so
                // anomalies here should be impossible — but a wedge in THIS
                // walk (the old exact-match skip) would leave every survivor
                // past the wedge un-fixed-up while the main sweep zeroes the
                // evacuated sources: mass dangling references. Use the same
                // robust traversal; on an (unexpected) anomaly, re-anchor at
                // the next free block and keep fixing up rather than break.
                //
                // gcf f3/q (r15w5/gcpage patch 3, `CRATONVM_GEN_SWEEP_3A_REPLAY`,
                // default ON): this walk fixes up the grid objects `g` with
                // `(MARKED(g) || side(g) || late(g)) && !forwarded(g)`. Walk (2)
                // parsed the same grid (same start, the same free-block
                // snapshot, the same `used` and the same `GridRules`) BEFORE
                // the installs touched any header. When its walk was clean, no
                // late pin exists, no grid object was header-marked (and the
                // installs and age bumps between the walks never set
                // `GC_FLAG_MARKED` on a young header), and it counted exactly
                // `side_sorted.len()` side-marked grid objects, then every
                // `side_sorted` entry IS a grid object start (the bitmap is
                // exact, one bit per 8-byte address, and a clean walk visits
                // each grid object once, ascending), and the set above is
                // `{a in side_sorted : !forwarded(a)}` -- visited here in the
                // same ascending order, with the same per-object action. Any
                // anomaly, late pin, header mark or count mismatch re-walks.
                let replay_3a = grid_clean
                    && grid_header_marked == 0
                    && late_pins.is_empty()
                    && grid_side_marked == side_sorted.len();
                if replay_3a {
                    self.young_premium_census[YP_3A_REPLAYS].fetch_add(1, Ordering::Relaxed);
                    if let Some(w) = evac_watch.as_mut() {
                        w.check(&evac_map, "fixup 3a start");
                    }
                    for &addr in &side_sorted {
                        // SAFETY: `replay_3a` proved every entry is an object
                        // start walk (2) parsed on a clean grid of mapped
                        // from-space, and nothing has freed or zeroed young
                        // since (the sweep runs after this block).
                        let header = unsafe { &*(addr as *const ObjectHeader) };
                        if !header.is_forwarded() {
                            fixup_object_fields(addr as *mut u8, header, &fwd_of, &is_y, None);
                        }
                    }
                } else {
                    self.young_premium_census[YP_3A_WALKS].fetch_add(1, Ordering::Relaxed);
                    YOUNG_WALK_ENTRIES[1].fetch_add(1, Ordering::Relaxed);
                    // gcf f3/q (patch 1): one size memo for this walk.
                    let mut size_memo = GridSizeMemo::new(size_memo_on);
                    // PERF: reuse the once-computed sorted free-block snapshot.
                    let mut free_iter = sweep_free_blocks.iter().peekable();
                    let used = sweep_used;
                    let mut cursor = 0usize;
                    // xt-hardening (2026-07-03): lockstep over side-marked
                    // survivors (see the fixup condition below).
                    let mut side_iter_3a = side_sorted.iter().peekable();
                    // Conservative fallback for a stretch this walk cannot
                    // parse (the evacuation walk resynced over the same
                    // stretch, so survivors inside it were not evacuated —
                    // but they may still REFERENCE evacuated objects, and an
                    // un-rewritten reference dangles once the main sweep
                    // zeroes the forwarded sources). Exact-match rewrite of
                    // any aligned slot equal to an evacuated source address.
                    //
                    // gen r4/sweep (2026-09-23): at BOTH reference widths. This
                    // used to read aligned 8-byte words only — the exact
                    // compressed-oops "hole 2" `for_each_conservative_ref_slot`
                    // documents and that `fixup_young_old_refs` /
                    // `mark_young_to_old_refs` were already fixed for: with
                    // narrow oops on, an 8-byte read spans two 4-byte reference
                    // slots, never equals an evacuated address, and every
                    // narrow reference in the stretch was left pointing at a
                    // source the sweep is about to zero. Same shared visitor,
                    // same re-encode-at-read-width rule and the same
                    // "never write a 0 re-encoding" guard as
                    // `fixup_young_old_refs`.
                    let narrow = active_narrow_geometry();
                    if let Some(w) = evac_watch.as_mut() {
                        w.check(&evac_map, "fixup 3a start");
                    }
                    let mut rewrite_stretch = |lo: usize, hi: usize| {
                        // gen r5w2/roots6 (`CRATONVM_DBG=root-write-audit`):
                        // the map must still be the one the walk built when
                        // this stretch starts (the netty SIGSEGV read it
                        // corrupt HERE), and the stretch must be from-space.
                        if let Some(w) = evac_watch.as_mut() {
                            w.check(&evac_map, "rewrite_stretch entry");
                            cratonvm_gc_core::root_write_audit::heap_write_in_bounds(
                                cratonvm_gc_core::root_write_audit::HeapWriteKind::SweepStretchRewrite,
                                from_base + lo,
                                hi.saturating_sub(lo),
                                &young_extent,
                            );
                        }
                        // SAFETY: `[from_base+lo, from_base+hi)` is mapped
                        // from-space memory below `sweep_used`, and each `slot`
                        // handed back is an aligned location this scan just
                        // read at the width it reports.
                        unsafe {
                            for_each_conservative_ref_slot(
                                from_base,
                                lo,
                                hi,
                                narrow,
                                |slot, width, value| {
                                    let Some(&dst) = evac_map.get(&value) else {
                                        return;
                                    };
                                    if width == 8 {
                                        *(slot as *mut u64) = dst as u64;
                                    } else {
                                        let re = cratonvm_types::narrow_oop::encode(dst as u64);
                                        if re != 0 {
                                            *(slot as *mut u32) = re;
                                        }
                                    }
                                },
                            );
                        }
                        if let Some(w) = evac_watch.as_mut() {
                            w.check(&evac_map, "rewrite_stretch exit");
                        }
                    };
                    // gen r4/sweep (2026-09-23): lockstep over `late_pins`, as
                    // the main sweep walk does — see the fixup condition below.
                    let mut late_iter_3a = late_pins.iter().peekable();
                    while cursor < used {
                        if skip_free_blocks(&mut cursor, &mut free_iter).0 {
                            continue;
                        }
                        let obj = (from_base + cursor) as *mut u8;
                        // gen r5w1/young5: the grid rules are
                        // `probe_grid_at`'s (the same rules, and the same
                        // zero-run arm, as the evacuation pre-pass above, so
                        // the two walks now see one grid). A run of empty
                        // objects holds no reference slots, so there is
                        // nothing in it for this pass to rewrite; every
                        // anomaly hands its stretch to `rewrite_stretch`'s
                        // conservative rewrite.
                        let next_free = free_iter.peek().map(|&&(off, _)| off);
                        let rules_3a = GridRules {
                            base: from_base,
                            used,
                            zero_run_hi: used,
                            side_sorted: &side_sorted,
                            zero_runs: ZeroRunRule::StepOver {
                                unresolved: None,
                                count_empty_runs: true,
                            },
                            bad_gap_filler_is_anomaly: false,
                            phantom: false,
                        };
                        let mut no_phantom_probe = 0usize;
                        // `cursor < used`; from-space mapped.
                        let total_size = match probe_grid_at_sized(
                            cursor,
                            next_free,
                            &rules_3a,
                            &mut no_phantom_probe,
                            &mut size_memo,
                        ) {
                            GridProbe::Object { size } | GridProbe::Phantom { size, .. } => size,
                            GridProbe::Gap { len } => {
                                cursor += len;
                                continue;
                            }
                            GridProbe::EmptyRun { resume } => {
                                cursor = resume;
                                continue;
                            }
                            GridProbe::ZeroSpan { .. }
                            | GridProbe::BadSize { .. }
                            | GridProbe::BadGapFiller => {
                                let stretch_lo = cursor;
                                let resynced =
                                    resync_to_next_free_block(&mut cursor, &mut free_iter);
                                let stretch_hi = if resynced { cursor } else { used };
                                rewrite_stretch(stretch_lo, stretch_hi);
                                if resynced {
                                    continue;
                                }
                                break;
                            }
                            // The hole-crossing rule this walk lacked
                            // (row "fixup 3a" of the drift page): a header
                            // whose extent crosses the next free block is
                            // not an object, so it is not parsed and fixed
                            // up across the hole. Its stretch up to the
                            // hole gets the same conservative rewrite as
                            // every other anomaly, and the walk resumes at
                            // the hole, where the free-block skip takes
                            // over (the evacuation pre-pass re-anchors at
                            // the same offset). Unreachable on a healthy
                            // arena.
                            GridProbe::CrossesFreeBlock { free_off, .. } => {
                                rewrite_stretch(cursor, free_off);
                                cursor = free_off;
                                continue;
                            }
                        };
                        // SAFETY: `probe_grid_at` parsed a plausible header at
                        // this in-bounds, on-grid offset of mapped from-space.
                        let header = unsafe { &*(obj as *const ObjectHeader) };
                        // xt-hardening (2026-07-03): side-marked survivors
                        // (kept alive without a header write — see
                        // `side_marks`) also need their references to
                        // evacuated objects rewritten; they are unmarked by
                        // construction so the MARKED gate alone would skip
                        // them (dangling refs once the main sweep frees the
                        // forwarded sources).
                        let side_hit = {
                            let abs = from_base + cursor;
                            while let Some(&&a) = side_iter_3a.peek() {
                                if a < abs {
                                    side_iter_3a.next();
                                } else {
                                    break;
                                }
                            }
                            side_iter_3a.peek().is_some_and(|&&a| a == abs)
                        };
                        // gen r4/sweep (2026-09-23): an object the main sweep
                        // will RETAIN because a late pin (an unplaceable
                        // conservative candidate) lies inside it is a survivor
                        // too, so its references to evacuated objects must be
                        // rewritten like any other survivor's — otherwise the
                        // sweep keeps the object and zeroes what it points at.
                        // Same lockstep rule as the sweep's `late_pinned`.
                        let late_hit = !late_pins.is_empty() && {
                            let abs = from_base + cursor;
                            while late_iter_3a.peek().is_some_and(|&&a| a < abs) {
                                late_iter_3a.next();
                            }
                            late_iter_3a.peek().is_some_and(|&&a| a < abs + total_size)
                        };
                        if (header.gc_flags() & GC_FLAG_MARKED != 0 || side_hit || late_hit)
                            && !header.is_forwarded()
                        {
                            fixup_object_fields(obj, header, &fwd_of, &is_y, None);
                        }
                        cursor += total_size;
                    }
                    if size_memo.hits != 0 {
                        self.young_premium_census[YP_MEMO_HITS]
                            .fetch_add(size_memo.hits, Ordering::Relaxed);
                    }
                }
                if let Some(w) = evac_watch.as_mut() {
                    w.check(&evac_map, "fixup 3a end");
                }
                if phase_diag {
                    sp_split_marks[2] = Some(std::time::Instant::now());
                }

                // (3b) References inside the evacuated (now old-gen) copies. Any
                // field still pointing at a (pinned) young object is a new
                // old→young edge whose card must be dirtied.
                let mut new_old_young: Vec<usize> = Vec::new();
                // gcd d5/s (cards3 item 3, producer (g)): a promoted reference
                // array marks its young-staying ELEMENTS' cards on an
                // element-precise table, as the moving path's promoted scans
                // do since gcd d2/h (`HolderCardDefer`); every other holder,
                // and every holder on a header-grained table
                // (`CRATONVM_GC_PRECISE_ARRAY_CARDS=0`), still marks its
                // header. Same edge set: `scan_dirty_cards_inner` reads the
                // elements of a dirty element card of a header-clean array.
                let element_card_base = self
                    .card_table
                    .precise_ref_array_marks()
                    .then(|| self.card_table.base_addr());
                let mut element_pairs: Vec<(usize, usize)> = Vec::new();
                for &dst in &evacuated {
                    // SAFETY: dst is a live old-gen object just written.
                    let dhdr = unsafe { &*(dst as *const ObjectHeader) };
                    let mut pts = false;
                    fixup_object_fields(dst, dhdr, &fwd_of, &is_y, Some(&mut pts));
                    if pts {
                        push_promoted_holder_cards(
                            dst,
                            dhdr,
                            element_card_base,
                            &is_y,
                            &mut element_pairs,
                            &mut new_old_young,
                        );
                    }
                }
                if let Some(w) = evac_watch.as_mut() {
                    w.check(&evac_map, "fixup 3b end");
                }
                if phase_diag {
                    sp_split_marks[3] = Some(std::time::Instant::now());
                }

                // (3c) Existing old→young edges (dirty-card seeds): rewrite any
                // whose young target was evacuated (now old→old). NOTE: a full
                // old-gen walk here did NOT fix the bintrees18 wrong-checksum, so
                // the missed reference is not a clean-card old→young edge — the
                // residual corruption is a register-invisible reference to an
                // evacuated object (see memory reference_osr_main_corruptor).
                //
                // gcg g4/yg: the opt-in listed-slot rewrite (gcf f3/q,
                // `CRATONVM_GEN_SWEEP_3C_LISTED_SLOTS`) is deleted -- triage
                // tg1 measured no gain, and the whole-holder rewrite also
                // covers a young reference on a clean card.
                {
                    let mut seen: FxHashSet<usize> = FxHashSet::default();
                    // gch h1/yy: the card scan lists a holder's pairs together
                    // (it reads one holder's dirty windows before the next), so
                    // a repeat of the PREVIOUS pair's holder is answered without
                    // the hash probe; it is in `seen` already, so the set of
                    // holders rewritten, and their order, are unchanged. On
                    // `GenR4W4EvacThroughputProbe` that is ~250k probes of one
                    // ring array per promoting cycle down to one.
                    let mut prev_holder = 0usize;
                    for &(old_obj, _, _) in &extra_roots {
                        let oaddr = old_obj.as_ptr() as usize;
                        if oaddr == prev_holder {
                            continue;
                        }
                        prev_holder = oaddr;
                        if !seen.insert(oaddr) {
                            continue;
                        }
                        // SAFETY: dirty-card scan yielded a live old-gen object.
                        let ohdr = unsafe { &*(oaddr as *const ObjectHeader) };
                        if phase_diag {
                            sp_split_3c_holders += 1;
                            sp_split_3c_bytes += gen_object_total_size(ohdr);
                        }
                        fixup_object_fields(oaddr as *mut u8, ohdr, &fwd_of, &is_y, None);
                    }
                }
                if let Some(w) = evac_watch.as_mut() {
                    w.check(&evac_map, "fixup 3c end");
                }
                if phase_diag {
                    sp_split_marks[4] = Some(std::time::Instant::now());
                }

                if !new_old_young.is_empty() {
                    self.card_table.mark_dirty_bulk(&new_old_young);
                }

                // DBG (CRATONVM_SP_VERIFY): after ALL fixup, does any surviving
                // object still reference a forwarded (evacuated) young object? A
                // nonzero count is a MISSED fixup (dangling ref into a reclaimed
                // slot). Splits old-gen vs surviving-young so we know which path
                // (3a/3b/3c) has the gap. Zero on both ⇒ the corruption is a
                // WRONG-address rewrite, not a missed one.
                if gc_flags().sp_verify {
                    // Incoming-reference count to each evacuated destination. In a
                    // forest of trees every node has exactly ONE parent, so any
                    // evacuated object with >=2 incoming heap refs is ALIASING — a
                    // child reference rewritten to a valid-but-wrong old object
                    // (passes the missed-fixup check, but inflates check()).
                    let dsts: FxHashSet<usize> = evac_map.values().copied().collect();
                    let mut incoming: FxHashMap<usize, u32> = FxHashMap::default();
                    let mut missed_old = 0usize;
                    let mut bump = |t: usize| {
                        if dsts.contains(&t) {
                            *incoming.entry(t).or_insert(0) += 1;
                        }
                    };
                    for (oaddr, _sz) in old_gen.walk_objects() {
                        // SAFETY: `walk_objects` yields the start of each live old-gen
                        // object, so `oaddr` targets a valid, initialized `ObjectHeader`.
                        let h = unsafe { &*(oaddr as *const ObjectHeader) };
                        missed_old += forwarded_ref_count(oaddr, h, &is_y);
                        for_each_ref(oaddr, h, &mut bump);
                    }
                    let mut missed_young = 0usize;
                    // gen r4w2/youngmark: the shared `YoungGridCursor` over the
                    // once-computed sorted free-block snapshot. The hand-rolled
                    // walk lacked the GAP-filler and empty-run rules and
                    // `break`ed at the first TLAB tail sentinel, so `young=0`
                    // could mean "checked everything" or "checked a prefix".
                    // It also tested only the header mark, which nothing on
                    // this path writes since Family-A: survivors are
                    // side-marked, so `young=` was structurally 0.
                    let mut grid = YoungGridCursor::new(
                        from_base,
                        sweep_used,
                        sweep_free_blocks,
                        &side_sorted,
                    );
                    for step in grid.by_ref() {
                        let GridStep::Object { off: c, .. } = step else {
                            continue;
                        };
                        let o = (from_base + c) as *mut u8;
                        // SAFETY: the cursor yields only parsed object starts
                        // inside the mapped from-space.
                        let h = unsafe { &*(o as *const ObjectHeader) };
                        let survivor =
                            h.gc_flags() & GC_FLAG_MARKED != 0 || side_bits.contains(o as usize);
                        if survivor && !h.is_forwarded() {
                            missed_young += forwarded_ref_count(o, h, &is_y);
                            for_each_ref(o, h, &mut bump);
                        }
                    }
                    let aliased = incoming.values().filter(|&&n| n >= 2).count();
                    let max_in = incoming.values().copied().max().unwrap_or(0);
                    eprintln!(
                        "[sp-verify] evac={} MISSED fwd refs old={} young={} | evac-objs with >=2 incoming (ALIASING)={} max_incoming={} | young walk anomalies={} truncated={}",
                        evac_map.len(),
                        missed_old,
                        missed_young,
                        aliased,
                        max_in,
                        grid.anomalies,
                        grid.truncated,
                    );
                }
            }
            // gch h1/yy: the split promised at `sp_split_t0`.
            if let Some(t0) = sp_split_t0 {
                let mut prev = t0;
                let mut part_us = [0u128; 5];
                for (i, mark) in sp_split_marks.iter().enumerate() {
                    if let Some(t) = *mark {
                        part_us[i] = t.saturating_duration_since(prev).as_micros();
                        prev = t;
                    }
                }
                eprintln!(
                    "[gcphase] selective-split: walk={}us installs={}us fix3a={}us fix3b={}us \
                     fix3c={}us par_walk={} evac={} age_bumps={} card_pairs={} holders_3c={} \
                     holder_bytes_3c={}",
                    part_us[0],
                    part_us[1],
                    part_us[2],
                    part_us[3],
                    part_us[4],
                    u8::from(sp_split_par_walk),
                    evac_map.len(),
                    age_bumps.len(),
                    extra_roots.len(),
                    sp_split_3c_holders,
                    sp_split_3c_bytes,
                );
            }
        }

        report_phase("selective-promotion");

        // ----- Diagnostic: inbound-edge search (CRATONVM_DBG_SWEEP_EDGES) --
        //
        // Marking is complete; nothing has been zeroed yet. This answers the
        // decisive question for the bintrees18 / AllocLoop non-moving-sweep
        // corruption: every object the sweep is about to zero is UNMARKED —
        // but is any of them actually still REACHABLE? We look for an inbound
        // edge from something that survives, classified by source:
        //
        //   (1) a passed-in ROOT/finalizer points straight at an unmarked
        //       young object  => `mark_young`'s plausibility filter rejected a
        //       live root (header looked corrupt at mark time).
        //   (2) a young SURVIVOR's reference field points at an unmarked young
        //       object  => intra-young BFS desync (survivor marked, but its
        //       field-scan didn't propagate: wrong num_slots at mark time, a
        //       post-mark SATB write, or a filter false-reject of the target).
        //   (3) an OLD-GEN object's reference field points at an unmarked young
        //       object  => card-table / write-barrier miss (the old->young edge
        //       was never seeded because its card wasn't dirty).
        //
        // ANY of (1)/(2)/(3) firing proves the swept node is reachable => the
        // bug is in marking/seeding (case "b"), not a register/native root gap.
        // If NONE fire across the whole run yet corruption still occurs, the
        // only live reference is outside roots+cards+finalizers+heap — i.e. a
        // register/native-stack root the sweep cannot see (case "a"), or a
        // sweep-walk/sizing defect. Routine dead garbage has no inbound edge,
        // so a clean (no-edge) sweep is NORMAL — only edge hits are bugs.
        if gc_flags().dbg_sweep_edges {
            use std::collections::HashSet;
            // Local young-membership test (the `in_young` closure above is
            // borrowed by `mark_young` for the rest of the fn; use a fresh one).
            let is_young = |a: usize| -> bool { a >= from_base && a < from_end && (a & 0x7) == 0 };
            let is_unmarked_young = |a: usize| -> bool {
                if !is_young(a) {
                    return false;
                }
                // Family-A (2026-07-03) made EVERY mark go through the SIDE
                // set (`side_marks`) — the header `GC_FLAG_MARKED` bit is no
                // longer written by this sweep's mark phase at all. Checking
                // only the header bit made every live object look "unmarked"
                // to this diagnostic (SUMMARY always reported marked=0 and
                // every root registered as a bogus (1)/(2)/(3) edge hit),
                // drowning the real signal. A side-marked object is live and
                // will NOT be swept; only side-UNmarked objects matter here.
                if side_bits.contains(a) {
                    return false;
                }
                // SAFETY: `is_young` confirmed an 8-aligned addr inside live
                // from-space; reading its header is valid.
                let h = unsafe { &*(a as *const ObjectHeader) };
                h.gc_flags() & GC_FLAG_MARKED == 0
            };

            let root_set: HashSet<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();

            // H2-CID0: a CONSERVATIVE root is an aligned stack/register word and
            // may be object-INTERIOR. `mark_young` marks the containing object's
            // BASE, not the candidate, so testing the raw candidate reports every
            // interior root as an unmarked live root — 9-16 bogus (1) hits per
            // cycle on a perfectly healthy H2 run, which is enough noise to hide
            // a real one. Resolve through the same oracle the marker used before
            // asking the question.
            let resolved_base = |addr: usize| -> usize {
                young_object_ranges
                    .partition_point(|(start, _)| *start <= addr)
                    .checked_sub(1)
                    .and_then(|i| young_object_ranges.get(i))
                    .filter(|&&(_, end)| addr < end)
                    .map_or(addr, |&(base, _)| base)
            };

            // (1) roots / finalizers that landed on an unmarked young object.
            let mut root_to_unmarked = 0usize;
            for &raw_addr in root_set.iter() {
                let addr = resolved_base(raw_addr);
                if is_unmarked_young(addr) {
                    root_to_unmarked += 1;
                    if root_to_unmarked <= 8 {
                        // SAFETY: `addr` came from the root set (a live `ObjectRef`),
                        // so it points at a valid, initialized `ObjectHeader`.
                        let h = unsafe { &*(addr as *const ObjectHeader) };
                        tracing::warn!(
                            "[sweep-edges] (1) ROOT @{:#x} -> UNMARKED young obj \
                             (class_id={} kind=0x{:02x} num_slots={} array_len={}) — \
                             mark filter rejected a live root?",
                            addr,
                            h.class_id.as_u32(),
                            ObjectHeader::kind_tag(h.mark_word.load(Ordering::Relaxed)),
                            h.num_slots(),
                            h.array_length(),
                        );
                    }
                }
            }
            for &raw_addr in finalizer_addrs.iter() {
                if is_unmarked_young(resolved_base(raw_addr)) {
                    root_to_unmarked += 1;
                }
            }

            // Helper: scan one object's reference fields for unmarked-young
            // targets, invoking `report(slot_idx, target_addr)` for each.
            let scan_refs = |obj_ptr: *mut u8, report: &mut dyn FnMut(usize, usize)| {
                // SAFETY: caller guarantees obj_ptr is a valid object header.
                let h = unsafe { &*(obj_ptr as *const ObjectHeader) };
                // Arrays + compact objects: 8-byte pointer slots; legacy objects:
                // 16-byte Value cells. `slot_id` is the element index / byte
                // offset / field index (diagnostic-only here).
                // SAFETY: `obj_ptr`/`h` are a valid live object.
                unsafe {
                    for_each_ref_slot(obj_ptr, h, |raw, slot_id| {
                        let ta = raw as usize;
                        if is_unmarked_young(ta) {
                            report(slot_id, ta);
                        }
                    });
                }
            };

            // (2) young survivors referencing an unmarked young object.
            let mut survivor_to_unmarked = 0usize;
            let mut unmarked_total = 0usize;
            let mut marked_total = 0usize;
            // `(anomalies, truncated)` of the young walk below — without it,
            // `marked=` + `unmarked=` is a count over an unknown share of the
            // arena (gen r4w2/youngmark).
            let walk_anomalies: (usize, bool);
            {
                // H2-CID0: `merge_skips`, not the bare free list. A reserved
                // TLAB tail of a forcibly-stopped in-JIT peer is unallocated
                // (all-zero) memory that the real sweep skips; walking it here
                // produced "arena already corrupt before this sweep" on a
                // healthy run and truncated the rest of the (2) scan.
                let existing_free_dbg = merge_skips(young_from.free_blocks_sorted());
                let used_dbg = young_from.used();
                // The object the walk last accepted: `(off, size, class_id,
                // kind, num_slots, array_len)`. A desync is caused by the
                // object BEFORE the one it lands on, so the report needs both.
                let mut prev_walk_object: Option<(usize, usize, usize, usize, usize, usize)> = None;
                // gen r4w2/youngmark: the shared `YoungGridCursor` (GAP fillers,
                // empty-object runs, hole crossings) instead of a walk that
                // lacked all three and stopped at the first TLAB tail sentinel.
                // An anomaly is still reported with its full context, and the
                // walk now CONTINUES at the next anchor instead of ending.
                let mut grid =
                    YoungGridCursor::new(from_base, used_dbg, &existing_free_dbg, &side_sorted);
                for step in grid.by_ref() {
                    let (c, tot, anomaly) = match step {
                        GridStep::Object { off, size } => (off, size, None),
                        GridStep::Anomaly { off, reason } => (off, 0, Some(reason)),
                    };
                    let optr = (from_base + c) as *mut u8;
                    // SAFETY: the cursor only yields offsets below `used_dbg`
                    // that lie outside every free/skip block, so this is a
                    // readable header slot in mapped from-space (an anomaly's
                    // header is read only to be printed).
                    let h = unsafe { &*(optr as *const ObjectHeader) };
                    if let Some(reason) = anomaly {
                        let tot = gen_object_total_size(h);
                        // NAME THE CORRUPTOR, not just the offset. This message
                        // used to print `off`, `size` and `used` only, which
                        // says a walk desynced and nothing about WHY -- and the
                        // desync is not a side note: on
                        // `io.netty.util.internal.ObjectCleanerTest` under
                        // `-XX:+UseGenerationalGC` the runs that print it are
                        // exactly the runs that then report
                        // `root=N ... REACHABLE NODE WILL BE SWEPT`, and the
                        // runs that do not print it report `root=0` and pass
                        // (measured 2026-09-05, both faces on the same binary).
                        //
                        // Two things are needed to act on it and neither was
                        // here: the OFFENDING header's own fields, and the
                        // PREVIOUS object -- because a walk desyncs at the
                        // object AFTER the one whose recorded size was wrong,
                        // so the offset in this message is the victim's, not
                        // the culprit's.
                        let hex = |addr: usize, words: usize| -> String {
                            let mut out = String::new();
                            for i in 0..words {
                                let w = addr + i * 8;
                                if w + 8 > from_base + used_dbg {
                                    break;
                                }
                                // SAFETY: `w` is 8-aligned inside the mapped
                                // from-space, bounded by `used_dbg` above.
                                let v = unsafe { *(w as *const u64) };
                                out.push_str(&format!("{v:016x} "));
                            }
                            out
                        };
                        let prev_desc = match prev_walk_object {
                            Some((poff, ptot, pcid, pkind, pslots, plen)) => format!(
                                "prev off={poff} size={ptot} class_id={pcid:#x} kind={pkind} \
                                 num_slots={pslots} array_len={plen} words=[{}]",
                                hex(from_base + poff, 6)
                            ),
                            None => "prev=<none: desynced on the first object>".to_string(),
                        };
                        tracing::warn!(
                            "[sweep-edges] diagnostic walk desynced at off={} ({:?}) \
                             (size={}, used={}) — arena already corrupt before this sweep; \
                             here class_id={:#x} kind={} num_slots={} array_len={} \
                             element_type={:?} gc_flags={:#x} words=[{}]; {}; \
                             walk resumes at the next free-block anchor",
                            c,
                            reason,
                            tot,
                            used_dbg,
                            h.class_id.as_u32(),
                            ObjectHeader::kind_tag(h.mark_word.load(Ordering::Relaxed)),
                            h.num_slots(),
                            h.array_length(),
                            h.element_type(),
                            h.gc_flags(),
                            hex(optr as usize, 6),
                            prev_desc,
                        );
                        continue;
                    }
                    prev_walk_object = Some((
                        c,
                        tot,
                        h.class_id.as_u32() as usize,
                        ObjectHeader::kind_tag(h.mark_word.load(Ordering::Relaxed)) as usize,
                        h.num_slots() as usize,
                        h.array_length() as usize,
                    ));
                    // Family-A: side-marked objects are live survivors even
                    // though their header GC_FLAG_MARKED bit is never written
                    // (see `is_unmarked_young` above).
                    if h.gc_flags() & GC_FLAG_MARKED != 0 || side_bits.contains(optr as usize) {
                        marked_total += 1;
                        let cid = h.class_id.as_u32();
                        let oaddr = optr as usize;
                        scan_refs(optr, &mut |si, ta| {
                            survivor_to_unmarked += 1;
                            if survivor_to_unmarked <= 16 {
                                // SAFETY: `ta` is a non-null young heap address read from a
                                // live survivor's ref slot, so it points at a valid header.
                                let th = unsafe { &*(ta as *const ObjectHeader) };
                                tracing::warn!(
                                    "[sweep-edges] (2) SURVIVOR @{:#x} (class_id={}) field[{}] \
                                     -> UNMARKED @{:#x} (class_id={} num_slots={} kind=0x{:02x}) \
                                     in_roots={}",
                                    oaddr,
                                    cid,
                                    si,
                                    ta,
                                    th.class_id.as_u32(),
                                    th.num_slots(),
                                    ObjectHeader::kind_tag(th.mark_word.load(Ordering::Relaxed)),
                                    root_set.contains(&ta),
                                );
                            }
                        });
                    } else {
                        unmarked_total += 1;
                    }
                }
                walk_anomalies = (grid.anomalies, grid.truncated);
            }

            // (3) old-gen objects referencing an unmarked young object
            // (card-table / write-barrier miss). Walks the whole old gen — the
            // dirty-card seed above only covers cards that were marked dirty,
            // so this is exactly the set the seeding could have missed.
            let mut old_to_unmarked = 0usize;
            for (optr, _sz) in old_gen.walk_objects() {
                let oaddr = optr as usize;
                // SAFETY: `walk_objects` yields the start of each live old-gen object, so
                // `optr` targets a valid, initialized `ObjectHeader` whose class_id we read.
                let cid = unsafe { (*(optr as *const ObjectHeader)).class_id.as_u32() };
                scan_refs(optr, &mut |si, ta| {
                    old_to_unmarked += 1;
                    if old_to_unmarked <= 16 {
                        // SAFETY: `ta` is a non-null young heap address read from a live
                        // old-gen object's ref slot, so it points at a valid header.
                        let th = unsafe { &*(ta as *const ObjectHeader) };
                        tracing::warn!(
                            "[sweep-edges] (3) OLD-GEN @{:#x} (class_id={}) field[{}] \
                             -> UNMARKED young @{:#x} (class_id={} num_slots={}) — \
                             card/write-barrier MISS",
                            oaddr,
                            cid,
                            si,
                            ta,
                            th.class_id.as_u32(),
                            th.num_slots(),
                        );
                    }
                });
            }

            let verdict = if root_to_unmarked > 0 || survivor_to_unmarked > 0 || old_to_unmarked > 0
            {
                "REACHABLE NODE WILL BE SWEPT — case (b) marking/seeding bug"
            } else {
                "no inbound heap edge to any swept node — case (a) register/native root gap or sweep-walk defect"
            };
            tracing::warn!(
                "[sweep-edges] SUMMARY marked={} unmarked={} walk_anomalies={} walk_truncated={} \
                 | edges: root={} young-survivor={} old-gen={} => {}",
                marked_total,
                unmarked_total,
                walk_anomalies.0,
                walk_anomalies.1,
                root_to_unmarked,
                survivor_to_unmarked,
                old_to_unmarked,
                verdict,
            );
        }

        report_phase("optional-edge-diagnostics");

        // ----- Sweep phase ------------------------------------------------
        //
        // Walk the from-space linearly, skipping holes already on the free
        // list (exactly as `OldGen::walk_objects` does — a hole's stale
        // bytes must never be parsed as an object). Every *unmarked*
        // object is dead: zero it and return its span to the free list.
        // Every marked object is a survivor: clear the mark and leave it
        // exactly where it is.
        //
        // gen r4/sweep (2026-09-23): the oracle's snapshot, MOVED rather than
        // rebuilt — the free list has not changed since it was taken (see the
        // note on `sweep_free_blocks`), and this was the third full
        // collect+sort of it in one sweep. The debug check is the tripwire for
        // the day something between the two points starts editing it.
        let existing_free = exact_skips;
        debug_assert!(
            existing_free == merge_skips(young_from.free_blocks_sorted()),
            "young free list changed between the oracle and the sweep walk"
        );
        // MARKWHY: which SKIP ARM covers the watched address? The walk never
        // stopped at the evicted JSP loader's base, so its span is inside a
        // stretch the walk strides over. There are three candidates and they
        // want three different fixes, so name the one that actually applies
        // instead of inferring it from the absence of desync markers.
        //
        // `existing_free` is already the MERGE of the arena free list and the
        // JIT TLAB reservations, so the two are tested separately here — a
        // reserved TLAB tail and a genuine free block are not the same finding.
        // MARKWHY: this sweep RAN, and here is where the watch is relative to it.
        //
        // Every other MARKWHY line is gated on the watch being inside
        // from-space, so all of them go silent together — and that silence has
        // three causes wanting three different next steps: this sweep never
        // ran; it ran and the watched object is not in from-space (promoted, or
        // in the other semispace); or it ran with the object in range and the
        // walk never reached it. Only the third is a statement about the
        // object. Reading it off an absence is how a fact about the collector's
        // schedule becomes a measurement about the heap — which is exactly what
        // happened here: the fourth-recurrence writeup's central chain rests on
        // a sweep that, measured, does not run in that test at all.
        {
            let w = cratonvm_gc_core::heap::young_mark_watch();
            if w != 0 {
                let used_now = young_from.used();
                eprintln!(
                    "[MARKWHY] sweep enter: watch={w:#x} from_base={from_base:#x} used={used_now:#x} in_from_space={} cycle={sweep_zero_cycle}",
                    w >= from_base && w < from_base + used_now,
                );
            }
        }
        {
            let w = cratonvm_gc_core::heap::young_mark_watch();
            if w != 0 && w >= from_base && w < from_base + young_from.used() {
                let off = w - from_base;
                let in_free = young_from
                    .free_blocks_sorted()
                    .iter()
                    .find(|&&(o, sz)| off >= o && off < o + sz)
                    .copied();
                let in_jit = jit_skips
                    .iter()
                    .find(|&&(o, sz)| off >= o && off < o + sz)
                    .copied();
                eprintln!(
                    "[MARKWHY] skip-arm probe: watch={w:#x} off={off:#x} used={:#x} in_free_block={in_free:?} in_jit_tlab_skip={in_jit:?}",
                    young_from.used(),
                );
            }
        }
        // A2 diag (CRATONVM_DBG_A2): does the free list ALREADY self-overlap at
        // sweep start? `existing_free` is built only from prior sweeps' coalesced
        // output + the alloc/split bookkeeping between sweeps. A self-overlap here
        // means the BETWEEN-SWEEP maintenance (Arena::alloc split, or a stale block
        // never removed) is the source — vs. this sweep's frees overlapping it.
        if gc_flags().dbg_a2 {
            for w in existing_free.windows(2) {
                let (a_off, a_sz) = w[0];
                let (b_off, _b_sz) = w[1];
                if b_off < a_off + a_sz {
                    let n = A2_FL_OVERLAP_HITS.load(Ordering::Relaxed);
                    if n < 12 {
                        eprintln!(
                            "[A2-FL] EXISTING-FREE self-overlap at sweep start: [{}, {}) then off={} (prev end {})",
                            a_off, a_off + a_sz, b_off, a_off + a_sz,
                        );
                    }
                    A2_FL_OVERLAP_HITS.fetch_add(1, Ordering::Relaxed);
                    break;
                }
            }
        }
        // (offset, size, class_id, kind_byte, object_count) of spans the walk
        // decided to reclaim. Adjacent dead objects are collapsed by default;
        // forensic gates retain one record per object. DoHead walk-desync
        // hardening (2026-07-02): zeroing and
        // free-list publication are DEFERRED to the publication loop after the
        // walk, so that a later-detected grid anomaly (free-block overshoot /
        // implausible header) can UNWIND the suspect entries collected since
        // the last trustworthy anchor (`dead_watermark`) instead of having
        // already zeroed what may be a live object's interior.
        let retain_dead_objects =
            sweep_zero_enabled() || cratonvm_gc_core::a2dbg::enabled() || gc_flags().dbg_sweep_census;
        let mut dead_regions: Vec<(usize, usize, u32, u8, usize)> = Vec::new();
        // CRATONVM_DBG_SWEEP_LIVENESS: bases of the objects this walk decides to
        // KEEP, so the assertion below can ask whether any of them still points
        // into a span the walk decided to zero. Collected here rather than by a
        // second walk because the walk is the only thing that knows the object
        // grid — re-deriving it is exactly the desync hazard documented on
        // `mark_young_to_old_refs`.
        let mut young_survivors: Vec<usize> = Vec::new();
        EMPTY_RUN_BYTES_CYCLE.store(0, Ordering::Relaxed);
        let mut bytes_swept: usize = 0;
        let mut objects_swept: usize = 0;
        // Index into `dead_regions` at the last trustworthy walk anchor
        // (walk start, or the end of a known free block). Entries above the
        // watermark were collected while striding an unverified stretch.
        let mut dead_watermark: usize = 0;
        // H2-GCVAR: where the walk was last standing on ground truth (walk
        // start, the parallel prefix's verified end, or a free block's end),
        // and the object it strode immediately before the current cursor.
        // The phantom-extent report needs both: it fires where the break is
        // DETECTED, and the stride that caused it is upstream of that.
        let mut last_anchor_off: usize = 0;
        let mut objects_since_anchor: usize = 0;
        let mut prev_obj: (usize, usize, u32, u8) = (0, 0, 0, 0);
        // MARKWHY census: `dead_regions` comes out empty and there are exactly
        // two ways that happens — the walk classified everything LIVE, or it
        // collected spans and UNWOUND them. These counters separate the two in
        // one run instead of another round of inference. Free: five `usize`
        // increments on a path that already does far more per object.
        let mut mw_side = 0usize;
        let mut mw_late = 0usize;
        let mut mw_fwd = 0usize;
        let mut mw_hdr_marked = 0usize;
        let mut mw_dead_pushed = 0usize;
        let mut mw_unwinds = 0usize;
        // Per-unwind-site tally: [0]=free-block overshoot, [1]=oversized-object
        // clamp, [2]=implausible-header resync, [3]=free-list overlap.
        let mut mw_site = [0usize; 4];
        let mut mw_unwound_entries = 0usize;

        let mut objects_live: usize = 0;
        // gen r4/sweep (2026-09-23): survivors (either walk) whose header
        // carries `GC_FLAG_MARKED`. Decides whether the mark-clear re-walk
        // after publication has anything to do — see the note there.
        let mut header_marks_seen: usize = 0;

        // See `YoungMarkCtx::mark_why`; read once, not per object.
        let sweep_mark_why = cratonvm_gc_core::heap::young_mark_watch();
        let mut cursor: usize = 0;
        let used = young_from.used();
        let mut free_iter = existing_free.iter().peekable();
        // xt-hardening (2026-07-03): lockstep iterator over the side mark
        // set (absolute addrs, sorted) — side-marked objects are survivors
        // whose headers were never written; the walk must retain them
        // without touching gc_flags/gc_age.
        let mut side_iter = side_sorted.iter().peekable();
        // H2-GCVAR: monotone probe into the SAME ascending mark set, used by the
        // phantom-extent check below. Separate from `side_iter` because that one
        // answers "is THIS base marked" (equality) while this one answers "does a
        // marked base fall strictly INSIDE the extent" — and the two questions
        // leave the cursor in different places.
        let mut live_probe: usize = 0;
        // H2-CID0: conservative candidates that NEITHER the anchor oracle NOR
        // the late grid walk could place. Lockstep exactly like `side_iter`:
        // an object whose span contains one of these must not be reclaimed,
        // because a conservative root points into it and the collector's
        // invariant is that such a root can only ever over-retain.
        let mut late_iter = late_pins.iter().peekable();
        // Debug diag: retain per-object walk history only under the explicit A2
        // forensic gate. The old default path retained every object (and a
        // later bounded-ring attempt still updated a deque per object), adding
        // gigabytes of metadata traffic across bintrees18's ~19M-node sweep.
        let retain_full_walk = gc_flags().dbg_a2;
        let mut walked_count = 0usize;
        let mut walked: std::collections::VecDeque<(usize, usize, u32, ObjectKind, u32, u32)> =
            std::collections::VecDeque::new();
        // A2 probe (CRATONVM_DBG_A2): parallel to `walked`, the element_type byte
        // and the raw first 8 header bytes AS THE WALKER READ THEM (so a desync
        // dump shows the actual walk-time header, not an unreliable post-zeroing
        // re-read). Indexed in lockstep with `walked`.
        let mut walked_ext: std::collections::VecDeque<(u8, u64)> =
            std::collections::VecDeque::new();

        // ----- Anchored parallel prefix -------------------------------------
        //
        // perf/parallel-young-gc (2026-07-25): sweep `[0, last_anchor)` on
        // `young_gc_threads` workers, one chunk per anchor interval, then let
        // the sequential walk below finish the tail from `cursor`.
        //
        // Why this is sound. The sweep walk is a linear header chase: offset
        // N+1 is only knowable from the object at offset N, which is exactly
        // why it has always been single-threaded. The anchors break that chain
        // WITHOUT guessing -- each one is an offset the oracle walk above
        // already parsed an object at, on a heap that has not been mutated
        // since. Each chunk additionally re-proves its anchor: its chain must
        // land EXACTLY on the next anchor or the attempt is abandoned.
        //
        // Why an abort is free. `parallel_sweep_walk` writes nothing: no
        // zeroing, no free-list publication, no header mark-clear (survivor
        // header writes are deferred to the merge below). On ANY disagreement
        // -- implausible header, unlisted zero span, free-block overshoot,
        // hole-crossing extent, malformed GAP filler, or a chain that misses
        // its anchor -- it returns `None`, `cursor` stays 0, and the
        // sequential walk below runs from scratch exactly as before, keeping
        // sole ownership of the diagnostics and the unwind/re-anchor recovery
        // policy. The forensic gates fall back for the same reason: this fast
        // path deliberately does not reimplement them.
        let sweep_threads = cratonvm_gc_core::young_mark::young_gc_threads(bytes_before);
        // gen r4/sweep (2026-09-23): ONE snapshot of the watched-referent set,
        // shared by the parallel prefix and the sequential walk below. The
        // sequential walk used to call `gc_quiescence::is_watched_referent`
        // per SURVIVOR — a thread-local `RefCell` borrow plus a SipHash probe
        // of a std `HashSet` for every live object, on the walk whose cost is
        // the pause. The set is fixed for the collection (the VM installs it
        // before `collect_garbage` and nothing in the sweep edits it), so the
        // snapshot answers identically; `None` when nothing is watched makes
        // the common case a single branch.
        let watched = cratonvm_gc_core::gc_quiescence::watched_referents_snapshot_for(self.watched_set_key());
        let is_watched = |addr: usize| watched.as_ref().is_some_and(|w| w.contains(&addr));
        // Read once, not per survivor (see `young_survivors`).
        let sweep_liveness_dbg = gc_flags().dbg_sweep_liveness;
        if sweep_threads > 1
            && sweep_anchors.len() > 2
            && !retain_dead_objects
            && !retain_full_walk
            && !watchref_dbg()
            // gen r4/sweep (2026-09-23): `CRATONVM_DBG_SWEEP_LIVENESS` is a
            // forensic gate like the three above. Its young->young half reads
            // `young_survivors`, which only the SEQUENTIAL walk fills, so an
            // accepted prefix left it scanning the tail alone and still
            // printing `hits=0` — a clean verdict from a check that looked at
            // almost nothing. Fall back instead, as the other gates do.
            && !sweep_liveness_dbg
            // H2-CID0: `late_pins` is the rare "a conservative root points into
            // an object neither resolution pass could place" list. The chunk
            // walk deliberately does not reimplement the sequential walk's
            // policies; hand the whole sweep to it rather than teach two
            // walkers the same retention rule.
            && late_pins.is_empty()
            && sweep_anchors[0] == 0
            && sweep_anchors[sweep_anchors.len() - 1] <= used
            && sweep_anchors.windows(2).all(|w| w[0] < w[1])
        {
            let (old_lo, old_hi) = old_gen.extent();
            let sweep_ctx = SweepCtx {
                from_base,
                used,
                existing_free: &existing_free,
                side_bits: &side_bits,
                side_sorted: &side_sorted,
                old_lo,
                old_hi,
                watched: watched.as_ref(),
                size_memo: size_memo_on,
            };
            PAR_SWEEP_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
            // gen r4/sweep: on the persistent pool (design proposal #1).
            // gch h1/yy: an attempt whose chunks reclaimed a forwarded object
            // the sequential walk would RETAIN (its target fails
            // `OldGen::contains`; see `SweepChunkResult::forward_targets`) is
            // discarded like a refused chunk, so the two walks agree.
            // Wave 6 (`CRATONVM_GEN_SWEEP_FUSED_WALK`): the promotion walk
            // already decided these chunks, in the same pass over the arena,
            // under these rules and on these anchors, and the cycle promoted
            // nothing since; take its result instead of walking again. The
            // empty-run bytes it counted are the ones this walk would count.
            let fused_chunks = fused_sweep.take().map(|(chunks, runs)| {
                EMPTY_RUN_BYTES_CYCLE.store(runs, Ordering::Relaxed);
                self.young_premium_census[YP_FUSED_SWEEPS].fetch_add(1, Ordering::Relaxed);
                chunks
            });
            let par_chunks = match fused_chunks {
                Some(chunks) => Some(chunks),
                None => parallel_sweep_walk(
                    &sweep_ctx,
                    &sweep_anchors,
                    sweep_threads,
                    self.evac_pool(sweep_threads),
                ),
            };
            if let Some(chunks) = par_chunks.filter(|chunks| {
                chunks.iter().all(|c| {
                    c.forward_targets
                        .iter()
                        .all(|&f| old_gen.contains(f as *const u8))
                })
            }) {
                PAR_SWEEP_ACCEPTS.fetch_add(1, Ordering::Relaxed);
                for chunk in chunks {
                    for (off, sz, cid, kind, count) in chunk.dead {
                        match dead_regions.last_mut() {
                            Some(last) if last.0 + last.1 == off => {
                                last.1 += sz;
                                last.4 += count;
                            }
                            _ => dead_regions.push((off, sz, cid, kind, count)),
                        }
                    }
                    header_marks_seen += chunk.survivors.len() + chunk.side_with_header_mark;
                    for off in chunk.survivors {
                        // SAFETY: `off` is an object start the chunk walk
                        // verified inside the live from-space region.
                        let h =
                            unsafe { &mut *((from_base + off) as *mut u8 as *mut ObjectHeader) };
                        // gcf f2/y: clear the mark and age by one in ONE
                        // load and plain store (was a locked `fetch_and` plus
                        // `set_gc_age`'s compare-exchange loop); the pause is
                        // stopped the world and this merge is the only
                        // writer. Bit-identical: `sweep_survivor_mark`.
                        let m = h.mark_word.load(Ordering::Relaxed);
                        let (word, new_age) = sweep_survivor_mark_by(m, age_step);
                        h.mark_word.store(word, Ordering::Relaxed);
                        if sweep_age_census {
                            let size = gen_object_total_size(h);
                            tenuring::add_to_age_table(&mut sweep_ages, new_age, size);
                        }
                    }
                    objects_live += chunk.live;
                    for addr in chunk.watched {
                        evac_map.insert(addr, addr);
                    }
                }
                // The parallel prefix is verified ground truth: it is the
                // sequential walk's trustworthy anchor for any later unwind.
                dead_watermark = dead_regions.len();
                cursor = sweep_anchors[sweep_anchors.len() - 1];
                last_anchor_off = cursor;
                objects_since_anchor = 0;
            } else {
                // gen r4/sweep (2026-09-23): the abandoned attempt's chunks may
                // already have added their stepped-over empty runs to the
                // cycle's retention figure, and the sequential walk that now
                // covers the WHOLE arena is about to add them again. Discard
                // the attempt's contribution, exactly as its dead spans are.
                EMPTY_RUN_BYTES_CYCLE.store(0, Ordering::Relaxed);
            }
        }
        report_phase("sweep-walk-parallel-prefix");
        // How much of from-space the PARALLEL prefix consumed.
        //
        // Every MARKWHY sweep hook lives in the SEQUENTIAL walk. When the
        // prefix is accepted it consumes `[0, sweep_anchors.last())` and the
        // sequential walk starts there, so a watched address inside the prefix
        // is one the sequential walk never visits BY CONSTRUCTION — and "the
        // walk never stops at that base" would then be a property of where the
        // instrument is rather than of the object grid.
        let par_prefix_end = cursor;

        // H2-CID0: anchor-validity probe. The parallel sweep prefix splits
        // from-space at `sweep_anchors` and starts an independent chain at each
        // one; its soundness rests entirely on those offsets being real walk
        // positions. This walk visits every walk position in ascending order,
        // so any anchor it strides past without landing on is an anchor that is
        // NOT on the object grid — a chunk started there would parse phantom
        // objects, and a phantom that reads as unmarked reclaims (and zeroes)
        // every live object it subsumes.
        //
        // Only meaningful when this walk covers the whole arena, i.e. the
        // parallel prefix did not already consume the front of it.
        let seq_walk_covers_all = cursor == 0;
        let mut anchor_probe = AnchorGridProbe::new(&sweep_anchors, seq_walk_covers_all);

        // gen r5w1/young5: this walk's grid rules are `probe_grid_at`'s — the
        // same implementation `sweep_chunk`, the selective-promotion walks,
        // the late resolution, the exact-base oracle and `YoungGridCursor`
        // use. Its policy stays its own, arm by arm below: a malformed GAP
        // filler is parsed as a header (`bad_gap_filler_is_anomaly: false`)
        // and the implausible-size arm reports it; the empty-object-run arm
        // gets the unresolved-mark set, which makes the proved-base resume
        // available on this walk only; the phantom check is on.
        let sweep_rules = GridRules {
            base: from_base,
            used,
            zero_run_hi: used,
            side_sorted: side_sorted.as_slice(),
            zero_runs: ZeroRunRule::StepOver {
                unresolved: Some(unresolved_snapshot.as_slice()),
                count_empty_runs: true,
            },
            bad_gap_filler_is_anomaly: false,
            phantom: true,
        };
        // gcf f3/q (r15w5/gcpage patch 1): one size memo for this walk.
        let mut seq_size_memo = GridSizeMemo::new(size_memo_on);

        while cursor < used {
            // Skip known free blocks ROBUSTLY. The free list (`existing_free`) is
            // sorted ascending and the walk advances `cursor` monotonically, so we
            // can stride `free_iter` forward in lockstep. Crucially this handles the
            // case where a previous object's size brought the cursor PAST a free
            // block's start (`cursor > off`): the old code only matched `cursor ==
            // off`, so a single overshoot wedged `free_iter` at that block FOREVER —
            // and every later free block was then read as a run of zeroed 40-byte
            // phantom `Object`s (class_id=0, kind=Object, num_slots=0 → size 40),
            // desyncing the walk off the object grid (the A2 / ReflRepro corruption:
            // `CRATONVM_DBG_A2` shows freed+zeroed regions being walked, not skipped).
            // Now: drop free blocks the cursor has wholly passed, and if the cursor
            // lands AT or INSIDE a free block, resync to that block's end.
            {
                // Where the walk stood BEFORE the free-block resync — the
                // anchor probe below needs it to tell "crossed a known free
                // block" (benign) from "strode past by an object" (the finding).
                let pre_skip = cursor;
                let (resynced, overshot) = skip_free_blocks(&mut cursor, &mut free_iter);
                if overshot {
                    // The previous stride ran INTO a known free block: the
                    // walk grid broke somewhere after the last anchor, so
                    // every reclaim decision made since then is suspect —
                    // it may cover a live object's interior. Unwind them
                    // (they have not been zeroed or published yet;
                    // over-retention is always safe under this sweep).
                    {
                        mw_unwinds += 1;
                        mw_site[0] += 1;
                        mw_unwound_entries += dead_regions.len() - dead_watermark;
                        dead_regions.truncate(dead_watermark);
                    }
                }
                if resynced {
                    // A free block's end is ground truth — a fresh anchor.
                    dead_watermark = dead_regions.len();
                    last_anchor_off = cursor;
                    objects_since_anchor = 0;
                    // Anchor-validity probe: this `continue` skips the
                    // walk-position probe below, so tell it about the span the
                    // resync just crossed or the next iteration reports every
                    // anchor inside it as off-grid.
                    anchor_probe.crossed_free_block(pre_skip, cursor);
                    continue;
                }
            }
            // H2-CID0 anchor-validity probe (see `SWEEP_ANCHOR_NOT_A_BASE`).
            // `cursor` is a walk position the grid actually reaches — an object
            // start or a GAP-filler sentinel, both of which `sweep_chunk`
            // handles. Anything the anchor list holds strictly below it was
            // never reachable.
            anchor_probe.at_walk_position(cursor);
            // `cursor` is within `used`; the from-space region
            // `[base, base+used)` is backed by mapped, allocated memory. The
            // integer-to-pointer cast itself is safe; only the header deref
            // on the next line requires `unsafe`.
            let obj_ptr = (from_base + cursor) as *mut u8;
            // gen r5w1/young5: the rules below (GAP filler, zero run, size,
            // free-hole crossing, phantom extent) are judged ONCE by
            // `probe_grid_at` under `sweep_rules`; the arms keep this walk's
            // own policy and diagnostics.
            //
            // Bug-D fix (2026-06-12): a GAP-filler sentinel marks a
            // sub-`HEADER_SIZE` TLAB tail (`install_tail_filler`) that is too
            // small to hold a walkable `int[]`. It carries its exact byte
            // length at offset 4. Reclaim the span and continue — this MUST
            // run before `gen_object_total_size`, whose `num_slots` read
            // (offset 16) would fall outside an 8-byte gap. We SKIP rather
            // than free it: a sub-`HEADER_SIZE` span can never satisfy an
            // allocation (the smallest object/array is `HEADER_SIZE` bytes),
            // so adding it to the free list only bloats the linear free-list
            // scan with permanently-unusable tiny blocks. Skipping keeps the
            // walk exactly on the object grid; the span is reclaimed wholesale
            // when the next moving (Cheney) cycle resets from-space. A
            // malformed sentinel (should be impossible) is parsed as a header
            // (`bad_gap_filler_is_anomaly: false`) and falls into the
            // corrupt-header re-sync path below.
            //
            // DoHead walk-desync hardening (2026-07-02): an all-zero header
            // word at a grid offset is NEVER a walkable object — it is a
            // reclaimed-then-zeroed span that is missing from the free list
            // (freed-but-unlisted residue), or a freed-then-REUSED slot whose
            // new owner's header was clobbered back to zero by a stale
            // register-held reference (the DoHead AQS family). The old code
            // strode such spans as 40-byte phantom "objects" and re-FREED
            // each one: spans are 40+16n bytes, so the final phantom window
            // crossed the span's end into the next LIVE object's header,
            // zeroing it and minting a free block inside a live object →
            // overlapping allocations → UAF. Instead: never parse, never
            // free (the span may be a live-but-clobbered allocation whose
            // memory must not be double-served); skip to the next known
            // free-block anchor and resume on-grid there.
            //
            // `side_sorted` is this cycle's independently-computed live set
            // (object bases, ascending). Since HEADER_SIZE 16 made the hash
            // lazy, a live, never-hashed, never-locked object with `ClassId(0)`
            // and no shape bits reads all-zero — indistinguishable from
            // reclaimed memory by header bytes alone. An address the marker
            // vouches for is not anomaly evidence, whatever its header reads:
            // it is parsed as a normal live header (the probe's `vouched`
            // test), instead of unwinding every reclaim decision taken since
            // the last anchor for an ordinary live object. The same holds for
            // a zero run shorter than a header (a real `ClassId(0)` ad-hoc
            // container's header legitimately starts with zero words but has
            // a non-zero `num_slots`/`gc_flags` word).
            //
            // A run of EMPTY objects is not a desync — step over it on-grid
            // and keep every reclaim decision behind it. See
            // `zero_run_empty_object_resume` for the measurement that
            // separates this shape from the corruption the unwind below
            // exists for. Still never parsed and never freed: over-retention
            // is always safe here. `resume <= run_end <= limit` (the next free
            // block's offset), so the run never crosses a free block and
            // `free_iter` is already positioned correctly.
            let next_free = free_iter.peek().map(|&&(off, _)| off);
            // `cursor < used`, inside mapped from-space (`probe_grid_at`'s
            // contract).
            let verdict = probe_grid_at_sized(
                cursor,
                next_free,
                &sweep_rules,
                &mut live_probe,
                &mut seq_size_memo,
            );
            let total_size = match verdict {
                GridProbe::Gap { len } => {
                    cursor += len;
                    continue;
                }
                GridProbe::EmptyRun { resume } => {
                    cursor = resume;
                    continue;
                }
                GridProbe::Object { size }
                | GridProbe::BadSize { size }
                | GridProbe::CrossesFreeBlock { size, .. }
                | GridProbe::Phantom { size, .. } => size,
                // Not produced under `sweep_rules` (a malformed sentinel is
                // parsed); judged as an implausible size below either way.
                GridProbe::BadGapFiller => 0,
                GridProbe::ZeroSpan { run_end, limit } => {
                    let n = SWEEP_ZERO_SPAN_HITS.fetch_add(1, Ordering::Relaxed);
                    if n < 8 {
                        tracing::warn!(
                            "non-moving sweep: unlisted all-zero span at offset {} \
                             (run {} bytes, next anchor at {}) — skipped, not freed",
                            cursor,
                            run_end - cursor,
                            limit,
                        );
                    }
                    // A zero span is anomaly evidence like any other: the
                    // walk can only claim `cursor` is on-grid if every stride
                    // since the last anchor was correctly sized — and a
                    // mis-sized stride landing in a live object's zeroed
                    // interior produces exactly this signature. Unwind the
                    // reclaim decisions collected since the anchor
                    // (over-retention is always safe), then re-anchor at the
                    // next free block, or stop if no anchor remains.
                    // Reclaim decisions taken since the last anchor were taken
                    // on a grid this span calls into question -- drop them. That
                    // half was always right.
                    {
                        mw_unwinds += 1;
                        mw_site[1] += 1;
                        mw_unwound_entries += dead_regions.len() - dead_watermark;
                        dead_regions.truncate(dead_watermark);
                    }
                    // What was wrong is the RESUME. Re-anchoring at the next
                    // FREE BLOCK abandons everything in between, and when the
                    // free list is empty there is no anchor at all, so the
                    // sweep freed nothing whatsoever: measured on the H2 UPDATE
                    // path at 233 MB of a 256 MB young generation skipped from
                    // one 16-byte span, `young_free_list=0` and
                    // `freed == promoted` exactly, until old gen bled out into
                    // `OutOfMemoryError` at 68 MB live in a 1 GB heap. See
                    // jit-young-heap-exhaustion-after-header-16-FIXED-20260807.md
                    //
                    // Resume at the next allocator-recorded object start
                    // instead: on-grid by construction (see `next_grid_anchor`
                    // for why no byte-level stride can be), and TLAB-granular,
                    // so the cost is one TLAB retained rather than the arena.
                    if let Some(a) = next_grid_anchor(&grid_anchors, cursor) {
                        if a > cursor && a < used {
                            // Keep the free-block cursor in step with the jump.
                            while free_iter.peek().is_some_and(|&&(off, sz)| off + sz <= a) {
                                free_iter.next();
                            }
                            cursor = a;
                            continue;
                        }
                    }
                    if resync_to_next_free_block(&mut cursor, &mut free_iter) {
                        continue;
                    }
                    break;
                }
            };
            // SAFETY: `cursor < used`, so `from_base + cursor` is the start of a
            // header inside mapped from-space memory; this walk holds the only
            // mutable access during sweep, so the `&mut` is unique.
            let header = unsafe { &mut *(obj_ptr as *const ObjectHeader as *mut ObjectHeader) };
            // Defensive: a corrupt / zero-size header would desynchronise
            // the linear walk. Stop rather than risk freeing live data.
            if matches!(verdict, GridProbe::BadSize { .. } | GridProbe::BadGapFiller) {
                // NOTE: format the RAW kind byte, not `header.kind` via Debug.
                // A corrupt header can hold an out-of-range discriminant; the
                // derived `Debug` for `ObjectKind` indexes a static name table
                // by discriminant, so `{:?}` on an invalid value reads past the
                // table into rodata and SIGSEGVs — turning a recoverable
                // corrupt-header detection into a hard crash (observed: JIT
                // miscompile of `JUnitCore.main` under real-JCA). Same for the
                // re-sync probe below.
                let raw_kind = ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed));
                if SWEEP_CORRUPTION_HITS.fetch_add(1, Ordering::Relaxed) == 0 {
                    eprintln!(
                        "[quiesce] FIRST corruption: quiescence depth={} enter_count={} leave_count={}",
                        cratonvm_gc_core::gc_quiescence::depth(),
                        cratonvm_gc_core::gc_quiescence::ENTER_COUNT.get(),
                        cratonvm_gc_core::gc_quiescence::LEAVE_COUNT.get(),
                    );
                }
                tracing::warn!(
                    "non-moving sweep: stopping walk at offset {} — implausible \
                     object size {} (kind=0x{:02x}, num_slots={}, array_len={}, class_id={})",
                    cursor,
                    total_size,
                    raw_kind,
                    header.num_slots(),
                    header.array_length(),
                    header.class_id.as_u32(),
                );
                tracing::warn!(
                    "  total walked={} objects, used={} from_base={:#x}",
                    walked_count,
                    used,
                    from_base,
                );
                // Find the FIRST cursor where the all-zero-header pattern
                // started (num_slots == 0 && class_id == 0 && kind == Object).
                let first_zero = walked.iter().position(|(_, _, cid, k, ns, _)| {
                    *ns == 0 && *cid == 0 && matches!(k, ObjectKind::Object)
                });
                if let Some(idx) = first_zero {
                    let start = idx.saturating_sub(15);
                    for i in start..=idx {
                        let (off, sz, cid, kind, ns, al) = walked[i];
                        tracing::warn!(
                            "  PRE-corruption idx {} @off={} size={} class_id={} kind=0x{:02x} num_slots={} array_length={}",
                            i, off, sz, cid, kind as u8, ns, al,
                        );
                    }
                }
                let recent: Vec<_> = walked.iter().rev().take(8).rev().cloned().collect();
                for (off, sz, cid, kind, ns, al) in &recent {
                    tracing::warn!(
                        "  prior obj @off={} size={} class_id={} kind=0x{:02x} num_slots={} array_length={}",
                        off, sz, cid, *kind as u8, ns, al,
                    );
                }
                // Dump 64 bytes of context starting 16 bytes before the bad
                // header so we can see the tail of the previous object's payload.
                let start = cursor.saturating_sub(16);
                let end = (cursor + 48).min(used);
                let mut hex = String::new();
                for i in start..end {
                    // SAFETY: i < used, region mapped.
                    let b = unsafe { *((from_base + i) as *const u8) };
                    hex.push_str(&format!("{:02x} ", b));
                    if (i - start + 1) % 16 == 0 {
                        hex.push('\n');
                    }
                }
                tracing::warn!("  bytes around bad header (start_off={}):\n{}", start, hex);

                // A2 forensic probe (CRATONVM_DBG_A2): identify the corrupt object.
                // The hypothesis is the corrupt slot at `cursor` is an OBJECT whose
                // 40-byte header was overwritten by 16-byte Value-cell field data
                // (allocate-then-putfield clobber), OR the PRIOR object (an array)
                // is a mis-typed reference array the walker under-sized. Dump the
                // prior object's exact element_type/elem-bytes and resolve the
                // Value cells at `cursor` (disc + payload, and whether the payload
                // points into the young arena).
                if gc_flags().dbg_a2 && A2_PROBE_HITS.fetch_add(1, Ordering::Relaxed) < 4 {
                    if let Some(&(loff, lsz, lcid, lkind, lns, lal)) = walked.back() {
                        // SAFETY: loff < used, header mapped.
                        let lhdr = unsafe { &*((from_base + loff) as *const ObjectHeader) };
                        eprintln!(
                            "[A2] PRIOR obj @{} size={} class_id={} kind={:?} num_slots={} array_len={} ELEM_TYPE={:?} elem_bytes={} (cursor={} = {}+{})",
                            loff, lsz, lcid, lkind, lns, lal,
                            lhdr.element_type(),
                            cratonvm_gc_core::heap::element_byte_size(lhdr.element_type()),
                            cursor, loff, lsz,
                        );
                    }
                    eprintln!(
                        "[A2] corruption @off={} from_base={:#x} used={}",
                        cursor, from_base, used
                    );
                    for k in 0..8usize {
                        let off = cursor + k * 16;
                        if off + 16 > used {
                            break;
                        }
                        // SAFETY: off+16 <= used, region mapped.
                        let disc = unsafe { *((from_base + off) as *const u64) };
                        let payload = unsafe { *((from_base + off + 8) as *const u64) };
                        let in_young =
                            payload >= from_base as u64 && payload < (from_base + used) as u64;
                        // If the payload points into the young arena at an aligned
                        // object boundary, read its class_id to identify the referent.
                        let referent = if in_young && (payload as usize - from_base) % 8 == 0 {
                            // SAFETY: `payload` was checked to point into the mapped from-space semi-space at an 8-aligned
                            // boundary (`in_young` && `% 8 == 0`); read-only diagnostic ([A2]) header read.
                            let rh = unsafe { &*(payload as *const ObjectHeader) };
                            format!(
                                "young referent class_id={} kind={} num_slots={}",
                                rh.class_id.as_u32(),
                                ObjectHeader::kind_tag(rh.mark_word.load(Ordering::Relaxed)),
                                rh.num_slots()
                            )
                        } else if payload >> 40 == (from_base as u64) >> 40 {
                            "heap-ptr (old-gen?)".to_string()
                        } else {
                            "(not-heap)".to_string()
                        };
                        eprintln!(
                            "[A2]   cell[{}] @{} disc={} payload={:#x} {}",
                            k, off, disc, payload, referent
                        );
                    }
                    // BREADCRUMB: what was actually allocated at/covering the
                    // corrupt cursor, and the prior object's REAL allocated size
                    // vs the size the walker computed (the decisive comparison).
                    match cratonvm_gc_core::a2dbg::lookup_covering(from_base + cursor) {
                        Some(r) => eprintln!(
                            "[A2] BREADCRUMB cursor@{} ({:#x}) covered by alloc start={:#x} class_id={} kind={} et={} alen={} ns={} REAL_size={} seq={} (mid-object offset={})",
                            cursor, from_base + cursor, r.addr, r.class_id, r.kind, r.element_type, r.array_length, r.num_slots, r.size, r.seq, (from_base + cursor) - r.addr,
                        ),
                        None => eprintln!(
                            "[A2] BREADCRUMB cursor@{} ({:#x}) — NO young alloc record covers it (header never written here, or freed+reused)",
                            cursor, from_base + cursor,
                        ),
                    }
                    if let Some(&(loff, lsz, _, _, _, _)) = walked.back() {
                        // Walk-time header AS THE WALKER READ IT (element_type byte
                        // + raw first 8 bytes). This is the decisive value: if it
                        // shows a 4-byte element_type for a byte[], the header is
                        // corrupt at walk time (vs an allocator/walker formula bug).
                        if let Some(&(wet, w0)) = walked_ext.back() {
                            eprintln!(
                                "[A2] WALK-TIME prior@{} element_type_byte={} raw_word0={:#018x} (b0=class_id_lo b4=kind b5=element_type)",
                                loff, wet, w0,
                            );
                        }
                        match cratonvm_gc_core::a2dbg::lookup_at(from_base + loff) {
                            Some(r) if r.kind == 0xFF => eprintln!(
                                "[A2] BREADCRUMB prior@{} — slot was FREED by the sweep (stale record); walker_size={}",
                                loff, lsz,
                            ),
                            Some(r) => eprintln!(
                                "[A2] BREADCRUMB prior@{} alloc class_id={} kind={} et={} alen={} ns={} REAL_size={} vs WALKER_size={} {}",
                                loff, r.class_id, r.kind, r.element_type, r.array_length, r.num_slots, r.size, lsz,
                                if r.size == lsz { "(match)" } else { "*** SIZE MISMATCH ***" },
                            ),
                            None => eprintln!(
                                "[A2] BREADCRUMB prior@{} — no exact alloc record (walker_size={})",
                                loff, lsz,
                            ),
                        }
                    }
                    // The desync DETECTION above is downstream: the walk may have
                    // silently overshot earlier (reading garbage that still passed
                    // the plausibility check). Find the FIRST walked object whose
                    // walker-computed size disagrees with its real allocated size —
                    // that is the ROOT overshoot. Compare walk-time header (et/raw0)
                    // to the alloc record to classify header-corruption vs reuse.
                    for (i, &(woff, wsz, _, _, _, _)) in walked.iter().enumerate() {
                        if let Some(r) = cratonvm_gc_core::a2dbg::lookup_at(from_base + woff) {
                            if r.kind != 0xFF && r.size != wsz {
                                let (wet, w0) = walked_ext.get(i).copied().unwrap_or((255, 0));
                                eprintln!(
                                    "[A2] FIRST-MISMATCH walked[{}]@{} walker_size={} walk_et={} raw0={:#018x} vs REAL(class_id={} kind={} et={} alen={} ns={} size={} seq={})",
                                    i, woff, wsz, wet, w0,
                                    r.class_id, r.kind, r.element_type, r.array_length, r.num_slots, r.size, r.seq,
                                );
                                break;
                            }
                        }
                    }
                }

                // Defensive recovery — DoHead walk-desync hardening
                // (2026-07-02): re-anchor at the START of the next known
                // free block instead of the old 8-byte plausibility probe.
                // The probe's check ACCEPTED an all-zero header (size = 40
                // passes every bound), so it could "re-sync" onto the first
                // zeroed word OFF the object grid and resume phantom-freeing
                // from an arbitrary offset — feeding the very overlap it was
                // recovering from. Free-block boundaries are the only ground
                // truth downstream; the stretch up to the anchor is retained
                // (recovered when a moving collection next resets the space).
                // The implausible header ALSO means the grid may have broken
                // BEFORE `cursor` (a mis-sized stride that happened to keep
                // passing the plausibility checks), so unwind the reclaim
                // decisions made since the last anchor — they may cover a
                // live object's interior. They have not been zeroed or
                // published yet (deferred to the publication loop).
                {
                    mw_unwinds += 1;
                    mw_site[2] += 1;
                    mw_unwound_entries += dead_regions.len() - dead_watermark;
                    dead_regions.truncate(dead_watermark);
                }
                if resync_to_next_free_block(&mut cursor, &mut free_iter) {
                    tracing::warn!(
                        "non-moving sweep: re-anchored at next free block (offset {}); \
                         skipped stretch retained until a moving cycle resets from-space",
                        cursor,
                    );
                    continue;
                }
                tracing::warn!(
                    "non-moving sweep: no free-block anchor remains after offset {} — \
                     abandoning rest of arena ({} bytes retained)",
                    cursor,
                    used - cursor,
                );
                break;
            }

            // A2 fix: an object can never span a pre-existing free HOLE (holes are
            // gaps between objects, not object interiors). If the computed size
            // oversteps the next known free block, the header is over-sized — the
            // ReflRepro `et=Int` byte-array corruption reads a `byte[N]` as an
            // `int[N]` (4× the element size). Without this guard the sweep would
            // FREE the over-large span `[cursor, cursor+total_size)`, which overlaps
            // the free hole (the `DEAD-vs-EXISTING` overlap) and feeds the
            // overlapping-free-block / double-serve cycle the coalescer then has to
            // mop up. Instead, do NOT free or trust this span (it may subsume a live
            // neighbour) — RETAIN it (over-retention is always safe under the
            // non-moving sweep) and re-sync the cursor at the hole boundary, where
            // the robust free-block skip takes over. Caps the desync to a single
            // object instead of an overstep cascade.
            if let GridProbe::CrossesFreeBlock { free_off: foff, .. } = verdict {
                if gc_flags().dbg_a2 && A2_FL_OVERLAP_HITS.load(Ordering::Relaxed) < 30 {
                    eprintln!(
                        "[A2-FL] CLAMP over-sized object @{} computed_size={} (kind=0x{:02x} class_id={}) oversteps free hole at {} — retaining + resyncing",
                        cursor, total_size, ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)), header.class_id.as_u32(), foff,
                    );
                }
                A2_FL_OVERLAP_HITS.fetch_add(1, Ordering::Relaxed);
                // A hole-crossing header is grid-break evidence: the
                // decisions since the last anchor are suspect — unwind
                // them (over-retention safe) before re-anchoring at the
                // hole, where the skip loop takes over.
                {
                    mw_unwinds += 1;
                    mw_site[3] += 1;
                    mw_unwound_entries += dead_regions.len() - dead_watermark;
                    dead_regions.truncate(dead_watermark);
                }
                cursor = foff;
                continue;
            }

            // ----- Phantom extent: a header that SUBSUMES a live object ------
            //
            // `side_sorted` is this cycle's mark set — object BASES, ascending —
            // and live objects never nest. So a marked address strictly inside
            // `(cursor, cursor + total_size)` is proof that this header's extent
            // is not an object's: the walk left the object grid HERE.
            //
            // Until this check the only thing that noticed was the end-of-sweep
            // `dead_regions` × `side_sorted` merge (`LIVE_IN_DEAD_SPANS`). That
            // merge is sound — it RETAINS the span rather than zeroing it — but
            // it runs after the walk has already strided the phantom, so every
            // decision from the phantom to the end of the arena was taken on a
            // grid that never resynchronised, and the unwind then throws all of
            // them away. Measured on the 2026-08-10 three-collector H2 sweep:
            // eight classes reported `spans=1 span_bytes=197216
            // span_head_class_id=4044482304` — one ~192 KB "object" swallowing
            // live ones — the same victim address repeatedly, milliseconds
            // apart, and all eight then blew the 300 s per-class cap.
            //
            // Same reasoning as the G1 sibling's `scan_source_region_for_cset_refs`
            // desync check (see `docs/known-issues/hibernate/g1-collector-fullsuite-*.md`):
            // a desynchronised linear walk cannot resynchronise itself, so stop
            // trusting it and re-anchor on ground truth.
            //
            // Cost is one monotone index advance per object over a list the mark
            // phase already built and sorted — no allocation, no search.
            {
                let abs = from_base + cursor;
                // `probe_grid_at` ran `mark_strictly_inside` on `live_probe`
                // for this extent (`sweep_rules.phantom`), after the size and
                // hole checks exactly as this walk always did.
                if let GridProbe::Phantom { victim, .. } = verdict {
                    // Are the subsumed marks object BASES? See
                    // `SWEEP_PHANTOM_INTERIOR_MARKS` — this classifies, it does
                    // not gate. Bounded by the marks inside this one extent.
                    let mut nonbase_inside = 0u64;
                    let mut probe = live_probe;
                    while let Some(&a) = side_sorted.get(probe) {
                        if a >= abs + total_size {
                            break;
                        }
                        if unresolved_snapshot.binary_search(&a).is_ok() {
                            nonbase_inside += 1;
                        }
                        probe += 1;
                    }
                    if nonbase_inside != 0 {
                        SWEEP_PHANTOM_INTERIOR_MARKS.fetch_add(nonbase_inside, Ordering::Relaxed);
                    }
                    let victim_is_unresolved_raw =
                        unresolved_snapshot.binary_search(&victim).is_ok();
                    SWEEP_PHANTOM_EXTENTS.fetch_add(1, Ordering::Relaxed);
                    if SWEEP_PHANTOM_REPORTS.fetch_add(1, Ordering::Relaxed) < 8 {
                        tracing::error!(
                            target: "cratonvm::gc::guard",
                            offset = cursor,
                            span_bytes = total_size,
                            span_head_class_id = header.class_id.as_u32(),
                            // RAW kind byte: a corrupt header can hold an
                            // out-of-range discriminant, and `{:?}` on one indexes
                            // a static name table past its end.
                            kind_byte = ObjectHeader::kind_tag(
                                header.mark_word.load(Ordering::Relaxed)
                            ),
                            num_slots = header.num_slots(),
                            victim = format!("{victim:#x}"),
                            victim_interior_offset = victim - abs,
                            // The discriminator: `true` means this "live
                            // object" is an unresolved RAW conservative
                            // candidate, i.e. possibly an interior word of the
                            // very object being sized here, and the verdict is
                            // then a false positive. See
                            // `SWEEP_PHANTOM_INTERIOR_MARKS`.
                            victim_is_unresolved_raw,
                            nonbase_marks_inside = nonbase_inside,
                            // Where the walk last stood on ground truth, and the
                            // stride that took it from there to here. The break is
                            // upstream of the detection: if `prev_*` sizes an object
                            // that does not end at this offset, `prev_` IS the
                            // mis-sized stride; if it does, look further back, and
                            // `objects_since_anchor` says how far there is to look.
                            last_anchor_off,
                            objects_since_anchor,
                            prev_off = prev_obj.0,
                            prev_size = prev_obj.1,
                            prev_class_id = prev_obj.2,
                            prev_kind_byte = prev_obj.3,
                            "young non-moving sweep: the header at this offset claims an \
                             extent that SUBSUMES a live (marked) object. Live objects never \
                             nest, so the walk is off the object grid here. Re-anchoring at \
                             the next allocator-recorded object start rather than striding \
                             the phantom and deciding the rest of the arena on a broken grid.",
                        );
                    }
                    // Everything reclaimed since the last anchor was decided on a
                    // grid this header calls into question — unwind it
                    // (over-retention is always safe under this sweep), then
                    // re-anchor exactly as the unlisted-zero-span path does.
                    dead_regions.truncate(dead_watermark);
                    if let Some(a) = next_grid_anchor(&grid_anchors, cursor) {
                        if a > cursor && a < used {
                            while free_iter.peek().is_some_and(|&&(off, sz)| off + sz <= a) {
                                free_iter.next();
                            }
                            cursor = a;
                            last_anchor_off = cursor;
                            objects_since_anchor = 0;
                            continue;
                        }
                    }
                    if resync_to_next_free_block(&mut cursor, &mut free_iter) {
                        last_anchor_off = cursor;
                        objects_since_anchor = 0;
                        continue;
                    }
                    break;
                }
            }

            prev_obj = (
                cursor,
                total_size,
                header.class_id.as_u32(),
                ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)),
            );
            objects_since_anchor += 1;
            walked_count += 1;
            // The allocator invariant, measured rather than assumed. Every
            // object the walk sizes reached the heap through some allocation
            // path, so every one of them should carry `GC_FLAG_HEADER`; a
            // non-zero count here names a path that publishes a header without
            // it. Nothing DEPENDS on the flag today (see
            // `header_reserved_fields_plausible`), which is precisely why the
            // count has to exist: an invariant nothing checks is an invariant
            // that decays silently, and the next change in this area — turning
            // the plausibility screen into `gc_flags & GC_FLAG_HEADER != 0` —
            // must not be made until this has been read clean on real
            // workloads.
            if header.gc_flags() & GC_FLAG_HEADER == 0 {
                SWEEP_NO_HEADER_FLAG.fetch_add(1, Ordering::Relaxed);
            }
            if retain_full_walk {
                walked.push_back((
                    cursor,
                    total_size,
                    header.class_id.as_u32(),
                    header.kind(),
                    header.num_slots(),
                    header.array_length(),
                ));
                // SAFETY: obj_ptr is the mapped header start; reading 8 bytes is in bounds.
                walked_ext.push_back((header.element_type() as u8, unsafe {
                    *(obj_ptr as *const u64)
                }));
            }

            let side_marked_survivor = if !header.is_forwarded() {
                // xt-hardening (2026-07-03): side-marked survivor check
                // (lockstep, absolute addrs). These candidates were kept
                // alive WITHOUT a header write; retain them without writing
                // gc_flags/gc_age either (their "header" may be a zero span
                // or a legal zero-word0 container — never write through it).
                let abs = from_base + cursor;
                while let Some(&&a) = side_iter.peek() {
                    if a < abs {
                        side_iter.next();
                    } else {
                        break;
                    }
                }
                side_iter.peek().is_some_and(|&&a| a == abs)
            } else {
                false
            };

            // H2-CID0: does an unplaceable conservative candidate point INSIDE
            // this object? Same lockstep discipline as `side_iter`, and free
            // (a single `is_empty` test) on every healthy cycle.
            let late_pinned = if late_pins.is_empty() {
                false
            } else {
                let abs = from_base + cursor;
                while let Some(&&a) = late_iter.peek() {
                    if a < abs {
                        late_iter.next();
                    } else {
                        break;
                    }
                }
                let hit = late_iter.peek().is_some_and(|&&a| a < abs + total_size);
                if hit && !side_marked_survivor && !header.is_forwarded() {
                    LATE_PIN_RETENTIONS.fetch_add(1, Ordering::Relaxed);
                }
                hit
            };

            // MARKWHY: the young marker's edge trace answers "was it marked".
            // This answers the different question the sweep decides — "will
            // its span be reclaimed and zeroed" — which is what
            // `is_live_young_survivor` (word0 != 0) later reports as liveness.
            // The two can disagree: `late_pinned` retains a span from an
            // INTERIOR conservative candidate, an address that never equals
            // the object base and so never trips the edge trace.
            // MARKWHY, the STRADDLE case — the one that matters when the hook
            // below never fires. "The walk covers the address and never stops
            // at it" is a statement about SOME OTHER object: the base is inside
            // an extent computed for an object that starts earlier. The hook
            // below tests EQUALITY with the base, so it is silent in exactly
            // that case and cannot name the culprit stride. This prints the raw
            // header words the stride was computed from, not a re-read — a
            // re-read after the fact is what a desync corrupts.
            if sweep_mark_why != 0 {
                let base = from_base + cursor;
                if base < sweep_mark_why && sweep_mark_why < base + total_size {
                    let n = MARKWHY_STRADDLE_HITS.fetch_add(1, Ordering::Relaxed);
                    if n < 8 {
                        // SAFETY: `cursor + HEADER_SIZE <= used` for any object
                        // the walk sized, so both header words are in bounds.
                        let (raw0, raw1) = unsafe {
                            (*(obj_ptr as *const u64), *((obj_ptr as *const u64).add(1)))
                        };
                        eprintln!(
                            "[MARKWHY] STRADDLE: object @{base:#x} off={cursor:#x} size={total_size} swallows watch={sweep_mark_why:#x} (+{} into it) class_id={} kind={:?} elem={:?} shape={} compact={} body_size={} array_len={} raw0={raw0:#018x} raw1={raw1:#018x} since_anchor={objects_since_anchor} anchor={last_anchor_off:#x} prev=({:#x},{},{},{})",
                            sweep_mark_why - base,
                            header.class_id.as_u32(),
                            header.kind(),
                            header.element_type(),
                            header.raw_shape(),
                            is_compact_object(header),
                            object_instance_size(header),
                            header.array_length(),
                            prev_obj.0,
                            prev_obj.1,
                            prev_obj.2,
                            prev_obj.3,
                        );
                    }
                }
            }
            if sweep_mark_why != 0 && from_base + cursor == sweep_mark_why {
                eprintln!(
                    "[MARKWHY] sweep @{:#x} size={total_size} side_marked={side_marked_survivor} late_pinned={late_pinned} forwarded={} header_marked={}",
                    from_base + cursor,
                    header.is_forwarded(),
                    header.gc_flags() & GC_FLAG_MARKED != 0,
                );
            }

            if header.is_forwarded() {
                mw_fwd += 1;
                // Evacuated to old gen by selective promotion: the live copy is
                // in old gen and references were redirected in the fixup pass;
                // reclaim (and zero) the young slot. (A "don't zero" variant was
                // tested to rule out a register-only dangling read — it did NOT
                // fix bintrees18's wrong checksum, so the residual corruption is
                // a structural wrong-address fixup, not a dangling read.)
                //
                // DoHead walk-desync hardening (2026-07-02): selective
                // promotion only ever forwards INTO old gen, so a forwarding
                // target outside it is a phantom write from a desynced walk
                // (or header corruption) — retain the span instead of zeroing
                // and freeing what may be a live object's interior.
                let fwd = header.forwarding_address();
                if watchref_dbg() && is_watched(obj_ptr as usize) {
                    eprintln!(
                        "[watchref] non-moving sweep: watched address @0x{:x} was EVACUATED to old gen @{:p} (should already be in evac_map from selective promotion)",
                        obj_ptr as usize, fwd
                    );
                }
                if !old_gen.contains(fwd) {
                    let n = SWEEP_BAD_FORWARD_HITS.fetch_add(1, Ordering::Relaxed);
                    if n < 8 {
                        tracing::warn!(
                            "non-moving sweep: forwarded young object at offset {} has \
                             non-old-gen target {:p} — retaining span, not freeing",
                            cursor,
                            fwd,
                        );
                    }
                } else if retain_dead_objects {
                    // Forensic gates: one record per object, never merged.
                    dead_regions.push((
                        cursor,
                        total_size,
                        header.class_id.as_u32(),
                        ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)),
                        1,
                    ));
                } else {
                    // gen r4/sweep (2026-09-23): the SAME merge the parallel
                    // chunks use (`push_dead`), instead of a hand-inlined copy
                    // of it in each disposition arm — two copies of one rule in
                    // one walk, plus a third in `sweep_chunk`, is how the
                    // MARKWHY `dead_pushed` tally ended up incremented in
                    // opposite branches here and in the unmarked arm below.
                    push_dead(&mut dead_regions, cursor, total_size, header);
                }
            } else if side_marked_survivor || late_pinned {
                if side_marked_survivor {
                    mw_side += 1;
                } else {
                    mw_late += 1;
                }
                // Side-marked survivor: pure retention, no header writes.
                // `late_pinned` joins it here for the same reason — a
                // conservative root points into this object, and the header
                // may not be one we are allowed to write through.
                objects_live += 1;
                let header_marked_here = header.gc_flags() & GC_FLAG_MARKED != 0;
                if header_marked_here && late_pinned && !side_marked_survivor {
                    // gcf f4/y (f3/q review finding 2): a HEADER-marked, not
                    // side-marked, late-pinned survivor. Walk (2) does not age
                    // it (header-marked and not side-marked), and this arm,
                    // tested before the header arm, used to write nothing, so
                    // it was aged by neither pass. Age it exactly as the
                    // header-marked arm below does -- the header is one the
                    // header arm would write through anyway (it carries the
                    // mark). Walk (2) never ages nor promotes it
                    // (`late_pinned_here` pins it), so this is its only aging.
                    let m = header.mark_word.load(Ordering::Relaxed);
                    let (word, new_age) = sweep_survivor_mark_by(m, age_step);
                    header.mark_word.store(word, Ordering::Relaxed);
                    if sweep_age_census {
                        let size = gen_object_total_size(header);
                        tenuring::add_to_age_table(&mut sweep_ages, new_age, size);
                    }
                } else if header_marked_here {
                    // A header mark left in place for the re-walk to clear.
                    header_marks_seen += 1;
                }
                if sweep_liveness_dbg {
                    young_survivors.push(obj_ptr as usize);
                }
                // Family-A fix follow-up: a side-marked survivor is kept in
                // place exactly like a header-marked one (same "never moves"
                // guarantee), so it needs the SAME watched-referent identity
                // mapping — a live Weak/Soft/Phantom reference watching this
                // address must still see it as "survived" post-GC. Before the
                // write-through fix, every candidate that reached this point
                // via a real root had `GC_FLAG_MARKED` set on its own header
                // and took the branch below; now ALL conservative-root
                // survivors (real objects included) take this side-marked
                // path instead, so the identity-map insert must move here too
                // (regression: `non_moving_sweep_records_identity_map_for_watched_survivor`).
                let addr = obj_ptr as usize;
                if is_watched(addr) {
                    if watchref_dbg() {
                        eprintln!(
                            "[watchref] non-moving sweep: side-marked watched survivor kept in place @0x{addr:x} — identity-mapped"
                        );
                    }
                    evac_map.insert(addr, addr);
                }
            } else if header.gc_flags() & GC_FLAG_MARKED != 0 {
                mw_hdr_marked += 1;
                header_marks_seen += 1;
                // Survivor: clear the mark, keep in place, and age it so the
                // next sweep can tenure it once it reaches PROMOTION_AGE
                // (selective promotion). Saturating so a long-lived pinned
                // object never wraps its age.
                // gcf f2/y: one load and one plain store of the folded word
                // (`sweep_survivor_mark`), not a locked `fetch_and` plus a
                // compare-exchange loop; and counted in the tenuring age
                // table, which used to miss header-marked survivors.
                {
                    let m = header.mark_word.load(Ordering::Relaxed);
                    let (word, new_age) = sweep_survivor_mark_by(m, age_step);
                    header.mark_word.store(word, Ordering::Relaxed);
                    if sweep_age_census {
                        let size = gen_object_total_size(header);
                        tenuring::add_to_age_table(&mut sweep_ages, new_age, size);
                    }
                }
                objects_live += 1;
                if sweep_liveness_dbg {
                    young_survivors.push(obj_ptr as usize);
                }
                // RandomizedContext WeakHashMap<Thread,...> fix (see
                // gc_quiescence::is_watched_referent): this survivor is kept
                // in place at its ORIGINAL address, so selective promotion
                // never gives it a `pointer_map` entry (nothing moved). If a
                // live Weak/Soft/Phantom reference is currently watching this
                // exact address, record an identity mapping so post-GC
                // reference processing's `is_marked` check recognizes it as
                // having survived instead of wrongly clearing the reference.
                let addr = obj_ptr as usize;
                if is_watched(addr) {
                    if watchref_dbg() {
                        eprintln!(
                            "[watchref] non-moving sweep: watched survivor kept in place @0x{addr:x} — identity-mapped"
                        );
                    }
                    evac_map.insert(addr, addr);
                }
            } else {
                if watchref_dbg() && is_watched(obj_ptr as usize) {
                    eprintln!(
                        "[watchref] non-moving sweep: watched address @0x{:x} was DEAD (unmarked) — zeroing",
                        obj_ptr as usize
                    );
                }
                // Dead: record the span for reclamation. Zeroing (so a later
                // conservative root scan cannot resurrect a stale header
                // inside the hole) and free-list publication are deferred to
                // the publication loop below, so a grid anomaly detected
                // later in the walk can still unwind this decision.
                //
                // gen r4/sweep (2026-09-23): `mw_dead_pushed` now counts every
                // object this arm condemns. It used to be bumped only when a
                // NEW span was opened after an existing one — never for the
                // first span, never for an object merged into its predecessor,
                // never under the forensic gates — while the forwarded arm
                // above bumped it only for the very first span. The MARKWHY
                // census that prints it exists to separate "classified
                // everything live" from "collected and unwound", and a count
                // that is 0 on a sweep that condemned one contiguous run
                // answers that question wrongly.
                mw_dead_pushed += 1;
                if retain_dead_objects {
                    dead_regions.push((
                        cursor,
                        total_size,
                        header.class_id.as_u32(),
                        ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)),
                        1,
                    ));
                } else {
                    push_dead(&mut dead_regions, cursor, total_size, header);                }            }
            cursor += total_size;        }
        if seq_size_memo.hits != 0 {
            self.young_premium_census[YP_MEMO_HITS]
                .fetch_add(seq_size_memo.hits, Ordering::Relaxed);
        }

        report_phase("sweep-walk");

        // gcf f2/y: adaptive tenuring for the NEXT cycle, from the whole age
        // table -- the selective walk's deferred bumps (side-marked
        // survivors) plus the header-marked survivors the walk above aged in
        // place. Moved here from the selective-promotion block, which ran it
        // before those were counted.
        if sweep_ages_filled && sweep_age_census {
            let eden = self.tenuring_eden_for(young_from.capacity()); // gcf f7/c
            let decision = if sweep_tenuring_engaged {
                self.tenuring.adapt(&sweep_ages, eden)
            } else {
                self.tenuring
                    .fixed_decision(eden, PROMOTION_AGE.saturating_sub(1))
            };
            if sweep_tenuring_print {
                for line in tenuring::distribution_lines(&decision, &sweep_ages) {
                    eprintln!("{line}");
                }
            }
            // gcf f4/r: the `gc,age` lines (`-Xlog:gc+age`).
            if self.tenuring.xlog_requested() {
                self.tenuring.stash_xlog(&decision, &sweep_ages);
            }
        }

        // H2-CID0: report the anchor-validity verdict. Reported here and not
        // behind a debug gate because the failure it describes is silent: a
        // chunk started off the object grid parses phantom objects, and an
        // unmarked phantom reclaims and ZEROES every live object it subsumes.
        // Bounded, and normally never emitted at all — on a sound anchor list
        // this counter is exactly zero. See [`AnchorGridProbe`] for the two
        // benign crossings it used to count as findings, which is why "exactly
        // zero" had not been true.
        let anchors_off_grid = anchor_probe.off_grid();
        if anchors_off_grid > 0 {
            let n = ANCHOR_OFF_GRID_REPORTS.fetch_add(1, Ordering::Relaxed);
            if n < 8 {
                tracing::warn!(
                    target: "cratonvm::gc::guard",
                    off_grid = anchors_off_grid,
                    anchors = sweep_anchors.len(),
                    used,
                    "young sweep: sweep anchor(s) are NOT on the object grid. The parallel \
                     sweep prefix starts an independent chain at each anchor and only \
                     re-proves it by landing on the next one, so a chunk beginning here can \
                     parse phantom objects and reclaim every live object they subsume.",
                );
            }
        }

        // Publish reclaimed regions to the arena's free list FIRST. Subsequent
        // `try_alloc_young_initialized` calls will satisfy allocations from
        // these holes before bumping the cursor — reclaiming memory without
        // moving a survivor.
        //
        // ORDER MATTERS (bintrees18 Bug B fix): this must run BEFORE
        // `clear_all_mark_bits_in_arena` below. The main sweep loop zeroed each
        // dead object in place but only *collected* the spans in `dead_regions`
        // — it had not yet added them to the free list. `clear_all_mark_bits_in_arena`
        // re-walks the whole from-space and skips only the regions on the free
        // list; if the just-zeroed dead spans are not yet published, it strides
        // INTO a zeroed (num_slots=0) span, decodes it as a 40-byte object, and
        // — when the span isn't a multiple of HEADER_SIZE — overshoots off the
        // object grid into a live Node's interior `Value::Object` field cell.
        // That produced the non-deterministic "inconsistent header
        // (class_id=4, array_length=1)" warnings on bintrees18 (a false
        // positive: the Nodes are valid; the *walk* desynced). The main sweep
        // loop never desynced because it knew each object's size before zeroing
        // it; publishing the holes first makes the re-walk hole-aware too.
        // A2 diag (CRATONVM_DBG_A2): does a NEW dead region this sweep overlap a
        // free block that was ALREADY free at sweep start? That is a double-free
        // (the walk reclaimed a region that was already on the free list — e.g. a
        // freed slot it walked as a phantom because the region wasn't skipped) or
        // an OVER-free (a too-large dead object whose span covers a free hole).
        // Either way it is the direct source of the overlapping free blocks the
        // coalescer then has to merge.
        if gc_flags().dbg_a2 {
            for &(doff, dsz, _, _, _) in &dead_regions {
                for &(foff, fsz) in &existing_free {
                    if doff < foff + fsz && foff < doff + dsz {
                        let n = A2_FL_OVERLAP_HITS.load(Ordering::Relaxed);
                        if n < 18 {
                            eprintln!(
                                "[A2-FL] DEAD-vs-EXISTING overlap: new dead [{}, {}) overlaps pre-existing free [{}, {})",
                                doff, doff + dsz, foff, foff + fsz,
                            );
                        }
                        A2_FL_OVERLAP_HITS.fetch_add(1, Ordering::Relaxed);
                        break;
                    }
                }
            }
        }
        // ---- CRATONVM_DBG_SWEEP_LIVENESS, young side ----
        //
        // The old-gen half of this assertion (see `old_gen_gc`) came back CLEAN
        // on the `--nojit` DefaultCatalogAndSchemaTest reproducer: SIGSEGV at
        // 94/132 with zero hits, so no marked old-gen object still pointed at a
        // block that sweep freed. But the corrupt receiver in that crash reads
        // `class_id=0 num_slots=0` — *zeroed* memory — and zeroing is what the
        // YOUNG sweep does to dead spans, not what `OldGen::free` does (it only
        // returns the block to a free list, leaving the bytes in place). So the
        // victim is a young object, and the question moves here.
        //
        // Checked from the OLD generation into the young dead spans, because
        // that is exactly the edge in the failing workload: a promoted bucket
        // array still pointing at a young chain node. A hit means the young mark
        // missed an old->young root — the remembered-set/card path — and the
        // sweep is about to zero a live node.
        //
        // Interior hits count: the spans are coalesced, so this asks "does a
        // live old-gen ref point INTO a doomed span", which also catches a
        // reference to a field cell rather than the object base.
        if gc_flags().dbg_sweep_liveness && !dead_regions.is_empty() {
            let base = from_base;
            let mut spans: Vec<(usize, usize)> = dead_regions
                .iter()
                .map(|&(off, sz, _, _, _)| (base + off, sz))
                .collect();
            spans.sort_unstable();
            let in_doomed = |addr: usize| -> bool {
                match spans.binary_search_by(|&(s, _)| s.cmp(&addr)) {
                    Ok(_) => true,
                    Err(0) => false,
                    Err(i) => {
                        let (s, sz) = spans[i - 1];
                        addr < s + sz
                    }
                }
            };
            let mut hits: Vec<(usize, u32, usize, usize)> = Vec::new();
            let mut old_scanned: usize = 0;
            {
                // Use the guard this function already holds (taken at the top,
                // alongside `young_from`). `parking_lot::Mutex` is NOT
                // reentrant, so re-locking `self.old_gen` here would deadlock
                // the collector at a safepoint — with every mutator thread
                // already stopped, i.e. a silent whole-VM hang rather than a
                // panic.
                for (obj_ptr, _size) in old_gen.walk_objects() {
                    old_scanned += 1;
                    // SAFETY: `walk_objects` yields valid old-gen object starts.
                    let h = unsafe { &*(obj_ptr as *const ObjectHeader) };
                    // SAFETY: a walked old-gen object has a valid header and an
                    // in-bounds body — `for_each_ref_slot`'s contract.
                    unsafe {
                        for_each_ref_slot(obj_ptr, h, |ref_ptr, slot| {
                            let victim = ref_ptr as usize;
                            if in_doomed(victim) && hits.len() < 64 {
                                hits.push((victim, h.class_id.as_u32(), obj_ptr as usize, slot));
                            }
                        });
                    }
                }
            }
            // YOUNG->YOUNG. Three prior runs of this assertion came back clean
            // (old->old, old->young, and the PIN-STALE canary), and the shape
            // that remains is a young referrer holding a young victim — e.g. a
            // bucket array and its chain nodes both allocated inside one test
            // method, never promoted. `young_survivors` is collected by the
            // walk itself, because the walk is the only thing that knows the
            // object grid; re-deriving it by a second linear scan is the exact
            // desync hazard `mark_young_to_old_refs` documents.
            let young_referrers = std::mem::take(&mut young_survivors);
            for &obj_ptr in &young_referrers {
                // SAFETY: these bases came from the sweep walk itself, which
                // only classifies spans it has parsed as valid object headers.
                let h = unsafe { &*(obj_ptr as *const ObjectHeader) };
                // SAFETY: a survivor has a valid header and an in-bounds body —
                // `for_each_ref_slot`'s contract.
                unsafe {
                    for_each_ref_slot(obj_ptr as *mut u8, h, |ref_ptr, slot| {
                        let victim = ref_ptr as usize;
                        if in_doomed(victim) && hits.len() < 64 {
                            hits.push((victim, h.class_id.as_u32(), obj_ptr, slot));
                        }
                    });
                }
            }
            // ---- ROOT -> doomed ----
            //
            // The heap-edge scan above cannot see a surviving reference that
            // lives in a ROOT (thread stack, JIT frame/register word, native
            // handle). That is where every remaining suspicion for the
            // reclaimed-live-object bug sits, and the young->young half is
            // vacuous outright whenever selective promotion moved all the
            // survivors out. `roots`/`finalizer_addrs` are the exact set the
            // mark phase was handed, so a hit here says the mark failed to
            // retain something it was given — the interior-pointer-to-base
            // mapping documented above being the prime candidate.
            let mut root_hits: Vec<(usize, bool)> = Vec::new();
            for r in roots.iter() {
                let a = r.as_ptr() as usize;
                if in_doomed(a) && root_hits.len() < 32 {
                    root_hits.push((a, false));
                }
            }
            for &a in finalizer_addrs.iter() {
                if in_doomed(a) && root_hits.len() < 32 {
                    root_hits.push((a, true));
                }
            }
            if !root_hits.is_empty() {
                eprintln!(
                    "[SWEEP-LIVENESS root] {} root(s) point into a span this sweep is about to zero ({} roots + {} finalizer addrs examined)",
                    root_hits.len(),
                    roots.len(),
                    finalizer_addrs.len(),
                );
                for (a, is_fin) in root_hits.iter().take(12) {
                    let addr = *a;
                    let covered = liveness_ranges
                        .partition_point(|&(st, _)| st <= addr)
                        .checked_sub(1)
                        .and_then(|i| liveness_ranges.get(i))
                        .filter(|&&(_, end)| addr < end)
                        .copied();
                    let in_verified = liveness_verified
                        .partition_point(|&(st, _)| st <= addr)
                        .checked_sub(1)
                        .is_some_and(|i| addr < liveness_verified[i].1);
                    // Which dead object did it land in, and is the root its
                    // BASE or an interior word?
                    let mut victim_desc = String::from("dead_obj=<none>");
                    for &(off, sz, cid, kind, _n) in &dead_regions {
                        let st = from_base + off;
                        if addr >= st && addr < st + sz {
                            victim_desc = format!(
                                "dead_obj=0x{st:x} size={sz} class_id={cid} kind={kind} interior_off={}",
                                addr - st
                            );
                            break;
                        }
                    }
                    // Was it marked? `mark_young` side-marks the resolved
                    // BASE, so ask the bitmap about both the base and the raw
                    // candidate, and read the header's own mark bit.
                    let base = covered.map(|(st, _)| st).unwrap_or(addr);
                    let side_base = side_bits.contains(base);
                    let side_raw = side_bits.contains(addr);
                    // SAFETY: `base` is 8-aligned inside mapped from-space —
                    // it came from the oracle's own object grid (or from a
                    // candidate `in_young` already bounds-checked).
                    // SAFETY: as above.
                    let bh = unsafe { &*(base as *const ObjectHeader) };
                    let hdr_marked = bh.gc_flags() & GC_FLAG_MARKED != 0;
                    let hdr_forwarded = bh.is_forwarded();
                    let hdr_fwd = bh.forwarding_address() as usize;
                    let hdr_cid = bh.class_id.as_u32();
                    let was_candidate = conservative_candidates.binary_search(&addr).is_ok();
                    eprintln!(
                        "[SWEEP-LIVENESS root]   victim=0x{addr:x} source={} covered={} in_verified_span={} side_marked_base={side_base} side_marked_raw={side_raw} header_marked={hdr_marked} forwarded={hdr_forwarded} fwd=0x{hdr_fwd:x} base_class_id={hdr_cid} was_candidate={was_candidate} {victim_desc}",
                        if *is_fin { "finalizer" } else { "root" },
                        match covered {
                            Some((st, en)) => format!("0x{st:x}..0x{en:x}"),
                            None => "none".to_string(),
                        },
                        in_verified,
                    );
                }
            }

            // Report on EVERY sweep, not only on a hit: otherwise a clean
            // campaign is indistinguishable from one where this block never
            // ran at all (no dead spans, flag not honoured, or the workload
            // never took the non-moving path). `hits=0` with a nonzero
            // `scanned` is the only form of "clean" worth quoting.
            eprintln!(
                "[SWEEP-LIVENESS young] cycle={} hits={} root_hits={} doomed_spans={} old_gen_scanned={} young_survivors_scanned={} roots={} finalizer_addrs={}",
                sweep_zero_cycle,
                hits.len(),
                root_hits.len(),
                spans.len(),
                old_scanned,
                young_referrers.len(),
                roots.len(),
                finalizer_addrs.len(),
            );
            if !hits.is_empty() {
                eprintln!(
                    "[SWEEP-LIVENESS young] {} live ref(s) point into {} young span(s) this sweep is about to zero ({} old-gen + {} young survivors scanned)",
                    hits.len(),
                    spans.len(),
                    old_scanned,
                    young_referrers.len(),
                );
                for (victim, cid, referrer, slot) in hits.iter().take(12) {
                    eprintln!(
                        "[SWEEP-LIVENESS young]   victim=0x{victim:x} <- referrer=0x{referrer:x} class_id={cid} slot={slot}",
                    );
                }
            }
        }

        // MARKWHY: where did the walk actually END? If the watched offset is
        // above this, the walk abandoned before reaching it and the span was
        // retained by the "skipped stretch retained until a moving cycle
        // resets from-space" arm rather than by any skip list.
        {
            let w = cratonvm_gc_core::heap::young_mark_watch();
            if w != 0 && w >= from_base {
                eprintln!(
                    // NOT `objects_swept`: its only increment is in the
                    // publication loop several hundred lines below, so reading
                    // it here prints a structural zero on every run and every
                    // workload. It did exactly that, and the zero was read as
                    // "this sweep reclaimed NOTHING" — a whole-VM reclamation
                    // failure inferred from a counter not yet written. The real
                    // total is on the `sweep done` line after the loop.
                    "[MARKWHY] walk ended: cursor={cursor:#x} used={used:#x} watch_off={:#x} objects_live={objects_live} dead_regions={} par_prefix_end={par_prefix_end:#x} watch_in_par_prefix={}",
                    w - from_base,
                    dead_regions.len(),
                    (w - from_base) < par_prefix_end,
                );
                eprintln!(
                    "[MARKWHY] disposition census: side_marked={mw_side} late_pinned={mw_late} forwarded={mw_fwd} header_marked={mw_hdr_marked} dead_pushed={mw_dead_pushed} unwinds={mw_unwinds} sites={mw_site:?} unwound_entries={mw_unwound_entries} dead_regions_final={} side_sorted={} late_pins={}",
                    dead_regions.len(),
                    side_sorted.len(),
                    late_pins.len(),
                );
            }
        }

        // ----- Invariant: no LIVE object inside a reclaimed span (H2-CID0) ---
        //
        // Since the Family-A hardening every mark lives in the side channel, so
        // `side_sorted` IS the complete live set, and `dead_regions` is every
        // span the walk decided to reclaim. The two must be disjoint. They can
        // overlap only if the walk left the object grid: a mis-sized header
        // subsumes its neighbours, and the phantom's own (unmarked) disposition
        // then condemns every live object inside it.
        //
        // That failure is silent today. The span is zeroed before publication,
        // so the next read through a still-live reference sees an all-zero
        // header — `ClassId(0)`, which renders as `java.lang.Object` and
        // surfaces minutes later on another thread as
        // `java.lang.Object cannot be cast to <something>`
        // (old-sweep-liveness.md §7).
        // Every existing guard stays quiet: the header is plausible, the extent
        // fits, no slot goes out of bounds, nothing segfaults.
        //
        // Unconditional, because a check you have to enable is a check that is
        // off in every bug report. It costs one merge pass over two lists that
        // are already built and already sorted.
        //
        // A span that fails is RETAINED rather than freed — over-retention is
        // always safe under this sweep, and it converts a silent
        // use-after-free into a bounded leak plus a loud report.
        if !dead_regions.is_empty() && !side_sorted.is_empty() {
            let mut si = 0usize;
            let mut violations = 0usize;
            let mut first: Option<(usize, usize, u32)> = None;
            dead_regions.retain(|&(off, sz, cid, _, _)| {
                let start = from_base + off;
                let end = start + sz;
                while si < side_sorted.len() && side_sorted[si] < start {
                    si += 1;
                }
                let mut live_inside = false;
                let mut k = si;
                while k < side_sorted.len() && side_sorted[k] < end {
                    // An evacuated source is marked AND dead by construction:
                    // selective promotion only ever evacuates objects the mark
                    // just proved live, and the copy is already in old gen.
                    // SAFETY: `side_sorted[k]` is an 8-aligned address inside
                    // the live from-space region and nothing has been zeroed
                    // yet, so its header is still readable.
                    let h = unsafe { &*(side_sorted[k] as *const ObjectHeader) };
                    if !h.is_forwarded() {
                        live_inside = true;
                        if first.is_none() {
                            first = Some((side_sorted[k], sz, cid));
                        }
                        break;
                    }
                    k += 1;
                }
                if live_inside {
                    violations += 1;
                    return false;
                }
                true
            });
            if violations > 0 {
                LIVE_IN_DEAD_SPANS.fetch_add(violations as u64, Ordering::Relaxed);
                let n = LIVE_IN_DEAD_REPORTS.fetch_add(1, Ordering::Relaxed);
                if n < 8 {
                    let (victim, sz, cid) = first.unwrap_or((0, 0, 0));
                    tracing::error!(
                        target: "cratonvm::gc::guard",
                        spans = violations,
                        victim = format!("{victim:#x}"),
                        span_bytes = sz,
                        span_head_class_id = cid,
                        "young non-moving sweep was about to ZERO a span containing a LIVE \
                         (marked) object — the walk left the object grid and a phantom header \
                         subsumed it. The span has been RETAINED instead of freed. Without \
                         this check the object's header would now read all-zero and the next \
                         use of it would fail as `java.lang.Object cannot be cast to ...`.",
                    );
                }
            }
        }

        // ----- Invariant: no ROOT points into a reclaimed span --------------
        //
        // H2-CID0-BLOCKED (2026-08-02). The invariant above compares the dead
        // spans against `side_sorted`, the MARK's own live set, so it can only
        // catch a walk that left the object grid. It is blind to the failure
        // this family is actually made of: the mark was handed a root, did not
        // retain what the root points at, and the sweep now reclaims it. From
        // the sweep's point of view that object is simply unmarked, and every
        // guard stays quiet.
        //
        // `roots` + `finalizer_addrs` are the exact set the mark phase was
        // given at the top of this function, so this question is decidable
        // right here and it is not an approximation: if one of them points
        // into a span about to be zeroed, marking dropped it. There is no
        // benign reading — even a conservative false positive is supposed to
        // OVER-retain, which is the collector's own stated invariant ("a
        // conservative false-positive root only over-retains, it can never
        // cause a live object to be freed").
        //
        // So the span is RETAINED rather than freed, exactly as the invariant
        // above does: over-retention is always safe under this sweep, and it
        // converts a silent use-after-free — read back minutes later on
        // another thread as `NoSuchMethodError java/lang/Object.hasNext()Z`,
        // `CloneNotSupportedException`, `java.lang.Object cannot be cast to X`
        // or a bare SIGSEGV — into a bounded leak plus a report that names the
        // reclaimed object's CLASS, which no reader-side guard can recover
        // once the span has been zeroed.
        //
        // Unconditional: one sort of the young-resident roots plus one merge
        // pass over two ascending lists. `CRATONVM_DBG_SWEEP_LIVENESS` prints
        // the same finding with the oracle's internals attached, but a check
        // you have to enable is a check that is off in every bug report — and
        // this family has been chased across three sessions on runs that were
        // not armed.
        if !dead_regions.is_empty() {
            let mut root_addrs: Vec<usize> = roots
                .iter()
                .map(|r| r.as_ptr() as usize)
                .chain(finalizer_addrs.iter().copied())
                .filter(|&a| a >= from_base && a < from_end)
                .collect();
            root_addrs.sort_unstable();
            root_addrs.dedup();
            if !root_addrs.is_empty() {
                let mut ri = 0usize;
                let mut violations = 0usize;
                let mut evacuated = 0usize;
                let mut first_dropped = false;
                let mut first_unresolved = false;
                let mut first_late_pinned = false;
                let mut first_root_side_marked = false;
                let mut first_resolved_base = 0usize;
                let mut first_resolved_base_side_marked = false;
                let mut first_span_objects = 0usize;
                let mut first: Option<(usize, usize, usize, u32, u8, bool)> = None;
                // The base of the object a root points into, as the mark's own
                // oracle resolves it. A dead span is a RUN of consecutive dead
                // objects carrying the FIRST one's header, so the span head is
                // frequently a DIFFERENT object than the root's — and the two
                // can disagree about the only question that matters here.
                let base_of = |a: usize, start: usize, end: usize| -> usize {
                    young_object_ranges
                        .partition_point(|(st, _)| *st <= a)
                        .checked_sub(1)
                        .and_then(|i| young_object_ranges.get(i))
                        .filter(|&&(b, e)| a < e && b >= start && b < end)
                        .map_or(start, |&(b, _)| b)
                };
                dead_regions.retain(|&(off, sz, cid, kind, nobj)| {
                    let start = from_base + off;
                    let end = start + sz;
                    while ri < root_addrs.len() && root_addrs[ri] < start {
                        ri += 1;
                    }
                    let mut k = ri;
                    let mut violation: Option<(usize, usize)> = None;
                    while k < root_addrs.len() && root_addrs[k] < end {
                        let a = root_addrs[k];
                        let base = base_of(a, start, end);
                        // An EVACUATED source is dead by construction and is
                        // not a violation: selective promotion only evacuates
                        // objects the mark just proved live, the copy is
                        // already in old gen, and the root still pointing at
                        // the young source is either a precise one the pointer
                        // map rewrites or a shadow-published movable JIT root
                        // whose owner reloads it. The same exclusion the
                        // `side_sorted` invariant above makes, for the same
                        // reason — without it this fires on every promoted
                        // root, retains its source, and the young generation
                        // stops draining (the GC-overhead-on-a-half-full-heap
                        // failure).
                        //
                        // It must be asked of the ROOT'S object, not of the
                        // span head. Measured 2026-08-02: every hit under
                        // `CRATONVM_DBG_GC_STRESS` was a promoted object
                        // sharing a dead run with an unpromoted neighbour, and
                        // reading the run's head header called all of them
                        // violations.
                        //
                        // SAFETY: `base` is an 8-aligned young-from-space
                        // object base — either from the oracle's own object
                        // grid or the span start this sweep's walk produced —
                        // and nothing has been zeroed yet.
                        let h = unsafe { &*(base as *const ObjectHeader) };
                        if h.is_forwarded() {
                            evacuated += 1;
                            k += 1;
                            continue;
                        }
                        violation = Some((a, base));
                        break;
                    }
                    if let Some((a, base)) = violation {
                        violations += 1;
                        if first.is_none() {
                            first = Some((a, start, sz, cid, kind, side_bits.contains(start)));
                            first_dropped = dropped_snapshot.binary_search(&a).is_ok();
                            first_unresolved = unresolved_snapshot.binary_search(&a).is_ok();
                            first_late_pinned = late_pins.binary_search(&a).is_ok();
                            first_root_side_marked = side_bits.contains(a);
                            first_span_objects = nobj;
                            first_resolved_base = base;
                            first_resolved_base_side_marked = side_bits.contains(base);
                        }
                        return false;
                    }
                    true
                });
                if evacuated > 0 {
                    ROOT_IN_EVACUATED_SPANS.fetch_add(evacuated as u64, Ordering::Relaxed);
                }
                if violations > 0 {
                    ROOT_IN_DEAD_SPANS.fetch_add(violations as u64, Ordering::Relaxed);
                    let n = ROOT_IN_DEAD_REPORTS.fetch_add(1, Ordering::Relaxed);
                    if n < 8 {
                        let (root, start, sz, cid, kind, side) =
                            first.unwrap_or((0, 0, 0, 0, 0, false));
                        tracing::error!(
                            target: "cratonvm::gc::guard",
                            spans = violations,
                            root = format!("{root:#x}"),
                            span = format!("{start:#x}+{sz:#x}"),
                            interior_off = root.wrapping_sub(start),
                            span_head_class_id = cid,
                            span_head_kind = kind,
                            span_head_side_marked = side,
                            // Which arm of the conservative-root oracle let it
                            // through. `oracle_dropped_as_gap` means
                            // `mark_young`'s verified-span branch concluded the
                            // address was free/gap space and returned without
                            // marking anything — and, unlike the unresolved
                            // arm, without handing it to the late-resolution
                            // pass either.
                            oracle_dropped_as_gap = first_dropped,
                            oracle_left_unresolved = first_unresolved,
                            late_pinned = first_late_pinned,
                            oracle_dropped_total = oracle_dropped.get(),
                            root_side_marked = first_root_side_marked,
                            span_objects = first_span_objects,
                            oracle_resolved_base = format!("{first_resolved_base:#x}"),
                            oracle_resolved_base_side_marked = first_resolved_base_side_marked,
                            sweep_cycle = sweep_zero_cycle,
                            roots = roots.len(),
                            "young non-moving sweep was about to zero a span a ROOT still \
                             points into — the mark phase was handed this address and did not \
                             retain what it points at. The span has been RETAINED instead of \
                             freed. `span_head_class_id` is the class of the object at the \
                             head of the span; a non-zero `interior_off` means the root is an \
                             interior word rather than the object base.",
                        );
                    }
                }
            }
        }

        // Coalesce the newly-dead spans while they are still in walk order.
        // Publishing every object-sized hole first and sorting the arena free
        // list afterward made a bintrees18 collection allocate, sort, and merge
        // millions of entries even though nearly all of them are adjacent.
        // Keep `dead_regions` intact for the per-object diagnostics below, but
        // zero and publish only maximal contiguous spans.
        let mut reclaimed_regions: Vec<(usize, usize)> =
            Vec::with_capacity(dead_regions.len().min(1024));
        for &(off, sz, _, _, _) in &dead_regions {
            if let Some(last) = reclaimed_regions.last_mut() {
                let last_end = last.0 + last.1;
                if off <= last_end {
                    last.1 = last_end.max(off + sz) - last.0;
                    continue;
                }
            }
            reclaimed_regions.push((off, sz));
        }

        // ----- Invariant: a reclaimed span is not ALREADY free (H2-CID0) -----
        //
        // `existing_free` is the free list as it stood at sweep start (merged
        // with the reserved-TLAB-tail skips); `reclaimed_regions` is what this
        // sweep is about to add. An overlap means the same bytes end up on the
        // free list twice, so the allocator can serve them to two objects — and
        // the second allocation ZEROES the block before writing its header,
        // which lands the same all-zero-header face on a still-referenced
        // object as a premature sweep does.
        //
        // This condition already had a diagnostic (`CRATONVM_DBG_A2`'s
        // `DEAD-vs-EXISTING overlap`) but only behind a gate, and as an
        // O(dead x free) double loop. Both lists are sorted and disjoint, so
        // one merge pass answers it unconditionally; on a hit the span is
        // dropped rather than published (over-retention, the safe direction).
        if !reclaimed_regions.is_empty() && !existing_free.is_empty() {
            let mut fi = 0usize;
            let mut overlaps = 0usize;
            let mut first: Option<(usize, usize)> = None;
            reclaimed_regions.retain(|&(off, sz)| {
                while fi < existing_free.len() && existing_free[fi].0 + existing_free[fi].1 <= off {
                    fi += 1;
                }
                if fi < existing_free.len() && existing_free[fi].0 < off + sz {
                    overlaps += 1;
                    if first.is_none() {
                        first = Some((off, sz));
                    }
                    return false;
                }
                true
            });
            if overlaps > 0 {
                DOUBLE_FREE_SPANS.fetch_add(overlaps as u64, Ordering::Relaxed);
                let n = DOUBLE_FREE_REPORTS.fetch_add(1, Ordering::Relaxed);
                if n < 8 {
                    let (off, sz) = first.unwrap_or((0, 0));
                    tracing::error!(
                        target: "cratonvm::gc::guard",
                        spans = overlaps,
                        first_span = format!("{:#x}+{sz:#x}", from_base + off),
                        "young non-moving sweep was about to publish a span that is ALREADY on \
                         the free list — a double free. The allocator would serve the same \
                         bytes twice and zero them under a live object. Span dropped.",
                    );
                }
                // gen r4/sweep (2026-09-23): drop the per-object records the
                // dropped spans were built from, too. The publication loop
                // below counts `bytes_swept` / `objects_swept` (→ this cycle's
                // `bytes_freed` and `live_bytes`), feeds `record_swept` and
                // `a2dbg::record_free`, and every later diagnostic reads
                // `dead_regions` as "doomed" — all from `dead_regions`, not
                // `reclaimed_regions`. Leaving them in claimed bytes freed that
                // were never zeroed or published, and told the forensic rings
                // that live-or-already-free memory had just been swept. Every
                // `dead_regions` entry lies inside exactly one coalesced
                // region (the coalescer is their union) and both lists are
                // ascending, so one merge pass keeps exactly the survivors'.
                let mut ri = 0usize;
                dead_regions.retain(|&(off, sz, _, _, _)| {
                    while ri < reclaimed_regions.len()
                        && reclaimed_regions[ri].0 + reclaimed_regions[ri].1 <= off
                    {
                        ri += 1;
                    }
                    ri < reclaimed_regions.len()
                        && reclaimed_regions[ri].0 <= off
                        && off + sz <= reclaimed_regions[ri].0 + reclaimed_regions[ri].1
                });
            }
        }

        // DoHead walk-desync hardening (2026-07-02): zeroing was deferred from
        // the walk's disposition arms to here so that anomaly-triggered
        // unwinding (dead_regions.truncate above) never has to un-zero.
        // Everything surviving in `dead_regions` was collected on a verified
        // stretch of the walk grid. Zero each span (so a later conservative
        // root scan cannot resurrect a stale header inside the hole) and
        // publish it to the free list. Per-object forensic records remain
        // available without forcing per-object arena publication.
        let defer_reclamation = gc_flags().dbg_no_nonmoving_reclaim;
        // Wave 6 (`CRATONVM_GEN_SWEEP_LAZY_ZERO`): see the zeroing below.
        let mut sweep_lazy_zero = self.sweep_lazy_zero;
        if !defer_reclamation {
            for &(off, sz, class_id, kind_byte, object_count) in &dead_regions {
                let obj_addr = from_base + off;
                record_swept(obj_addr, class_id, kind_byte, sweep_zero_cycle);
                cratonvm_gc_core::a2dbg::record_free(obj_addr);
                bytes_swept += sz;
                objects_swept += object_count;
            }
            // perf/parallel-young-gc: zero the reclaimed spans on the same
            // worker count as the mark. `reclaimed_regions` is the coalesced,
            // ascending, pairwise-DISJOINT union of the dead spans the
            // verified walk collected, so the workers touch strictly separate
            // memory and need no synchronisation at all. Publication to the
            // arena free list stays on this thread (it mutates `young_from`).
            // JIT round 23 wave 6, lane L4 (`CRATONVM_GEN_SWEEP_LAZY_ZERO`,
            // default ON, `0` restores the memset): the sweep leaves its
            // reclaimed spans dirty, exactly as the pinned in-place cycle does
            // (`finish_in_place_young_cycle`, `CRATONVM_GEN_IN_PLACE_LAZY_ZERO`
            // with the dead-span mirror): they go to the arena's lazy-dirty set
            // and its exact mirror (r23 w4 review R-L4z), so
            // * every hand-out overlapping one is memset in full at the two
            //   young doors (`take_lazy_dirty`), on the mutator, inside the
            //   TLAB it fills -- HotSpot's ZeroTLAB shape -- instead of the
            //   whole dead eden being written to memory inside the pause and
            //   read back by the allocation;
            // * `object_start` (`is_object_address` / `is_object_start`) and
            //   the mark's fallback arm decline any address inside one before
            //   reading a byte, so no corpse header is ever accepted;
            // * every young walker skips them (they are free blocks: each
            //   reclaimed span is whole dead objects, >= HEADER_SIZE);
            // * the known-zero claim below is not made for them.
            // Measured shape (n23-w5, R21AllocLean lean-static-long16, the
            // same machinery on in-place cycles): zero-and-publish 12 of the
            // 27 ms sweep pause. Declines to the memset when the arena has no
            // mirror (no capacity).
            if sweep_lazy_zero {
                young_from.note_lazy_dirty(&reclaimed_regions);
                if young_from.lazy_dirty_mirror().is_none() {
                    sweep_lazy_zero = false;
                } else {
                    // The first lazy sweep CREATES the mirror; publish it now,
                    // not only at the next bounds publish.
                    self.store_lazy_dirty_mirror_from_slot(young_from.lazy_dirty_mirror());
                }
            }
            if !sweep_lazy_zero {
                // SAFETY: every span is inside the live from-space region (the
                // walk bounded each `dead_regions` entry by `used`) and the spans
                // are pairwise disjoint after coalescing.
                unsafe {
                    cratonvm_gc_core::young_mark::zero_spans_parallel_on(
                        from_base,
                        &reclaimed_regions,
                        mark_threads,
                        // gce e1/y: the mark's pool (same width; no dispatch is
                        // open here).
                        mark_pool,
                    )
                };
            }
            // H2-CID0: the unconditional, lock-free record of every reclaimed
            // span, so a later wild receiver can be attributed to a young span
            // without the run having been armed in advance. gcd d4/m: in ONE
            // batch (`record_young_spans_freed`, gcd d1/e's form for the pinned
            // rebuild): the per-span form paid a process-wide `fetch_add` and a
            // pause-ledger coverage read per span, and on a JIT-warm program a
            // sweep publishes one span per hole between scattered survivors --
            // ~10^5 per cycle on `GenR4W4EvacThroughputProbe`. Same ring
            // contents (same `seq`, fields and write order per record).
            record_young_spans_freed(from_base, &reclaimed_regions, sweep_zero_cycle as u64);
            for &(off, sz) in &reclaimed_regions {
                let obj_addr = from_base + off;
                // stw-residual-close forensics: site 1 = non-moving sweep
                // dead-span zero+freelist, tagged with the sweep cycle.
                cratonvm_gc_core::zero_forensics::record(1, sweep_zero_cycle, obj_addr, sz);
                young_from.add_free_block(off, sz);
            }
        } else if !dead_regions.is_empty() {
            tracing::debug!(
                spans = dead_regions.len(),
                bytes = reclaimed_regions
                    .iter()
                    .map(|(_, size)| *size)
                    .sum::<usize>(),
                "GC: retaining dead young spans during non-moving reclamation probe"
            );
        }
        report_phase("zero-and-publish");

        // MARKWHY: what this sweep ACTUALLY reclaimed, read after the only
        // place that writes it. Companion to the note on `walk ended` above.
        {
            let w = cratonvm_gc_core::heap::young_mark_watch();
            if w != 0 && w >= from_base {
                eprintln!(
                    "[MARKWHY] sweep done: objects_swept={objects_swept} bytes_swept={bytes_swept} dead_regions={} reclaimed_regions={} deferred={defer_reclamation}",
                    dead_regions.len(),
                    reclaimed_regions.len(),
                );
            }
        }

        // DBG (CRATONVM_DBG_SWEEP_CENSUS): per-cycle census of what this sweep
        // reclaimed, by class. A continuously-live workload class (e.g. the
        // RRWL probe's ThreadLocalMap$Entry / HoldCounter chain) showing up
        // here names the wrongly-swept set directly at reclamation time —
        // no use-time face (zero-header receiver / OOB read) required.
        if gc_flags().dbg_sweep_census && !dead_regions.is_empty() {
            let mut counts: std::collections::HashMap<u32, (usize, usize)> =
                std::collections::HashMap::new();
            for &(off, sz, class_id, _kind, object_count) in &dead_regions {
                let e = counts.entry(class_id).or_insert((0, from_base + off));
                e.0 += object_count;
                let _ = sz;
            }
            let mut v: Vec<(u32, usize, usize)> =
                counts.into_iter().map(|(c, (n, a))| (c, n, a)).collect();
            v.sort_by(|a, b| b.1.cmp(&a.1));
            let mut line = format!(
                "[sweep-census] cycle={} swept={} classes={}:",
                sweep_zero_cycle,
                dead_regions.len(),
                v.len()
            );
            for (cid, n, first_addr) in v.iter().take(14) {
                let name = cratonvm_gc_core::gc::resolve_class_info(*cid)
                    .map(|(n, _)| n)
                    .unwrap_or_else(|| format!("cid{cid:#x}"));
                line.push_str(&format!(" {name}x{n}@{first_addr:#x}"));
            }
            eprintln!("{line}");

            // Holder scan: for each swept victim, brute-scan young from-space
            // and old gen for any aligned 8-byte word holding the victim's
            // address, plus any legacy 16-byte Value cell whose payload is the
            // victim. The mark said "unreachable"; if a live holder still
            // points at the victim, this names the EXACT edge the mark missed
            // — the decisive datum for the live-reclaim investigation.
            for &(doff, _dsz, dcid, _dk, _) in dead_regions.iter().take(6) {
                let victim = from_base + doff;
                let vname = cratonvm_gc_core::gc::resolve_class_info(dcid)
                    .map(|(n, _)| n)
                    .unwrap_or_else(|| format!("cid{dcid:#x}"));
                let mut holders = 0usize;
                // Young from-space scan (skip the victim's own span).
                let mut w = from_base;
                let from_hi = from_base + young_from.used();
                while w + 8 <= from_hi {
                    // SAFETY: `[from_base, from_hi)` is mapped arena memory, w is 8-aligned.
                    let word = unsafe { *(w as *const u64) } as usize;
                    if word == victim && !(w >= victim && w < victim + _dsz) {
                        holders += 1;
                        if holders <= 4 {
                            eprintln!(
                                "[sweep-census]   victim {vname}@{victim:#x}: young holder word @{w:#x} (holder_off={:#x})",
                                w - from_base,
                            );
                        }
                    }
                    w += 8;
                }
                // Old-gen scan via object walk (bounded per object by its size).
                for (optr, osz) in old_gen.walk_objects() {
                    let lo = optr as usize;
                    let mut w = lo + HEADER_SIZE;
                    let hi = lo + osz;
                    while w + 8 <= hi {
                        // SAFETY: `[lo, hi)` is a live old-gen object's span.
                        let word = unsafe { *(w as *const u64) } as usize;
                        if word == victim {
                            holders += 1;
                            if holders <= 8 {
                                // SAFETY: `optr` is a live old-gen object header.
                                let ocid =
                                    unsafe { (*(optr as *const ObjectHeader)).class_id.as_u32() };
                                let oname = cratonvm_gc_core::gc::resolve_class_info(ocid)
                                    .map(|(n, _)| n)
                                    .unwrap_or_else(|| format!("cid{ocid:#x}"));
                                eprintln!(
                                    "[sweep-census]   victim {vname}@{victim:#x}: OLD holder {oname}@{lo:#x}+{:#x}",
                                    w - lo,
                                );
                            }
                        }
                        w += 8;
                    }
                }
                // Was the victim in the ROOT SET (incl. every thread's folded
                // snapshot)? A root-listed victim that still got swept means
                // `mark_young` dropped it (validator/walk bug); an unlisted
                // one means the upstream root/snapshot coverage missed it.
                let in_roots = roots.iter().any(|r| r.as_ptr() as usize == victim);
                if holders == 0 && !in_roots {
                    eprintln!(
                        "[sweep-census]   victim {vname}@{victim:#x}: NO heap holder, NOT in roots — register/native-side ref only, or truly dead",
                    );
                } else if in_roots {
                    eprintln!(
                        "[sweep-census]   victim {vname}@{victim:#x}: WAS IN ROOTS ({holders} heap holder(s)) — mark_young dropped a live root!",
                    );
                } else {
                    eprintln!(
                        "[sweep-census]   victim {vname}@{victim:#x}: {holders} heap holder(s) — MARK MISSED A HEAP EDGE",
                    );
                }
            }

            // INVERTED holder scan: every victim, in ONE pass over the heap.
            //
            // The per-victim scan above is O(victims x heap) and is therefore
            // bounded to six. On a cycle that sweeps 20,000 objects that is a
            // 0.03% sample, and it is the wrong 0.03%: it takes the six lowest
            // addresses, which are whatever the arena happens to open with.
            // Chasing `io.netty.util.internal.ObjectCleanerTest` on
            // 2026-09-05 it reported "NO heap holder, NOT in roots" six times
            // per cycle and never once looked at the object the run then died
            // on — a lambda held by a live `java.util.Collections
            // $ReverseComparator2`, which the very next use read back with an
            // all-zero header.
            //
            // Turned inside out it is O(heap log victims) for ALL of them:
            // build the dead SPANS once, walk young from-space and old gen
            // once, and binary-search each word. Reports only victims that DO
            // have a holder, because a swept object with a live holder is the
            // defect and a swept object without one is the sweep working.
            //
            // Spans, not addresses, so an INTERIOR pointer counts: the earlier
            // pass compared `word == victim` and would have missed a holder
            // that points at a field rather than the header.
            {
                let mut spans: Vec<(usize, usize, u32)> = dead_regions
                    .iter()
                    .map(|&(off, sz, cid, _k, _n)| (from_base + off, from_base + off + sz, cid))
                    .collect();
                spans.sort_unstable_by_key(|&(start, _, _)| start);
                let victim_span = |addr: usize| -> Option<(usize, u32)> {
                    spans
                        .partition_point(|&(start, _, _)| start <= addr)
                        .checked_sub(1)
                        .and_then(|i| spans.get(i))
                        .filter(|&&(_, end, _)| addr < end)
                        .map(|&(start, _, cid)| (start, cid))
                };
                let mut found = 0usize;
                let mut reported = 0usize;
                let mut report = |holder_desc: &dyn Fn() -> String,
                                  slot: usize,
                                  target: usize,
                                  found: &mut usize,
                                  reported: &mut usize| {
                    if let Some((vstart, vcid)) = victim_span(target) {
                        *found += 1;
                        if *reported < 16 {
                            *reported += 1;
                            let vname = cratonvm_gc_core::gc::resolve_class_info(vcid)
                                .map(|(n, _)| n)
                                .unwrap_or_else(|| format!("cid{vcid:#x}"));
                            eprintln!(
                                "[sweep-census]   LIVE HOLDER -> DOOMED: {} slot@{slot:#x} \
                                 -> {vname}@{target:#x} (span head {vstart:#x})",
                                holder_desc()
                            );
                        }
                    }
                };
                // Young from-space, walked as OBJECTS rather than as a flat
                // word array. A bare word scan names an address and nothing
                // else; the question is which OBJECT still points at a doomed
                // one, and the answer is only actionable with its class and
                // the slot offset. Survivorship is decided by the dead-span
                // set itself — an object inside one is doomed, so a reference
                // out of it is not evidence — which needs no mark bits and
                // cannot disagree with what this sweep is about to free.
                let from_hi = from_base + young_from.used();
                let existing_free_inv = merge_skips(young_from.free_blocks_sorted());
                let used_inv = young_from.used();
                // gen r4w2/youngmark: the shared `YoungGridCursor` — this walk
                // used to `break` at the first GAP-filler sentinel and then
                // report "0 references" for an arena it had walked a prefix of.
                let mut grid =
                    YoungGridCursor::new(from_base, used_inv, &existing_free_inv, &side_sorted);
                for step in grid.by_ref() {
                    let GridStep::Object { off: c, size: tot } = step else {
                        continue;
                    };
                    let optr = from_base + c;
                    // SAFETY: the cursor yields only parsed object starts
                    // inside the mapped from-space.
                    let h = unsafe { &*(optr as *const ObjectHeader) };
                    if victim_span(optr).is_none() {
                        let hcid = h.class_id.as_u32();
                        let mut w = optr + HEADER_SIZE;
                        let hi = (optr + tot).min(from_hi);
                        while w + 8 <= hi {
                            // SAFETY: `[optr, optr + tot)` is this object's own
                            // span inside the mapped arena, and `w` is 8-aligned.
                            let word = unsafe { *(w as *const u64) } as usize;
                            let off = w - optr;
                            report(
                                &|| {
                                    let hname = cratonvm_gc_core::gc::resolve_class_info(hcid)
                                        .map(|(n, _)| n)
                                        .unwrap_or_else(|| format!("cid{hcid:#x}"));
                                    format!(
                                        "YOUNG {hname}@{optr:#x} (kind={} slots={} alen={})",
                                        ObjectHeader::kind_tag(h.mark_word.load(Ordering::Relaxed)),
                                        h.num_slots(),
                                        h.array_length(),
                                    )
                                },
                                off,
                                word,
                                &mut found,
                                &mut reported,
                            );
                            w += 8;
                        }
                    }
                }
                let (young_anomalies, young_truncated) = (grid.anomalies, grid.truncated);
                // Old gen: walk objects so the holder can be NAMED.
                for (optr, osz) in old_gen.walk_objects() {
                    let lo = optr as usize;
                    let mut w = lo + HEADER_SIZE;
                    let hi = lo + osz;
                    while w + 8 <= hi {
                        // SAFETY: `[lo, hi)` is a live old-gen object's span.
                        let word = unsafe { *(w as *const u64) } as usize;
                        let off = w - lo;
                        report(
                            &|| {
                                // SAFETY: `optr` is a live old-gen object header.
                                let ocid =
                                    unsafe { (*(optr as *const ObjectHeader)).class_id.as_u32() };
                                let oname = cratonvm_gc_core::gc::resolve_class_info(ocid)
                                    .map(|(n, _)| n)
                                    .unwrap_or_else(|| format!("cid{ocid:#x}"));
                                format!("OLD {oname}@{lo:#x}")
                            },
                            off,
                            word,
                            &mut found,
                            &mut reported,
                        );
                        w += 8;
                    }
                }
                eprintln!(
                    "[sweep-census] cycle={sweep_zero_cycle} INVERTED holder scan: \
                     {found} reference(s) from a SURVIVING object into a span this sweep \
                     is about to free (0 is the only clean answer; young walk \
                     anomalies={young_anomalies} truncated={young_truncated} — a truncated \
                     walk makes the count a LOWER BOUND)"
                );
            }

            // TARGETED CLASS TRACE (`CRATONVM_DBG_SWEEP_TRACE_CLASS=<internal/Name>`).
            //
            // The scans above answer aggregate questions -- "does ANY surviving
            // object point at a doomed one" -- and when the answer is 0 they
            // leave the next question unanswerable: was the object I expected
            // to be the holder even THERE, in which generation, marked or not,
            // and what did its slots actually contain at this instant?
            //
            // Chasing `ObjectCleanerTest` that gap cost a whole session. The
            // victim is a lambda held by `java/util/Collections$ReverseComparator2
            // .cmp`; the inverted scan says no surviving object points into a
            // doomed span; and those two statements are only compatible if the
            // holder did not yet hold it. Nothing in the sweep could say which.
            //
            // So: name a class, and every young and old instance of it is
            // dumped at every sweep with its address, generation, mark state
            // and every slot, each slot flagged if it targets a span this sweep
            // is about to free. One env var, no rebuild per question.
            if let Ok(want) = Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
                // Own copy of the dead-span index: the inverted scan's lives in
                // its own block, and duplicating four lines is cheaper than
                // widening that block's scope for a diagnostic.
                let mut tspans: Vec<(usize, usize)> = dead_regions
                    .iter()
                    .map(|&(off, sz, _c, _k, _n)| (from_base + off, from_base + off + sz))
                    .collect();
                tspans.sort_unstable_by_key(|&(start, _)| start);
                let victim_span = |addr: usize| -> Option<usize> {
                    tspans
                        .partition_point(|&(start, _)| start <= addr)
                        .checked_sub(1)
                        .and_then(|i| tspans.get(i))
                        .filter(|&&(_, end)| addr < end)
                        .map(|&(start, _)| start)
                };
                let mut seen = 0usize;
                let mut walked = 0usize;
                let mut walk_broke_at: Option<usize> = None;
                let mut dump = |gen: &str,
                                optr: usize,
                                h: &ObjectHeader,
                                tot: usize,
                                marked: bool,
                                seen: &mut usize| {
                    let cid = h.class_id.as_u32();
                    let name = cratonvm_gc_core::gc::resolve_class_info(cid)
                        .map(|(n, _)| n)
                        .unwrap_or_else(|| format!("cid{cid:#x}"));
                    if name != want {
                        return;
                    }
                    *seen += 1;
                    let mut slots = String::new();
                    let mut w = optr + HEADER_SIZE;
                    let hi = optr + tot;
                    while w + 8 <= hi {
                        // SAFETY: `[optr, optr + tot)` is this object's own span.
                        let word = unsafe { *(w as *const u64) } as usize;
                        let doomed = victim_span(word).is_some();
                        slots.push_str(&format!(
                            "{:+#x}={:#x}{} ",
                            w - optr,
                            word,
                            if doomed { "<<DOOMED" } else { "" }
                        ));
                        w += 8;
                    }
                    eprintln!(
                        "[sweep-trace] cycle={sweep_zero_cycle} {gen} {name}@{optr:#x} \
                         size={tot} marked={marked} slots=[{slots}]"
                    );
                };
                // Young, walked as objects (same walk as the inverted scan).
                let existing_free_tr = merge_skips(young_from.free_blocks_sorted());
                let used_tr = young_from.used();
                // gen r4w2/youngmark: the shared `YoungGridCursor`. An anomaly
                // no longer ends the walk; the first one is still reported, and
                // a walk that could not re-anchor still says TRUNCATED.
                let mut grid =
                    YoungGridCursor::new(from_base, used_tr, &existing_free_tr, &side_sorted);
                for step in grid.by_ref() {
                    let (c, tot) = match step {
                        GridStep::Object { off, size } => (off, size),
                        GridStep::Anomaly { off, .. } => {
                            // A walk that skips a stretch has NOT looked at
                            // all of the arena, and "0 instances" from it is a
                            // different statement from "0 instances found".
                            // Say which, or the next reader builds a timeline
                            // on it.
                            walk_broke_at.get_or_insert(off);
                            continue;
                        }
                    };
                    let optr = from_base + c;
                    // SAFETY: the cursor yields only parsed object starts
                    // inside the mapped from-space.
                    let h = unsafe { &*(optr as *const ObjectHeader) };
                    walked += 1;
                    let marked = h.gc_flags() & GC_FLAG_MARKED != 0;
                    let gen = if victim_span(optr).is_some() {
                        "YOUNG-DOOMED"
                    } else {
                        "YOUNG"
                    };
                    dump(gen, optr, h, tot, marked, &mut seen);
                }
                let (tr_anomalies, tr_truncated) = (grid.anomalies, grid.truncated);
                for (optr, osz) in old_gen.walk_objects() {
                    let lo = optr as usize;
                    // SAFETY: `optr` is a live old-gen object header.
                    let h = unsafe { &*(lo as *const ObjectHeader) };
                    let marked = h.gc_flags() & GC_FLAG_MARKED != 0;
                    dump("OLD", lo, h, osz, marked, &mut seen);
                }
                eprintln!(
                    "[sweep-trace] cycle={sweep_zero_cycle} {seen} instance(s) of {want} (young walk: {walked} objects, {}; used={used_tr})",
                    match walk_broke_at {
                        None => "COMPLETE".to_string(),
                        Some(off) => format!(
                            "{tr_anomalies} anomalies, the first at off={off}{} - the count is a \
                             LOWER BOUND",
                            if tr_truncated { ", walk TRUNCATED" } else { ", stretches skipped" }
                        ),
                    }
                );
                // Every DOOMED lambda-proxy object, and whether the root set
                // the mark phase was handed contained it. A lambda proxy is
                // recognisable without any class lookup: its synthetic class id
                // carries the high bit. `in_roots` is the whole question when a
                // reclaimed object turns out to have been live -- `false` means
                // the ROOT SET was incomplete (a different bug, in a different
                // file, from a marker that dropped a root it was given), and
                // nothing else in this sweep distinguishes the two.
                for &(doff, dsz, dcid, _dk, _dn) in dead_regions.iter() {
                    if dcid & 0x8000_0000 == 0 {
                        continue;
                    }
                    let victim = from_base + doff;
                    let in_roots = roots.iter().any(|r| {
                        let a = r.as_ptr() as usize;
                        a >= victim && a < victim + dsz
                    });
                    eprintln!(
                        "[sweep-trace] cycle={sweep_zero_cycle} DOOMED-LAMBDA @{victim:#x} size={dsz} class_id={dcid:#x} in_roots={in_roots}"
                    );
                }
            }
        }

        // Coalesce adjacent free blocks into maximal spans. Selective promotion
        // evacuates the long-lived set and the sweep reclaims the short-lived
        // churn, leaving a large CONTIGUOUS free region carved into hundreds of
        // thousands of Node-sized holes. `Arena::alloc` scans the free list
        // LINEARLY, so an un-coalesced 500k-entry free list makes every
        // subsequent allocation O(n) — the bintrees18 allocation cliff (young
        // drains correctly but throughput collapses). Merging adjacent holes
        // collapses that region to a handful of spans, restoring near-O(1) bump
        // allocation out of a free block. Tied to `selective_on` (Fix A): it is
        // the necessary partner of the evacuation above — without it the default-on
        // selective sweep drains young but leaves a 500k-entry free list, so
        // allocation goes O(n) and bt18 throughput collapses (the rc=127 cliff).
        // DoHead walk-desync hardening (2026-07-02): run the coalescer (and
        // its overlap-merge safety invariant, 6e3ddb05) regardless of
        // `selective_on` — with promotion disabled the free list still
        // fragments and overlapping blocks would still double-serve.
        // gcf f2/y (`CRATONVM_GEN_PINNED_COPY_DEFRAG`): the free-list bytes in
        // holes smaller than a TLAB, measured on the coalesced list this walk
        // already holds -- no extra pass.
        let mut defrag_small_bytes = 0usize;
        let mut defrag_measured = false;
        // gcg g4/yg: the opt-in incremental coalesce (gcf f4/y,
        // `CRATONVM_GEN_SWEEP_INCREMENTAL_COALESCE`) is deleted -- triage tg1
        // measured no gain over this full rebuild.
        if !gc_flags().sp_no_coalesce {
            let sorted = young_from.free_blocks_sorted();
            defrag_measured = true;
            if sorted.len() <= 1 {
                defrag_small_bytes = sorted
                    .iter()
                    .filter(|&&(_, sz)| sz < DEFRAG_SMALL_BLOCK)
                    .map(|&(_, sz)| sz)
                    .sum();
            }
            if sorted.len() > 1 {
                young_from.clear_free_list();
                let mut merged: Vec<(usize, usize)> = Vec::with_capacity(sorted.len());
                for (off, sz) in sorted {
                    if let Some(last) = merged.last_mut() {
                        let last_end = last.0 + last.1;
                        // Merge ADJACENT (off == last_end) AND OVERLAPPING
                        // (off < last_end) blocks. The old code only handled the
                        // adjacent case, so two overlapping free blocks both
                        // survived — and `Arena::alloc` (no overlap check, see
                        // `arena.rs::add_free_block`) could then SERVE the same
                        // young region twice → two live objects overlapping → the
                        // non-moving sweep's linear walk reads a header inside a
                        // neighbour and desyncs (the A2 / ReflRepro corruption:
                        // CRATONVM_DBG_A2 shows the walk overstep a live object).
                        // Overlapping spans appear when a region is freed more than
                        // once (a stale block not removed on reuse, or two dead
                        // spans that overlap after an earlier desync). Extend to the
                        // farther end so the union is one block, never double-served.
                        if off <= last_end {
                            if off < last_end {
                                A2_FL_OVERLAP_HITS.fetch_add(1, Ordering::Relaxed);
                                if gc_flags().dbg_a2
                                    && A2_FL_OVERLAP_HITS.load(Ordering::Relaxed) <= 6
                                {
                                    eprintln!(
                                        "[A2-FL] coalesce OVERLAP: block off={} size={} overlaps prev [{}, {}) — merging",
                                        off, sz, last.0, last_end,
                                    );
                                }
                            }
                            let new_end = last_end.max(off + sz);
                            last.1 = new_end - last.0;
                            continue;
                        }
                    }
                    merged.push((off, sz));
                }
                for (off, sz) in merged {
                    if sz < DEFRAG_SMALL_BLOCK {
                        defrag_small_bytes += sz;
                    }
                    young_from.add_free_block(off, sz);
                }
            }
        }
        if self.pinned_copy_defrag && defrag_measured {
            let fragmented = young_space_fragmented(
                defrag_small_bytes,
                young_from.free_list_bytes(),
                young_from.capacity().saturating_sub(young_from.used()),
            );
            self.pinned_copy_defrag_census[0].fetch_add(1, Ordering::Relaxed);
            if fragmented {
                self.pinned_copy_defrag_census[1].fetch_add(1, Ordering::Relaxed);
            }
            self.young_defrag_pending
                .store(fragmented, Ordering::Relaxed);
        }
        // gce e1/y (item 2 of `gcd-d4m-sweep-trigger-follows-the-moving-pause-goal-20260928.md`):
        // tell the arena which bytes this sweep has just zeroed and listed
        // (`zero_spans_parallel_on` above), AFTER the coalesce, whose re-adds
        // forget any earlier claim. Nothing between the publication and here
        // writes a reclaimed span (the mark-clear re-walk below skips the free
        // list). A refill whose free-list hand-out lies wholly inside one of
        // these spans may then skip its second memset; that consumer is opt-in
        // (see `Arena::hand_out_is_known_zero`). A deferred reclamation zeroed
        // and listed nothing, so it claims nothing.
        if !defer_reclamation {
            // gcj j2/yg2 (`CRATONVM_GEN_YOUNG_KNOWN_ZERO_CARRY`): plus the
            // previous set's bytes that were still free at the sweep's start
            // (taken above), merged with these spans where they touch, so a
            // coalesced block built from both is one claimed span. Empty with
            // the switch off: the set is this sweep's spans, as before.
            let carried_bytes: usize = carried_known_zero.iter().map(|&(_, l)| l).sum();
            if carried_bytes > 0 {
                self.young_premium_census[YP_KZ_CARRIED_BYTES]
                    .fetch_add(carried_bytes as u64, Ordering::Relaxed);
                self.young_premium_census[YP_KZ_CARRY_SWEEPS].fetch_add(1, Ordering::Relaxed);
            }
            // Wave 6: a lazily-zeroed sweep claims nothing for its own spans
            // (they are dirty until handed out); the carried claim stands
            // (bytes a previous sweep zeroed, still free).
            let claimed = if sweep_lazy_zero {
                Vec::new()
            } else {
                std::mem::take(&mut reclaimed_regions)
            };
            young_from.set_known_zero_free_spans(cratonvm_gc_core::arena::union_known_zero_spans(
                claimed,
                carried_known_zero,
            ));
        }
        report_phase("arena-coalesce");

        // Defence-in-depth: clear `GC_FLAG_MARKED` on every object header in
        // the from-space (conditional since gen r4/sweep — see the gate
        // below for when it is skipped and why). The survivor branch above already
        // clears the bit for objects it visited, but if the walk broke out
        // early (corrupt / zero-size header) every later survivor would keep
        // a stale mark. The next non-moving sweep treats any object with the
        // mark bit set as live regardless of root reachability, which would
        // retain garbage indefinitely (bug C5). Re-walk and clear all marks
        // — cheap (one byte per header) and idempotent on this path. Runs on
        // both the normal-completion and the `break` arm because it sits
        // after the `while` loop. Now hole-aware: the dead spans are on the
        // free list (published above), so the re-walk skips them.
        // gen r4/sweep (2026-09-23): read the cycle's empty-run retention
        // BEFORE the mark-clear re-walk. `clear_all_mark_bits_in_arena` steps
        // over the very same empty-object runs the sweep walk just stepped over
        // (they are neither freed nor published, so its re-walk meets them
        // again) and adds them to `EMPTY_RUN_BYTES_CYCLE` a second time.
        // `EMPTY_RUN_BYTES_LAST` is documented as the STANDING retention —
        // "accumulated by whichever walkers covered the arena (the sequential
        // walk and the parallel chunks partition it)" — and the ratchet below
        // compares it against a budget, so the double count inflated both by
        // ~2x. `SWEEP_ZERO_SPAN_EMPTY_RUNS` is deliberately a sum over walks
        // and is unaffected.
        let empty_run_retained = EMPTY_RUN_BYTES_CYCLE.load(Ordering::Relaxed);

        // gen r4/sweep (2026-09-23): skip the whole-arena mark-clear re-walk
        // when it provably has nothing to clear.
        //
        // Since the Family-A hardening NOTHING on the generational path writes
        // `GC_FLAG_MARKED` into a young header: `mark_young` and
        // `mark_edge_precise` side-mark only, and every `add_gc_flags(
        // GC_FLAG_MARKED)` left in this file marks an OLD-gen base. So on the
        // default path this re-walk — a second sequential header chase over the
        // entire allocated from-space, on the collector JIT-warm processes take
        // every cycle — cleared zero bits, every cycle.
        //
        // It is kept, and still runs, whenever that could be wrong: the walk
        // above was abandoned (`cursor < used`, so later survivors were never
        // visited) or any survivor it DID visit carried a header mark (a
        // side-marked one keeps its bit for this pass to clear). What remains
        // unvisited on a completed walk is only the stretches it re-anchored
        // over, and a stale mark there costs at most one extra cycle of
        // retention (the next walk that parses the object clears it in its
        // header-marked arm) — over-retention, the safe direction, which is the
        // entire documented cost of a missed clear (the bug-C5 note above).
        // `CRATONVM_DBG_GCPHASE` reports which way each cycle went.
        if cursor < used || header_marks_seen > 0 {
            clear_all_mark_bits_in_arena(&mut young_from, &side_sorted);
            report_phase("clear-marks");
        } else {
            report_phase("clear-marks-skipped");
        }

        let empty_run_young_used = young_from.used() as u64;
        EMPTY_RUN_BYTES_LAST.store(empty_run_retained, Ordering::Relaxed);
        EMPTY_RUN_YOUNG_USED_LAST.store(empty_run_young_used, Ordering::Relaxed);
        // The ratchet on the deliberate deferral -- see
        // `EMPTY_RUN_RETENTION_EXCEEDED`. Cheap (two loads and a compare, once
        // per sweep) and unconditional, because the whole point is that it runs
        // on workloads nobody thought to re-measure.
        if empty_run_retention_exceeded(empty_run_retained, empty_run_young_used) {
            EMPTY_RUN_RETENTION_EXCEEDED.fetch_add(1, Ordering::Relaxed);
            if EMPTY_RUN_RETENTION_REPORTS.fetch_add(1, Ordering::Relaxed) < 4 {
                tracing::warn!(
                    target: "cratonvm::gc::guard",
                    retained_bytes = empty_run_retained,
                    young_used = empty_run_young_used,
                    budget_permille = EMPTY_RUN_RETENTION_BUDGET_PERMILLE,
                    "young non-moving sweep: the bytes STEPPED OVER as runs of empty objects \
                     exceeded their budget as a share of the young generation. Every cycle \
                     re-skips the same runs, so this figure is a standing retention and was \
                     measured flat at 11.5-29 KB from 4 to 27 collections; a workload that \
                     grows it is the new evidence the decision not to reclaim these runs was \
                     explicitly left open for.",
                );
            }
        }
        let live_bytes = bytes_before.saturating_sub(bytes_swept);
        // gen r4w2/youngmark (2026-09-23): what this cycle actually copied —
        // selective promotion's evacuations, net of unwound candidates. See
        // `docs/feature-designs/generative-gc-architecture.md`.
        let objects_promoted_cycle = self
            .stats
            .objects_promoted
            .load(Ordering::Relaxed)
            .saturating_sub(promoted_before.0);
        let bytes_promoted_cycle = self
            .stats
            .bytes_promoted
            .load(Ordering::Relaxed)
            .saturating_sub(promoted_before.1);
        // gen r4w3/young2 (2026-09-23): the young live set this cycle traced —
        // what stayed in place (`used - free_list_bytes` after the sweep,
        // i.e. the same live-occupancy figure `needs_gc` triggers on) plus what
        // selective promotion moved out. This, not `used - swept`, is the
        // pause-goal loop's latch input on a non-moving cycle; it is handed
        // over through `non_moving_latch` below, stamped with the cycle it
        // describes, because `GcStats` has no field for it and
        // `bytes_copied` now says what was copied.
        let survivors_in_place = young_from
            .used()
            .saturating_sub(young_from.free_list_bytes());
        let latch_bytes = (survivors_in_place as u64).saturating_add(bytes_promoted_cycle);
        // A sweep that kept nearly all of a full young generation arms the
        // next cycle to tenure it, as a moving cycle does: it cannot move its
        // survivors out itself, and while it admits only a sliver of
        // allocation the age clock never ages them either.
        self.arm_promote_pressure_on_high_survival(
            occupied_before,
            survivors_in_place,
            young_from.capacity(),
        );
        if gc_flags().dbg_gcpause {
            // The A/B instrument (gen r4w2/youngmark), now reporting the
            // changed values beside the quantity `bytes_copied` used to carry
            // (`used_at_entry - swept`, printed as `was_bytes_copied`).
            eprintln!(
                "[gcpause] non-moving young: reported_bytes_copied={bytes_promoted_cycle} \
                 latch_bytes={latch_bytes} was_bytes_copied={live_bytes} \
                 used_at_entry={bytes_before} swept={bytes_swept} \
                 promoted_bytes={bytes_promoted_cycle} promoted_objects={objects_promoted_cycle} \
                 promotion_gated={}",
                // gci i4/yg (i4/st's X1): the cycle's promotion walk was gated
                // off (the value `last_cycle_drain_blocked` stores below).
                promotion_age_reachable && !selective_on,
            );
        }
        tracing::debug!(
            "non-moving young sweep: {} live objects ({} bytes), {} dead \
             objects reclaimed ({} bytes) into free list",
            objects_live,
            live_bytes,
            objects_swept,
            bytes_swept,
        );

        // gcd d9/c: this sweep's promotion walk was gated off (a refusing
        // helper window or a frozen peer, `CRATONVM_NO_SELECTIVE_PROMOTE`, or
        // a requested major holding promotion), so nothing could leave young
        // however much old gen had free. Before the count moves, like the
        // moving path's store (see `young_drain_blocked`).
        self.last_cycle_drain_blocked
            .store(promotion_age_reachable && !selective_on, Ordering::Relaxed);
        let latch_stamp = self
            .stats
            .minor_gc_count
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        // gen r4w3/young2: "the cycle `minor_gc_count` now counts was a
        // non-moving one, and its young live set was `latch_bytes`". The only
        // reader, `adapt_young_trigger_to_pause`, runs on this same thread
        // straight after the collection, so relaxed stores suffice.
        self.non_moving_latch[1].store(latch_bytes, Ordering::Relaxed);
        self.non_moving_latch[0].store(latch_stamp, Ordering::Relaxed);

        if gc_flags().dbg_precise && !evac_map.is_empty() {
            eprintln!(
                "[PRECISE] sweep_young_non_moving returning evac_map.len()={}",
                evac_map.len()
            );
        }

        (
            GcResult {
                stats: cratonvm_gc_core::gc::GcStats {
                    // gen r4w2/youngmark: deliberately still 0 although
                    // selective promotion may have evacuated objects
                    // (`objects_promoted_cycle`). Tests across the tree use
                    // `objects_copied == 0` as THE "this cycle did not run the
                    // copying collector" discriminator (e.g.
                    // `selective_promotion_runs_under_an_unproven_frame_map_but_not_a_frozen_peer`
                    // asserts it on promoting cycles), so changing it is a
                    // contract change, not a fix — see the gap page.
                    objects_copied: 0,
                    // gen r4w3/young2 (2026-09-23): what this cycle COPIED —
                    // selective promotion's evacuations, net of unwound
                    // candidates (0 on most cycles). Until now this was
                    // `used - swept`: retained bytes plus every byte already on
                    // the free list at entry, a near-constant the pause-goal
                    // loop latched on and never re-armed from. The loop's
                    // survivor input for a non-moving cycle is now
                    // `latch_bytes` (via `non_moving_latch`), changed in the
                    // same commit as this field, as the gap page required. See
                    // `docs/feature-designs/generative-gc-architecture.md`.
                    bytes_copied: usize::try_from(bytes_promoted_cycle).unwrap_or(usize::MAX),
                    bytes_freed: bytes_swept,
                },
                // Selective promotion may have evacuated some survivors to old
                // gen; `evac_map` (young→old) lets the VM-level remap update any
                // reference the conservative sweep could not. Empty when the
                // CRATONVM_SELECTIVE_PROMOTE gate is off (true non-moving).
                pointer_map: evac_map,
            },
            // The candidates the resurrection phase PROVED unreachable — not
            // the whole input list, which is what this returned until
            // 2026-09-10 and which finalized reachable objects. Addresses are
            // unchanged: the sweep does not move, and selective promotion pins
            // every young `finalizer_addrs` entry (see its pin set), so a
            // resurrected object cannot have been evacuated out from under the
            // address reported here.
            dead_finalizers,
        )
    }

}
