#[allow(unused_imports)]
use super::*;


impl ZgcRealHeap {
    // =====================================================================
    // zr z2/g2 -- THE YOUNG COMPLEMENT SWEEP
    // =====================================================================

    /// Sweep a young cycle's nursery `[floor, low cursor)` as the COMPLEMENT
    /// of its live set, and everything below the floor that the cycle owns
    /// (the young grains on the floor arm, every non-old base on the aged arm)
    /// one object at a time. `CRATONVM_ZGC_GEN_YOUNG_COMPLEMENT`, default ON
    /// since zt t14/gn5 (`=0` kill).
    ///
    /// # What it stops paying
    ///
    /// A whole-heap cycle has used the complement sweep (`sweep_bitmap`) since
    /// 2026-09: a start bit with no mark bit is garbage, so a dead object costs
    /// an address and a 16-byte header store, never a header READ. A young
    /// cycle could not use it (its floor makes the whole-heap complement wrong:
    /// the dead below the floor belong to the next major), so it kept the
    /// per-object `sweep_one` -- a header read, a size computation and a run
    /// merge for EVERY dead nursery object. On `Zr1GnGenShape` that is every
    /// object the program allocates, swept one at a time, where the default
    /// collector streams the same garbage through two bitmaps. That asymmetry
    /// is one of the candidates for generational mode's ~2x CPU (z2/g2 report,
    /// finding 3).
    ///
    /// # Why the complement is exact here
    ///
    /// Above the floor, every byte below the low cursor is covered by a live
    /// (marked) object, by a dead young one, by a filler or retired TLAB tail,
    /// or is already on the free list. On a young cycle the old generation is
    /// PRE-MARKED, so "marked" is "live or old", and every unmarked base above
    /// the floor is a dead young object this cycle reclaims. "Not covered by a
    /// marked object" is therefore exactly the reclaimable set -- the same
    /// argument as `sweep_bitmap`'s, restricted to `[c0, cursor)`.
    ///
    /// `c0` is where the chain starts: the floor, or the END of the highest
    /// registered object below the floor when that object runs over it (a
    /// free-list allocation below the floor can straddle it). Every registered
    /// base below the floor, live or dead, swept or retained, owns its whole
    /// extent, so starting the chain at its end can neither free a byte it
    /// still holds nor list a byte pass (1) has already listed.
    ///
    /// # The free list
    ///
    /// The nursery's old free blocks are RECOMPUTED by the complement, so the
    /// installer (`collect_garbage`) replaces the low free list with: the old
    /// blocks below the floor (a block straddling it is clipped at it), the
    /// spans pass (1) freed, and the complement spans -- merged, ascending.
    /// See the `young_install` arm there.
    ///
    /// # Refusals (nothing has been written when one is returned)
    ///
    /// No side mark bitmap, the `Hash` registry, a per-object sweep switch
    /// (`CRATONVM_ZGC_SWEEP_HEADER_ZERO=0` / `_DEAD_RUNS=0`) or dead-hash
    /// collection, a non-empty spill set, a floor outside `[base, cursor]`,
    /// published TLAB tails of frozen peers (`jit_tlab_skip_regions`: the
    /// per-object sweep keeps today's handling of those), an unsizable or
    /// unknowable object below the floor, or a registered base inside
    /// `[floor, c0)`. The caller then takes the per-object arm it would have
    /// taken anyway; `gen_young_complement_refusal` keeps the last reason.
    pub fn sweep_young_complement(
        &self,
        registered: &ZObjectStartsSnapshot,
        floor: usize,
        cfg: &ZSweepCfg,
        aged: bool,
    ) -> Result<ZYoungComplement, usize> {
        // ---- refusals, before any write -------------------------------------
        let Some(marks) = self.mark_bits.as_ref() else {
            return Err(YC_REFUSE_NO_MARK_BITS);
        };
        if registered.words.is_empty() {
            return Err(YC_REFUSE_HASH_REGISTRY);
        }
        if !cfg.zero_header_only || !cfg.merge_dead_runs || cfg.collect_dead_hashes {
            return Err(YC_REFUSE_PER_OBJECT_SWITCH);
        }
        if !registered.extra.is_empty() {
            return Err(YC_REFUSE_SPILL);
        }
        let base = cfg.arena_base;
        let low_end = base.saturating_add(cfg.arena_used);
        let high_abs = base.saturating_add(cfg.high_floor);
        if floor < base || floor < registered.base || floor > low_end {
            return Err(YC_REFUSE_FLOOR);
        }
        if !self.jit_tlab_skip_regions().is_empty() {
            return Err(YC_REFUSE_SKIP_REGIONS);
        }
        let old = if aged {
            match self.counters.gen_old_bits.get() {
                Some(o) => Some(o),
                None => return Err(YC_REFUSE_FLOOR),
            }
        } else {
            None
        };
        let c0 = if floor > base {
            match self.registry.nearest_base_within(floor - 1, base) {
                None => return Err(YC_REFUSE_BELOW_FLOOR_EXTENT),
                Some(None) => floor,
                // zs s2/rf (dg residual page, row 7): an over-long array the
                // live screen sizes owns its extent like any other base
                // (`live_extent`; `=0` of the screen's switch refuses as before).
                Some(Some(b)) => match self.live_extent(b, self.header_ref(b as *mut u8)) {
                    Some(size) => floor.max(b.saturating_add(size)),
                    None => return Err(YC_REFUSE_BELOW_FLOOR_EXTENT),
                },
            }
        } else {
            floor
        };
        if c0 > low_end || (c0 > floor && registered.any_start_in(floor, c0)) {
            return Err(YC_REFUSE_INSIDE_STRADDLER);
        }

        let words_len = registered.words.len();
        let floor_w = (floor - registered.base) >> 9;
        // t1 wave 8 lane ZT3GN: the diagnostic clock (debug switch only).
        let diag_t0 = zgc_dbg_relocate_phases_enabled().then(std::time::Instant::now);
        // t1 wave 8 lane ZT3GN (`CRATONVM_ZGC_GEN_YOUNG_BULK_DEAD`, zt3gn S2.4
        // slice 2 step 6): a word's dead bits are taken WHOLE -- one popcount
        // and one mask -- when nothing per object is owed for them: no dead
        // list, no header store, and the whole word below the large-object
        // end. Same counts, same registry bits as the per-bit loop.
        let bulk_dead = !cfg.want_dead
            && cfg.skip_dead_headers
            && self.counters.young_evac.bulk_dead.load(Ordering::Relaxed);
        let (mut d_words, mut d_live_old, mut d_live_young, mut d_bulk) = (0usize, 0, 0, 0);

        // ---- (1) below the floor, one object at a time ----------------------
        //
        // Exactly the set the per-object arm visits there: the young grains
        // (`for_each_young_base`'s `visit_below`) on the floor arm, every
        // registered non-old base (`sweep_young_aged`) on the aged arm. zt
        // t6/tc (GN-17): the aged arm's walk is the young-aged sweep's own,
        // stopped at the floor, and shards under
        // `CRATONVM_ZGC_GEN_YOUNG_SWEEP_WORKERS` like it.
        // t1 wave 8 lane ZT3GN (`CRATONVM_ZGC_GEN_YOUNG_BELOW_FLOOR_COMPLEMENT`,
        // opt-in): the aged arm's pass (1) as WINDOWS of the complement --
        // see `sweep_below_floor_windows`. Needs the same per-object duties
        // switched off as the bulk dead bits (no dead list, no header store).
        let below_windows_on = aged
            && !cfg.want_dead
            && cfg.skip_dead_headers
            && self
                .counters
                .young_evac
                .below_floor_complement
                .load(Ordering::Relaxed);
        let mut below_windows: Vec<(usize, usize)> = Vec::new();
        let mut below_window_bytes = 0usize;
        let mut sh = if below_windows_on {
            let old = old.expect("the aged arm holds the old-start bitmap");
            let (sh, bytes) = self.sweep_below_floor_windows(
                registered,
                (floor_w + 1).min(words_len),
                floor,
                c0,
                cfg,
                &|w| marks.word_at(w),
                &|w| old.word_at(w),
                &mut below_windows,
            );
            below_window_bytes = bytes;
            sh
        } else if aged {
            self.sweep_young_aged_sharded(
                registered,
                (floor_w + 1).min(words_len),
                floor,
                floor,
                cfg,
                self.gen_young_sweep_workers(),
            )
            .0
        } else {
            let mut sh = ZSweepShard::default();
            for (a, b) in registered.segments(0, (floor_w + 1).min(words_len)) {
                for w in a..b {
                    let mut word = registered.words[w];
                    while word != 0 {
                        let bit = word.trailing_zeros() as usize;
                        word &= word - 1;
                        let addr = registered.base + ((w * 64 + bit) << 3);
                        if addr >= floor {
                            break; // ascending: the rest of this word is pass (2)'s
                        }
                        if !self.young_page_at_offset(addr - registered.base) {
                            continue;
                        }
                        self.sweep_one(addr, cfg, &mut sh, Some(registered));
                    }
                }
            }
            sh
        };
        sh.flush();
        // ZT3GN: a window's bytes are booked as freed below the floor here
        // (the installer takes back the free blocks it already held, see
        // `ZYoungComplement::below_windows`), and are NOT in the shard's
        // `bytes_freed`: the arena's growth counts them for the cycle.
        let below = (
            sh.swept,
            sh.bytes_freed + below_window_bytes,
            sh.bytes_copied,
            sh.gen_promoted_bytes,
        );
        // Every byte pass (1) freed is LOW (below the floor) and will be counted
        // again by the installer's `now_free - was_free`.
        let low_per_object_freed = sh.bytes_freed;
        let diag_t1 = diag_t0.map(|_| std::time::Instant::now());

        // ---- (2) the nursery, as the complement of the marked set -----------
        let mut spans: Vec<(usize, usize)> = Vec::new();
        let mut prev_end = c0;
        let mut unsizable_live = false;
        let mut nursery_dead = 0usize;
        for (a, b) in registered.segments(floor_w, words_len) {
            for w in a..b {
                let start_w = registered.words[w];
                if start_w == 0 {
                    continue;
                }
                let word_base = registered.base + (w << 9);
                let mut in_range = start_w;
                if word_base < floor {
                    // Slot k names `word_base + 8k`; pass (1) owned k < ceil(gap/8).
                    let skip = (floor - word_base).div_ceil(8);
                    in_range &= if skip >= 64 { 0 } else { !0u64 << skip };
                }
                if in_range == 0 {
                    continue;
                }
                d_words += 1;
                let mark_w = marks.word_at(w);
                // Read BEFORE any `age_survivor` below can promote into it, so
                // `was_old` is the pre-sweep answer.
                let old_w = old.map_or(0, |o| o.word_at(w));

                // -- the dead: addresses only, no header read --
                let mut dead_w = in_range & !mark_w;
                let mut cleared = 0u64;
                if bulk_dead && dead_w != 0 && word_base.saturating_add(512) <= high_abs {
                    let n = dead_w.count_ones() as usize;
                    sh.swept += n;
                    sh.dead_count += n;
                    nursery_dead += n;
                    sh.dead_headers_skipped += n;
                    cleared = dead_w;
                    dead_w = 0;
                    d_bulk += 1;
                }
                while dead_w != 0 {
                    let bit = dead_w.trailing_zeros() as usize;
                    dead_w &= dead_w - 1;
                    let addr = word_base + (bit << 3);
                    if addr >= high_abs {
                        // The large-object end keeps the per-object protocol.
                        self.sweep_one(addr, cfg, &mut sh, Some(registered));
                        continue;
                    }
                    sh.swept += 1;
                    sh.dead_count += 1;
                    nursery_dead += 1;
                    if cfg.want_dead {
                        sh.dead.push(addr);
                    }
                    if cfg.skip_dead_headers {
                        sh.dead_headers_skipped += 1;
                    } else {
                        // SAFETY: a registered base below the low cursor; every
                        // allocation is at least `HEADER_SIZE` bytes.
                        unsafe { std::ptr::write_bytes(addr as *mut u8, 0, HEADER_SIZE) };
                    }
                    cleared |= 1u64 << bit;
                }
                if cleared != 0 {
                    // `registered.words.is_empty()` refused the `Hash` arm above.
                    let _ = self.registry.clear_word_bits(w, cleared);
                }

                // -- the live: one header read, extent, and the complement --
                let mut live_w = in_range & mark_w;
                while live_w != 0 {
                    let bit = live_w.trailing_zeros() as usize;
                    live_w &= live_w - 1;
                    let addr = word_base + (bit << 3);
                    let was_old = old_w & (1u64 << bit) != 0;
                    if addr >= high_abs {
                        // zt t16/gn5fix: the aged arm does not visit an OLD
                        // large-object survivor (`sweep_young_aged` walks
                        // `registered & !old`): it is pre-marked, its bytes are
                        // already in `gen_old_live_bytes`, and `sweep_one` here
                        // counted them again in `bytes_copied` -- the live figure
                        // read the array twice once the complement became the
                        // default (`zt6tc_an_old_large_array_is_counted_once_on_
                        // the_floor_arm`, bitmap arm). The floor arm still
                        // visits it (GN-18 subtracts those bytes there).
                        if !(aged && was_old) {
                            self.sweep_one(addr, cfg, &mut sh, Some(registered));
                        }
                        continue;
                    }
                    if was_old {
                        d_live_old += 1;
                    } else {
                        d_live_young += 1;
                    }
                    let header = self.header_mut(addr as *mut u8);
                    let size = Self::alloc_size(header);
                    // The aged arm does not visit old objects (their bytes ride
                    // in `gen_nursery_old_bytes`), so they must not be counted
                    // here either; the floor arm visits and counts them.
                    if !(aged && was_old) {
                        sh.swept += 1;
                        match size {
                            Some(s) => {
                                sh.bytes_copied += s;
                                sh.objects_copied += 1;
                            }
                            // zs s2/rv: a live over-long array is a survivor
                            // (accounting only; `size` stays `None`).
                            None => match self.oversize_live_array_extent(addr, header) {
                                Some(s) => {
                                    sh.bytes_copied += s;
                                    sh.objects_copied += 1;
                                }
                                None => sh.unsizable += 1,
                            },
                        }
                    }
                    if !cfg.bulk_clearable {
                        header.clear_gc_flags(GC_FLAG_MARKED);
                    }
                    if cfg.gen_on && self.age_survivor(addr, header, cfg.promo_age) {
                        sh.gen_promoted += 1;
                        sh.gen_promoted_bytes += size.unwrap_or(0);
                    }
                    if unsizable_live {
                        // The previous survivor ends at or before this base.
                        unsizable_live = false;
                        prev_end = prev_end.max(addr);
                    }
                    if addr > prev_end {
                        spans.push((prev_end - base, addr - prev_end));
                    }
                    match size {
                        Some(s) => prev_end = prev_end.max(addr.saturating_add(s)),
                        None => {
                            unsizable_live = true;
                            prev_end = prev_end.max(addr);
                        }
                    }
                }
            }
        }
        sh.flush();
        sh.dead_in_runs += nursery_dead;
        // An unsizable last survivor has no known end: keep the cursor.
        let new_cursor = if unsizable_live {
            low_end
        } else {
            prev_end.max(c0).min(low_end)
        };
        let runs = spans.len() + usize::from(new_cursor < low_end) + below_windows.len();
        if d_bulk != 0 {
            self.counters
                .young_evac
                .bulk_dead_words
                .fetch_add(d_bulk, Ordering::Relaxed);
        }
        if !below_windows.is_empty() {
            self.counters
                .young_evac
                .below_windows_total
                .fetch_add(below_windows.len(), Ordering::Relaxed);
        }
        let (p1_us, p2_us) = match (diag_t0, diag_t1) {
            (Some(t0), Some(t1)) => (
                t1.duration_since(t0).as_micros() as u64,
                t1.elapsed().as_micros() as u64,
            ),
            _ => (0, 0),
        };
        Ok(ZYoungComplement {
            sh,
            below,
            spans,
            new_cursor_off: new_cursor - base,
            floor_off: floor - base,
            low_per_object_freed,
            runs,
            diag: (d_words, d_live_old, d_live_young, d_bulk, p1_us, p2_us),
            below_windows,
        })
    }

    /// t1 wave 8 lane ZT3GN (`CRATONVM_ZGC_GEN_YOUNG_BELOW_FLOOR_COMPLEMENT`,
    /// opt-in): the young complement's pass (1) on the AGED arm, below the
    /// floor, as windows of the complement instead of one `sweep_one` (a
    /// header read, a size, a run merge) per dead young object.
    ///
    /// # Why it is needed
    ///
    /// Near full, or once the nursery's bump cursor meets the arena's end, the
    /// mutator allocates from free-list holes BELOW the floor, and every one
    /// of those objects is then swept per object: t1 wave 8 rows measured
    /// 25-35 ms per minor on `ProxyBoxingOomeProbe -Xmx256m` (~0.8M dead
    /// below the floor, 35 ns each) and 150-200 ms on `Zr1GnGenShape` once a
    /// young slide had marched the floor to the arena's end.
    ///
    /// # The windows, and why they are exact
    ///
    /// Below the floor a registered base is LIVE when it is old (pre-marked,
    /// the old-start bit) or marked, and DEAD otherwise (young and unmarked:
    /// exactly the set the per-object arm frees). Objects never overlap, and
    /// a free block never overlaps an object, so the bytes from the END of the
    /// last live object before a run of dead ones to the START of the next
    /// live object (or to `c0`, the chain start of pass (2), when the run
    /// reaches the floor) hold only that run's dead objects, free blocks and
    /// unregistered filler -- all reclaimable, the nursery complement's own
    /// argument. Each such window is returned (arena offsets, ascending) and
    /// the installer drops every old free block that touches one, so no byte
    /// is listed twice. Gaps between live objects that hold no dead base get
    /// no window, and their free blocks stay as they were.
    ///
    /// Header reads: every young survivor (its sweep duties, through
    /// `sweep_one`), and the one live object before each run whose end is not
    /// yet known -- O(young live + runs), never a dead object's. A live object
    /// whose extent cannot be known makes its following run fall back to
    /// `sweep_one`, object by object.
    ///
    /// Returns the shard (dead counts, the survivors' accounting, the
    /// fallback's spans) and the windows' total bytes.
    #[allow(clippy::too_many_arguments)]
    pub fn sweep_below_floor_windows(
        &self,
        registered: &ZObjectStartsSnapshot,
        w_end: usize,
        floor: usize,
        c0: usize,
        cfg: &ZSweepCfg,
        mark_word: &dyn Fn(usize) -> u64,
        old_word: &dyn Fn(usize) -> u64,
        windows: &mut Vec<(usize, usize)>,
    ) -> (ZSweepShard, usize) {
        let base = cfg.arena_base;
        let mut sh = ZSweepShard::default();
        // The end of the last live object whose end is known, and a live base
        // whose end is still to be read (an old object, sized lazily).
        let mut live_end = base.max(registered.base);
        let mut pending: Option<usize> = None;
        // An open window's start; `fallback` while the run after a live
        // object of unknown extent is swept object by object.
        let mut open: Option<usize> = None;
        let mut fallback = false;
        let mut window_bytes = 0usize;
        for (a, b) in registered.segments(0, w_end) {
            for w in a..b {
                let start_w = registered.words[w];
                if start_w == 0 {
                    continue;
                }
                let word_base = registered.base + (w << 9);
                if word_base >= floor {
                    break;
                }
                // Bit k names `word_base + 8k`; only k < ceil((floor - wb) / 8)
                // is below the floor.
                let keep = (floor - word_base).div_ceil(8);
                let in_range = start_w & if keep >= 64 { !0u64 } else { (1u64 << keep) - 1 };
                if in_range == 0 {
                    continue;
                }
                let old_w = old_word(w);
                let live_w = in_range & (mark_word(w) | old_w);
                let dead_w = in_range & !live_w;
                if dead_w == 0 && open.is_none() && !fallback {
                    // No run starts or ends here: visit the young survivors,
                    // and remember the highest live base for a later run.
                    let mut young = live_w & !old_w;
                    let top = 63 - live_w.leading_zeros() as usize;
                    while young != 0 {
                        let bit = young.trailing_zeros() as usize;
                        young &= young - 1;
                        let addr = word_base + (bit << 3);
                        self.sweep_one(addr, cfg, &mut sh, Some(registered));
                        pending = Some(addr);
                    }
                    pending = Some(word_base + (top << 3));
                    continue;
                }
                let mut bits = in_range;
                let mut cleared = 0u64;
                while bits != 0 {
                    let bit = bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    let m = 1u64 << bit;
                    let addr = word_base + (bit << 3);
                    if live_w & m != 0 {
                        if let Some(st) = open.take() {
                            if addr > st {
                                windows.push((st - base, addr - st));
                                window_bytes += addr - st;
                            }
                        }
                        fallback = false;
                        if old_w & m == 0 {
                            self.sweep_one(addr, cfg, &mut sh, Some(registered));
                        }
                        pending = Some(addr);
                        continue;
                    }
                    // A dead young object.
                    if open.is_none() && !fallback {
                        if let Some(lb) = pending.take() {
                            match self.live_extent(lb, self.header_ref(lb as *mut u8)) {
                                Some(size) => live_end = live_end.max(lb.saturating_add(size)),
                                None => fallback = true,
                            }
                        }
                        if !fallback {
                            if live_end <= addr {
                                open = Some(live_end);
                            } else {
                                // An extent reaching past this base: not a
                                // shape the windows can describe.
                                fallback = true;
                            }
                        }
                    }
                    if fallback {
                        // `sweep_one` removes the registry bit itself.
                        self.sweep_one(addr, cfg, &mut sh, Some(registered));
                        continue;
                    }
                    sh.swept += 1;
                    sh.dead_count += 1;
                    sh.dead_in_runs += 1;
                    sh.dead_headers_skipped += 1;
                    cleared |= m;
                }
                if cleared != 0 {
                    // The bitmap registry (`registered.words` is non-empty).
                    let _ = self.registry.clear_word_bits(w, cleared);
                }
            }
        }
        if let Some(st) = open.take() {
            // The run reaches the floor: the highest base below it is dead and
            // `c0` is its end (`sweep_young_complement`).
            if c0 > st {
                windows.push((st - base, c0 - st));
                window_bytes += c0 - st;
            }
        }
        sh.flush();
        (sh, window_bytes)
    }

    /// Merge the young complement's free spans into the arena's LOW free list,
    /// under the arena lock the caller holds: old blocks below the floor
    /// (clipped at it), pass (1)'s low spans, the complement spans; then lower
    /// the cursor. `per_object_spans` is drained: its HIGH spans are handed to
    /// `add_free_block` one at a time, as every sweep arm does. Returns the
    /// bytes this cycle freed in the arena (the low list's growth plus the
    /// retraction plus the high spans), to be added to the shard's figure
    /// minus `low_per_object_freed`.
    pub fn install_young_complement(
        arena: &mut cratonvm_gc_core::arena::Arena,
        yc_spans: &[(usize, usize)],
        floor_off: usize,
        new_cursor_off: usize,
        per_object_spans: &mut Vec<(usize, usize)>,
        below_windows: &[(usize, usize)],
    ) -> (usize, usize) {
        let dbg_t0 = zgc_dbg_relocate_phases_enabled().then(std::time::Instant::now);
        let high = arena.high_cursor();
        // ZT3GN: the new low spans are the windows (whole gaps between live
        // objects), the per-object sweep's low spans and the complement. Every
        // other old block below the floor stays on the list untouched.
        let mut low: Vec<(usize, usize)> = below_windows.to_vec();
        let mut high_spans: Vec<(usize, usize)> = Vec::new();
        for (off, len) in per_object_spans.drain(..) {
            if off >= high {
                high_spans.push((off, len));
            } else {
                low.push((off, len));
            }
        }
        low.extend_from_slice(yc_spans);
        // Stable, run-adaptive: `low` is three ascending runs back to back.
        low.sort_by_key(|s| s.0);
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(low.len());
        for (off, len) in low {
            if len == 0 {
                continue;
            }
            match merged.last_mut() {
                Some((mo, ml)) if *mo + *ml >= off => {
                    debug_assert!(
                        *mo + *ml == off,
                        "young complement: free spans overlap ({}+{} over {off})",
                        *mo,
                        *ml
                    );
                    *ml = (*ml).max(off + len - *mo);
                }
                _ => merged.push((off, len)),
            }
        }
        let dbg_t1 = dbg_t0.map(|_| std::time::Instant::now());
        let (removed, in_windows) =
            arena.splice_low_free_list(below_windows, floor_off.min(high), floor_off, &merged);
        let dbg_t2 = dbg_t0.map(|_| std::time::Instant::now());
        let now_new: usize = merged.iter().map(|s| s.1).sum();
        let mut freed = now_new.saturating_sub(removed);
        freed += arena.retract_cursor_to(new_cursor_off);
        for (off, len) in high_spans {
            arena.add_free_block(off, len);
        }
        if let (Some(t0), Some(t1), Some(t2)) = (dbg_t0, dbg_t1, dbg_t2) {
            eprintln!(
                "[zgc-young-install] merge_us={} splice_us={} new={} windows={}",
                t1.duration_since(t0).as_micros(),
                t2.duration_since(t1).as_micros(),
                merged.len(),
                below_windows.len(),
            );
        }
        (freed, in_windows)
    }

    /// The fourth reason a generational cycle must be whole-heap: the fixed
    /// `minors_per_major` ceiling, or -- under
    /// `CRATONVM_ZGC_GEN_MAJOR_PROMOTED_PERCENT=<p>` -- old GROWTH.
    ///
    /// # Why a minor count is the wrong ceiling on this heap
    ///
    /// The ceiling exists to bound garbage promoted before it died: a young
    /// cycle retains every old object unexamined. But the bound it actually
    /// needs is on BYTES PROMOTED, and a count of minors only approximates it.
    /// At the default 10 % nursery and 8 minors per major, a major falls due
    /// after ~0.9x the heap of allocation -- the same rate the default
    /// collector runs WHOLE-HEAP cycles at (its 75 % threshold) -- so on a
    /// workload whose old generation does not grow (`Zr1GnGenShape`: 64 MiB
    /// old, survivors held 8 rounds and released before they promote) the
    /// generational mode ran as many full traces as the default and ~35 young
    /// cycles on top. HotSpot's generational ZGC ran zero minors and three
    /// majors on the same shape: its majors are driven by old occupancy.
    ///
    /// # The rule
    ///
    /// Major when the bytes promoted since the last major reach `p` % of
    /// capacity (floating old garbage cannot exceed the bytes promoted), or
    /// when the last cycle's live estimate reached the static threshold
    /// (`gc_threshold`: the heap is filling, examine everything), or -- the
    /// backstop -- after 8x the fixed ceiling of minors.
    pub fn gen_major_ceiling_reached(&self) -> bool {
        self.gen_major_ceiling_due(true)
    }

    /// [`Self::gen_major_ceiling_reached`] with its rule counters bumped only
    /// when `count` is set. zt t9/gn: the young-cycle prediction
    /// (`young_cycle_predicted`) asks with `count = false`, so it evaluates
    /// the opt-in rules exactly instead of failing closed on them.
    pub fn gen_major_ceiling_due(&self, count: bool) -> bool {
        let minors = self.minors_since_major.load(Ordering::Relaxed);
        let ceiling = self.gen_minors_per_major.load(Ordering::Relaxed);
        let pct = self
            .counters
            .gen_major_promoted_percent
            .load(Ordering::Relaxed);
        if pct == 0 {
            return minors >= ceiling;
        }
        self.gen_major_by_old_growth(minors, ceiling, pct, count)
    }

    /// See [`Self::gen_major_ceiling_reached`]. Counted (when `count`) when
    /// the old-growth clauses (not the backstop) force the major.
    pub fn gen_major_by_old_growth(
        &self,
        minors: usize,
        ceiling: usize,
        pct: usize,
        count: bool,
    ) -> bool {
        if minors >= ceiling.saturating_mul(8).max(1) {
            return true;
        }
        let cap = self.heap_capacity();
        let promoted = self
            .counters
            .gen_promoted_since_major
            .load(Ordering::Relaxed);
        // zt t3/gn: the occupancy clause reads the LAST CYCLE'S live figure
        // (`cycle_alloc_watermark`, the `allocated` that cycle republished), as
        // the doc above says. It read `allocated`, which at the decision also
        // holds every byte allocated since -- so every collection the
        // live-bytes threshold asked for (`allocated >= gc_threshold` is that
        // clause's own test) was forced whole-heap, and only collections the
        // nursery clause asked for below the threshold could be young.
        let grew = promoted >= (cap / 100).saturating_mul(pct)
            || self.cycle_alloc_watermark.load(Ordering::Relaxed) >= self.gc_threshold;
        if grew && count {
            self.counters
                .gen_majors_by_old_growth
                .fetch_add(1, Ordering::Relaxed);
        }
        grew
    }

    // zt t3/gn's third rule, majors by old growth against the free space the
    // last major left (`CRATONVM_ZGC_GEN_MAJOR_HEADROOM_PERCENT`), was retired
    // in zt t14/gn5: refuted by the t12 rows (`Zr2CpCapacity grow`, CPU
    // 1.02x / 1.49x / 5.02x of the default at q = 25 / 50 / 75, the bar was
    // <= 0.75x; `zgc-zt12gn4-generational-what-is-left-CLOSED-20261022.md` item 9).

    /// zt t5/gn: the old live figure a whole-heap generational cycle leaves
    /// for the young cycles after it (`gen_old_live_bytes`): every survivor's
    /// bytes MINUS the young survivors of the large-object region.
    ///
    /// # Why those are not "below the floor"
    ///
    /// A young cycle's live figure is `old live + what its aged sweep visited
    /// above the floor + nursery promotions` (the PHASE G bookkeeping), which
    /// assumes everything a whole-heap cycle counted lies below the floor it
    /// sets (the low cursor). The large-object region is at the TOP of the
    /// arena, above every floor: a young object there (an allocation of 64 KiB
    /// or more, `ZGC_LARGE_OBJECT_MIN`) was counted here AND by every following
    /// young cycle's aged sweep, then again in `gen_nursery_old_bytes` once
    /// promoted, until the next major. A heap filled with large arrays read up
    /// to twice its live set: the live-bytes trigger fired early and the
    /// near-full soft clear (`free_after_last_cycle`) saw a full heap that was
    /// not (`Zs6NoSoftNearFull` on the generational arms).
    /// `CRATONVM_ZGC_GEN_HIGH_YOUNG_LIVE=0` restores the whole figure.
    ///
    /// # zt t6/tc (GN-18): the floor arm counts the OLD ones too
    ///
    /// When the young cycles after this major take the FLOOR-bounded sweep
    /// (the old-start bitmap off, `CRATONVM_ZGC_GEN_OLD_BITMAP=0`, or the
    /// young-aged sweep off, `CRATONVM_ZGC_GEN_YOUNG_AGED_SWEEP=0`), they sweep
    /// every base from the floor upward -- the whole large-object region, its
    /// OLD objects included (pre-marked, so they survive and land in
    /// `bytes_copied`) -- and add `gen_old_live_bytes` on top. So on that arm
    /// the old large-object survivors (`old_high_bytes`) leave the figure as
    /// well: the young cycle counts them itself, once.
    /// `CRATONVM_ZGC_GEN_FLOOR_ARM_HIGH_OLD_LIVE=0` keeps them (the double
    /// count). The aged arm visits `registered & !old` only, so there the old
    /// ones stay in the figure.
    pub fn gen_old_live_after_major(
        &self,
        bytes_copied: usize,
        young_high_bytes: usize,
        old_high_bytes: usize,
    ) -> usize {
        let mut old_live = bytes_copied;
        if zgc_flag_default("CRATONVM_ZGC_GEN_HIGH_YOUNG_LIVE", true) {
            old_live = old_live.saturating_sub(young_high_bytes);
        }
        if self.gen_young_cycles_take_the_floor_arm()
            && zgc_flag_default("CRATONVM_ZGC_GEN_FLOOR_ARM_HIGH_OLD_LIVE", true)
        {
            old_live = old_live.saturating_sub(old_high_bytes);
        }
        old_live
    }

    /// zt t6/tc (GN-18): will the young cycles after a whole-heap cycle take
    /// the floor-bounded sweep? The aged sweep needs the old-start bitmap on
    /// (a whole-heap cycle has just rebuilt it, so it is valid) and its own
    /// switch on -- the `young_aged_sweep` condition of `collect_garbage`.
    pub fn gen_young_cycles_take_the_floor_arm(&self) -> bool {
        !(self.counters.gen_old_bits_on.load(Ordering::Relaxed)
            && self
                .counters
                .gen_young_aged_sweep_on
                .load(Ordering::Relaxed))
    }

    /// `CRATONVM_ZGC_GEN_FULL_REQUEST_MAJOR` for THIS heap (tests).
    pub fn set_gen_full_request_major(&self, on: bool) {
        self.counters
            .gen_full_request_major
            .store(on, Ordering::Relaxed);
    }

    /// `CRATONVM_ZGC_GEN_MAJOR_RULE_CAUSES` for THIS heap (tests).
    pub fn set_gen_major_rule_causes(&self, on: bool) {
        self.counters
            .gen_major_rule_causes
            .store(on, Ordering::Relaxed);
    }

    /// zt t11/gn3: collections `CRATONVM_ZGC_GEN_MAJOR_RULE_CAUSES` made
    /// whole-heap (`[GC] zgc-gen-cost:` `majors_by_rule_cause=`).
    pub fn gen_majors_by_rule_cause(&self) -> usize {
        self.counters
            .gen_majors_by_rule_cause
            .load(Ordering::Relaxed)
    }

    /// `CRATONVM_ZGC_GEN_YOUNG_SKIP_REFERENCES` for THIS heap (tests).
    pub fn set_gen_young_skip_references(&self, on: bool) {
        self.counters
            .gen_young_skip_refs_on
            .store(on, Ordering::Relaxed);
    }

    /// `CRATONVM_ZGC_GEN_YOUNG_PREDICT_EXACT` for THIS heap (tests).
    pub fn set_gen_young_predict_exact(&self, on: bool) {
        self.counters
            .gen_young_predict_exact
            .store(on, Ordering::Relaxed);
    }

    /// zt t7/cc: is `CRATONVM_ZGC_GEN_YOUNG_SKIP_REFERENCES` on and the
    /// generational mode with it? The VM's pre-collection pass asks this
    /// first, so outside the generational mode (the default) or with the
    /// switch killed (`=0`) it takes no lock for it.
    pub fn young_skip_references_on(&self) -> bool {
        self.generational_enabled.load(Ordering::Relaxed)
            && self.counters.gen_young_skip_refs_on.load(Ordering::Relaxed)
    }

    // =====================================================================
    // zt t7/cc -- A YOUNG CYCLE TREATS REFERENCES AS STRONG (default ON
    // inside the generational mode since zt t8, `=0` kill)
    // =====================================================================
    //
    // `docs/feature-designs/zgc-architecture.md`. HotSpot 25's generational ZGC discovers
    // references in its OLD marking only; a minor follows a `Reference`'s
    // `referent` like any other field, so a young referent survives every
    // minor and is cleared (or its finalizable object finalized) by the next
    // major. CratonVM's references are cleared one level up: the VM's
    // pre-collection pass (`weakref_null_referents_pre_gc`) nulls every active
    // weak / phantom (and policy-condemned soft) referent slot before ANY
    // collection, and the post-collection pass clears the rows whose referent
    // did not survive. So the young cycle is decided too late for the
    // reference pass: it is PREDICTED here, before the pass, from the same
    // inputs `collect_garbage`'s `young_cycle` reads.
    //
    // Armed (switch on, a young cycle predicted), the pre-collection pass
    // leaves every slot in place, so every marker traces the referent as a
    // strong edge: a young referent survives, its row is restored as a
    // survivor, nothing is cleared or enqueued. A dead young finalizable
    // object is kept (marked with its closure) and NOT reported as
    // resurrected, so its finalizer row stays pending for the next major
    // (`collect_garbage`'s resurrection loop).
    //
    // Sound in both directions of a wrong prediction. Armed but whole-heap:
    // the major traces the referents strongly too and clears them one major
    // later (retention, never a loss; `young_refs_mispredicted=`). Not armed
    // but young: the cycle runs as before the switch.

    /// The young-cycle prediction: `collect_garbage`'s `young_cycle` test with
    /// the inputs the pre-collection pass can see, WITHOUT the counters the
    /// real decision bumps.
    ///
    /// zt t9/gn: the two opt-in major rules (`CRATONVM_ZGC_GEN_MAJOR_HEADROOM_
    /// PERCENT`, `CRATONVM_ZGC_GEN_MAJOR_PROMOTED_PERCENT`) are evaluated
    /// exactly (`gen_major_ceiling_due(false)`, the function the real decision
    /// calls with its counters). t7/cc failed closed on them -- every
    /// collection under either rule predicted a major -- so on the S1 arms
    /// (`G+H<q>` of the generational proposal) no young cycle kept its
    /// referents: the young window answered `kept=false` against HotSpot's
    /// `true`, and `young_refs_mispredicted` stayed 0 because a collection
    /// that is never armed is never counted. Without either rule the answer is
    /// t7/cc's. `CRATONVM_ZGC_GEN_YOUNG_PREDICT_EXACT=0` restores the
    /// fail-closed arm.
    pub fn young_cycle_predicted(&self) -> bool {
        let c = &self.counters;
        if !self.generational_enabled.load(Ordering::Relaxed)
            || self.conc_cycle_active.load(Ordering::Acquire)
            || self.hard_alloc_failure.load(Ordering::Relaxed)
            || self.gen_force_major_next.load(Ordering::Relaxed)
            || !self.has_old_objects.load(Ordering::Relaxed)
            || !self.young_pages_are_complete()
        {
            return false;
        }
        // The full request (`System.gc()`): read on the collecting thread, as
        // `collect_garbage` reads it, and not consumed here.
        if c.gen_full_request_major.load(Ordering::Relaxed)
            && cratonvm_gc_core::gc_quiescence::major_gc_requested()
        {
            return false;
        }
        // zt t11/gn3: the major-only HotSpot causes, as `collect_garbage`
        // decides them. A full-request door clears the warm-up bit before the
        // collection reads the mask (`CRATONVM_ZGC_WARMUP_EXPLICIT_DOOR`); a
        // full request already answered above unless its own switch is off,
        // so the bit is dropped here the same way.
        //
        // zt t12/x9: through `mask_after_warmup_doors`, which mirrors ALL of
        // `collect_garbage`'s warm-up clears (the explicit door under its own
        // switch, the `Timer` rule; zt t20/tg9 retired the opt-in stall door
        // this also mirrored). Dropping the bit on the full request alone
        // predicted a major for a cycle that then ran young and UNARMED
        // (young referents cleared, unlike HotSpot's minor), a direction
        // `young_refs_mispredicted` cannot see.
        if c.gen_major_rule_causes.load(Ordering::Relaxed) {
            let mask = mask_after_warmup_doors(c.trigger_seen.load(Ordering::Relaxed));
            if gen_mask_is_major_rule(mask) {
                return false;
            }
        }
        // `gen_major_ceiling_reached` without its counters.
        let opt_in_rule = c.gen_major_promoted_percent.load(Ordering::Relaxed) != 0;
        let fail_closed = opt_in_rule && !c.gen_young_predict_exact.load(Ordering::Relaxed);
        if fail_closed || self.gen_major_ceiling_due(false) {
            return false;
        }
        // `gen_young_refused_for_bitmap` without its refusal count.
        !(c.gen_old_bits_on.load(Ordering::Relaxed)
            && c.gen_bitmap_fallback_major.load(Ordering::Relaxed)
            && !self.old_bits_usable())
    }

    /// zt t7/cc: called by the VM's pre-collection pass, on the collecting
    /// thread at the safepoint, immediately before this collection. `want` is
    /// `false` when a clear-all-soft request is pending (that collection must
    /// clear, and is made whole-heap by `CRATONVM_ZGC_GEN_SOFT_CLEAR_MAJOR`).
    /// Answers whether the pass may leave every referent slot in place, and
    /// records the answer for `collect_garbage` (always stored, so a stale
    /// arming cannot reach a later collection).
    pub fn arm_young_refs_strong(&self, want: bool) -> bool {
        let armed = want
            && self.counters.gen_young_skip_refs_on.load(Ordering::Relaxed)
            && self.young_cycle_predicted();
        self.counters
            .gen_young_refs_strong_armed
            .store(armed, Ordering::Relaxed);
        // zt t10/gn2: a soft-clock note the previous collection's reference
        // round did not consume (no round ran) never reaches this one.
        self.counters
            .gen_armed_major_soft_clock_due
            .store(false, Ordering::Relaxed);
        armed
    }

    /// zt t11/gn3: is the collection about to run armed as a young cycle that
    /// keeps its referents ([`Self::arm_young_refs_strong`] answered `true`
    /// and `collect_garbage` has not taken it yet)? Read by the VM between
    /// the pre-collection pass and the collection, on the collecting thread
    /// (`CRATONVM_ZGC_GEN_YOUNG_JNI_WEAK_STRONG`). One relaxed load.
    pub fn young_refs_strong_armed(&self) -> bool {
        self.counters
            .gen_young_refs_strong_armed
            .load(Ordering::Relaxed)
    }

    /// `collect_garbage`'s half: take the arming and judge the prediction.
    /// `true` only for an armed young cycle.
    pub fn take_young_refs_strong(&self, young_cycle: bool) -> bool {
        if !self
            .counters
            .gen_young_refs_strong_armed
            .swap(false, Ordering::Relaxed)
        {
            return false;
        }
        if young_cycle {
            self.counters
                .gen_young_refs_strong_cycles
                .fetch_add(1, Ordering::Relaxed);
        } else {
            self.counters
                .gen_young_refs_mispredicted
                .fetch_add(1, Ordering::Relaxed);
            // zt t10/gn2: this whole-heap cycle's pre-collection pass ran
            // the young arm, which records no "now" for the soft clock.
            self.counters
                .gen_armed_major_soft_clock_due
                .store(true, Ordering::Relaxed);
        }
        young_cycle
    }

    /// zt t10/gn2 (`CRATONVM_ZGC_GEN_ARMED_MAJOR_SOFT_CLOCK`, read by the
    /// VM): did the last collection run whole-heap after the pre-collection
    /// pass armed it as young (`young_refs_mispredicted`)? Such a pass skips
    /// `condemn_idle_soft_refs`, the only place a ZGC collection records the
    /// round's "now", so the round would commit no new `SoftReference.clock`
    /// although HotSpot's major advances it. `true` once per such collection
    /// (consumed); `false` on every other collection and off the mode.
    pub fn take_armed_major_soft_clock_due(&self) -> bool {
        self.counters
            .gen_armed_major_soft_clock_due
            .swap(false, Ordering::Relaxed)
    }

    /// zt t7/cc: `(young cycles with references strong, armed collections
    /// that ran whole-heap, dead young finalizables deferred to a major)` --
    /// `[GC] zgc-gen-cost:` `young_refs_strong_cycles=` /
    /// `young_refs_mispredicted=` / `young_fin_deferred=`.
    pub fn gen_young_refs_strong_report(&self) -> (usize, usize, usize) {
        let c = &self.counters;
        (
            c.gen_young_refs_strong_cycles.load(Ordering::Relaxed),
            c.gen_young_refs_mispredicted.load(Ordering::Relaxed),
            c.gen_young_fin_deferred.load(Ordering::Relaxed),
        )
    }

    /// `CRATONVM_ZGC_GEN_YOUNG_COMPLEMENT` for THIS heap (tests).
    pub fn set_gen_young_complement_enabled(&self, on: bool) {
        self.counters
            .gen_young_complement_on
            .store(on, Ordering::Relaxed);
    }

    /// `CRATONVM_ZGC_GEN_YOUNG_RELOCATE` for THIS heap (tests).
    pub fn set_gen_young_relocate_enabled(&self, on: bool) {
        self.counters
            .gen_young_relocate_on
            .store(on, Ordering::Relaxed);
    }

    /// `CRATONVM_ZGC_GEN_YOUNG_EVACUATE` for THIS heap (tests).
    pub fn set_gen_young_evacuate_enabled(&self, on: bool) {
        self.counters.young_evac.on.store(on, Ordering::Relaxed);
    }

    /// t1 wave 6 lane Y4: the nursery-scoped young slide's cumulative
    /// engagement (`[GC] zgc-gen-young-evac:`).
    pub fn gen_young_evacuate_report(&self) -> young_evacuate::ZYoungEvacReport {
        self.counters.young_evac.report()
    }

    /// `CRATONVM_ZGC_NURSERY_CENSUS` for THIS heap (tests).
    pub fn set_nursery_census_enabled(&self, on: bool) {
        self.counters.gen_census_on.store(on, Ordering::Relaxed);
    }

    /// `CRATONVM_ZGC_GEN_MAJOR_PROMOTED_PERCENT` for THIS heap (tests); 0 is
    /// the fixed ceiling.
    pub fn set_gen_major_promoted_percent(&self, pct: usize) {
        self.counters
            .gen_major_promoted_percent
            .store(pct.min(100), Ordering::Relaxed);
    }

    /// zt t7/tg (GN-12): book one young cycle's outcome in the weak-minor
    /// census and answer whether the next cycle must be a major: always when
    /// it freed nothing (the rule since zr), and under the opt-in
    /// `CRATONVM_ZGC_GEN_ESCALATE_WEAK_MINOR=1` also when it freed under an
    /// eighth of the nursery budget ([`gen_escalate_after_minor`]).
    pub fn note_gen_minor_outcome(&self, bytes_freed: usize) -> bool {
        let c = &self.counters;
        if gen_minor_is_weak(bytes_freed, self.heap_capacity()) {
            c.gen_weak_minors.fetch_add(1, Ordering::Relaxed);
            let run = c.gen_weak_minor_run.fetch_add(1, Ordering::Relaxed) + 1;
            c.gen_weak_minor_max_run.fetch_max(run, Ordering::Relaxed);
        } else {
            c.gen_weak_minor_run.store(0, Ordering::Relaxed);
        }
        let weak_switch = c.gen_escalate_weak_minor.load(Ordering::Relaxed);
        let budget = self.gen_nursery_bytes.load(Ordering::Relaxed);
        let escalate = gen_escalate_after_minor(bytes_freed, budget, weak_switch);
        if escalate && bytes_freed != 0 {
            c.gen_weak_minor_escalations.fetch_add(1, Ordering::Relaxed);
        }
        escalate
    }

    /// zt t14/gn5 (`CRATONVM_ZGC_GEN_EMPTY_MINOR_NEEDS_PROMOTION`, default ON
    /// inside the generational mode, `=0` kill): must the zr escalation of a
    /// young cycle that freed nothing be withheld? Charged (and `true`) only
    /// when the cycle freed nothing, saw a live nursery (`bytes_copied > 0`:
    /// it examined something and all of it survived), took the young-aged
    /// sweep (it owns every non-old base, so no dead young object is left for
    /// a major), and nothing was promoted since the last major beyond that
    /// major's own seed (`gen_promoted_at_major`).
    ///
    /// # Why (zt t12 defect 3, `gn4_SF4_1*`)
    ///
    /// HotSpot 25's generational director has no "a minor freed nothing"
    /// rule: a minor is followed by a major only by `rule_major_*`
    /// (`Warmup`, `Proactive`, `Timer`, and the cost-based
    /// `rule_major_allocation_rate`, which weighs the old generation's
    /// garbage -- its used bytes past the last old mark's live bytes) or an
    /// allocation stall for old. A program that fills the heap with LIVE data
    /// after a `System.gc()` (`Zt12Gn4SoftFree minor`: 200 MiB of 1 MiB arrays
    /// held) gets minors only on HotSpot (`attempts=1 minors=1 majors=0`);
    /// here the first minor of the fill freed nothing and forced
    /// `Major Collection (Allocation Rate)` on every attempt
    /// (`attempts=8 spoiled=8 minors=2 majors=1`). With nothing promoted
    /// since the major, the old generation holds no garbage a major could
    /// free that HotSpot's estimate would see (objects old at the major that
    /// died since are invisible to both), so that major is pure cost.
    ///
    /// # What still escalates
    ///
    /// A minor that freed nothing and promoted something (or after one that
    /// did, in the same major epoch); one that examined nothing
    /// (`bytes_copied == 0`: the impossible-floor state of
    /// `a_nursery_floor_above_the_cursor_recovers_through_a_major`); the
    /// floor-bounded sweep arm (dead young objects below the floor are only
    /// a major's). Allocation failure, the minor ceiling and every other
    /// `force_major` reason are untouched, so a heap that really fills still
    /// gets its major from the allocation that failed.
    pub fn gen_empty_minor_escalation_skipped(
        &self,
        bytes_freed: usize,
        bytes_copied: usize,
        young_aged_sweep: bool,
    ) -> bool {
        let c = &self.counters;
        if bytes_freed != 0
            || bytes_copied == 0
            || !young_aged_sweep
            || !c.gen_empty_minor_needs_promotion.load(Ordering::Relaxed)
        {
            return false;
        }
        let promoted = c
            .gen_promoted_since_major
            .load(Ordering::Relaxed)
            .saturating_sub(c.gen_promoted_at_major.load(Ordering::Relaxed));
        if promoted != 0 {
            return false;
        }
        c.gen_empty_minor_escalations_skipped.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// zt t14/gn5: young cycles that freed nothing whose escalation
    /// `CRATONVM_ZGC_GEN_EMPTY_MINOR_NEEDS_PROMOTION` withheld
    /// (`[GC] zgc-gen-cost:` `empty_minor_escalations_skipped=`).
    pub fn gen_empty_minor_escalations_skipped(&self) -> usize {
        self.counters
            .gen_empty_minor_escalations_skipped
            .load(Ordering::Relaxed)
    }

    /// zt t14/gn5: `CRATONVM_ZGC_GEN_EMPTY_MINOR_NEEDS_PROMOTION` for THIS
    /// heap (tests).
    pub fn set_gen_empty_minor_needs_promotion(&self, on: bool) {
        self.counters
            .gen_empty_minor_needs_promotion
            .store(on, Ordering::Relaxed);
    }

    /// zt t7/tg (GN-12): `[GC] zgc-gen-cost:` -- `(weak minors, longest run of
    /// weak minors, majors the opt-in escalation forced)`.
    pub fn gen_weak_minor_report(&self) -> (usize, usize, usize) {
        let c = &self.counters;
        (
            c.gen_weak_minors.load(Ordering::Relaxed),
            c.gen_weak_minor_max_run.load(Ordering::Relaxed),
            c.gen_weak_minor_escalations.load(Ordering::Relaxed),
        )
    }

    /// zt t7/tg (GN-12): the opt-in weak-minor escalation, per heap (tests;
    /// the VM reads `CRATONVM_ZGC_GEN_ESCALATE_WEAK_MINOR` at construction).
    pub fn set_gen_escalate_weak_minor(&self, on: bool) {
        self.counters
            .gen_escalate_weak_minor
            .store(on, Ordering::Relaxed);
    }

    /// zt t6/tc (GN-5): whole-heap generational sweeps that tenured every
    /// survivor for a `System.gc()` (`[GC] zgc-gen-cost:` `system_gc_tenures=`).
    pub fn gen_tenure_all_sweeps(&self) -> usize {
        self.counters.gen_tenure_all_sweeps.load(Ordering::Relaxed)
    }

    /// `[GC] zgc-gen-cost:` -- `(young row, major row)`, columns as
    /// `GEN_COST_*`, plus `(complement sweeps, complement refusals, last
    /// refusal, majors by old growth, element granules scanned, element
    /// granules carded, majors by full request)` -- the last zt t3/gn (its
    /// `majors by headroom` column went with the retired rule, zt t14/gn5).
    pub fn gen_cost_report(
        &self,
    ) -> ([u64; GEN_COST_COLUMNS], [u64; GEN_COST_COLUMNS], [usize; 7]) {
        let c = &self.counters;
        (
            c.gen_cost.row(true),
            c.gen_cost.row(false),
            [
                c.gen_young_complement_sweeps.load(Ordering::Relaxed),
                c.gen_young_complement_refusals.load(Ordering::Relaxed),
                c.gen_young_complement_refusal.load(Ordering::Relaxed),
                c.gen_majors_by_old_growth.load(Ordering::Relaxed),
                c.gen_element_granules_scanned.load(Ordering::Relaxed),
                c.gen_element_cards_converted.load(Ordering::Relaxed),
                c.gen_majors_by_request.load(Ordering::Relaxed),
            ],
        )
    }

    /// The JIT inline start-bit store's published `blocked` word, if THIS heap
    /// owns the table (`None`: unpublished -- the switch is off, a forensic
    /// mode is on, or another heap owns it). `[GC] zgc-gen-cost:` prints it as
    /// the positive control for `CRATONVM_ZGC_GEN_INLINE_ANNOUNCE`: on a
    /// generational heap it reads 1 only under `=0` (or while a concurrent
    /// mark runs at shutdown).
    pub fn jit_inline_announce_blocked_report(&self) -> Option<bool> {
        let table = &cratonvm_gc_core::tlab::JIT_ZGC_ANNOUNCE;
        (self.arena_base != 0
            && table.owner.load(Ordering::Acquire) == self.arena_base
            && table.words.load(Ordering::Acquire) != 0)
            .then(|| table.blocked.load(Ordering::Acquire) != 0)
    }

    /// Card the DESTINATION of every old object a relocation moved.
    ///
    /// # Why a card cannot simply survive a slide
    ///
    /// A card is a page id plus a byte offset, so it names a **location**. Object
    /// ages travel in the header and are unaffected, so the generation split is
    /// still correct after a slide -- it is only the remembered set that has been
    /// invalidated, which is what would have made the failure silent.
    ///
    /// # Why the destinations alone are enough, and O(moved) not O(live)
    ///
    /// Work through the four cases against the post-slide heap:
    ///
    /// 1. **Old, carded, did not move.** Its bit is still at its own address.
    ///    Correct, untouched.
    /// 2. **Old, carded, moved.** Its bit is at the vacated address and there is
    ///    none at the new one -- the **only** case that loses an edge, and the
    ///    one this fixes.
    /// 3. **Old, not carded, moved.** It had no young reference when the last
    ///    young cycle cleaned it, so it needs no card. Carding it anyway would
    ///    be harmless; not carding it is correct.
    /// 4. **A survivor that slid INTO a vacated address whose bit is set.** It
    ///    inherits a card it did not earn -- a false positive, which
    ///    [`Self::young_extra_roots`] resolves by re-scanning it and keeping the
    ///    card only if it really does hold a young reference.
    ///
    /// So the whole repair is case 2, and the pointer map names exactly those
    /// objects. The first version of this cleared the table and re-carded the
    /// entire old generation, which is correct by over-approximation and cost a
    /// full pass over the live set per relocating cycle: the 2026-08-17 run
    /// recorded **4.8M re-cards over three collections** with relocation on,
    /// which is the same order as the mark the phase exists to avoid.
    ///
    /// Counted, because a configuration where relocation moves objects on every
    /// cycle should show that in a number rather than in a wall-clock mystery.
    pub fn recard_relocated_old_objects(
        &self,
        map: &cratonvm_types::PointerMap,
        promotion_age: u8,
    ) -> usize {
        let mut carded = 0usize;
        for (_from, to) in map.iter() {
            // The AGE at the destination, which is the object's own header and
            // travelled with it. A young destination needs no card: a young cycle
            // traces it.
            if !self.addr_is_young(*to, promotion_age) {
                self.card_object(*to);
                carded += 1;
            }
        }
        self.counters
            .gen_recards_after_relocation
            .fetch_add(carded, Ordering::Relaxed);
        carded
    }

    /// Throw the remembered set and the barrier's arming away.
    ///
    /// Object ages are deliberately NOT cleared: they are in the headers, they
    /// are still true, and clearing them would need a pass over the live set
    /// this function does not have. What it does guarantee is that no card
    /// survives -- so the caller must either re-card
    /// ([`Self::recard_all_old_objects`]) or accept that the next cycle is a
    /// full one, which is what a disarmed barrier forces: with
    /// `has_old_objects` down, `young_cycle` is false.
    pub fn reset_generational_state(&self) {
        // Ages survive the reset (see above), the bitmap's maintenance does not.
        self.invalidate_old_bits();
        for page in self.remembered.page_ids() {
            self.remembered.remove(page);
        }
        self.has_old_objects.store(false, Ordering::Relaxed);
        // Lowered AFTER the truth, so the JIT gate is never permissive before
        // the state it mirrors is. See `JitRefStoreGates`.
        //
        self.store_jit_gate(|g| &g.post_active, 0);
    }

    /// Turn young-only collections on or off for THIS heap.
    ///
    /// The process-wide default comes from `CRATONVM_ZGC_GENERATIONAL`; this is
    /// how a test drives the phase, and how a host owning two heaps configures
    /// them differently.
    pub fn set_generational_enabled(&self, on: bool) {
        // The JIT's inline start-bit store does not `note_young_page`: closed
        // before a generational heap is, reopened only after it is not (and
        // only if no mark is running). Round 9 wave 8, `zgc8`.
        //
        // zr z2/g2: only under `CRATONVM_ZGC_GEN_INLINE_ANNOUNCE=0`. The carve
        // already marks the chunk's grains young and no young grain is cleared
        // while the chunk's ownership epoch holds -- see
        // `zgc_gen_inline_announce_enabled` -- so on the default arm the
        // generational mode no longer closes the inline store.
        if on && !zgc_gen_inline_announce_enabled() {
            self.set_jit_inline_announce_blocked(true);
        }
        // A chunk carved while the mode was OFF has no young grains marked, so
        // its owned-chunk records must not survive the switch: bumping the
        // ownership epoch sends every thread's next allocation in such a chunk
        // through the helper (`note_young_page`) until its next refill carves
        // a marked one. Belt and braces -- a young cycle needs an old object,
        // and promotion needs a collection, which bumps the epoch anyway.
        if on && !self.generational_enabled.load(Ordering::Relaxed) {
            vm_tlab::invalidate_owned_vm_tlab_chunks();
        }
        // Ages are bumped only while the mode is on, so a toggle can leave
        // header ages the old-start bitmap never saw.
        if self.generational_enabled.load(Ordering::Relaxed) != on {
            self.invalidate_old_bits();
        }
        self.generational_enabled.store(on, Ordering::Relaxed);
        if !on || zgc_gen_inline_announce_enabled() {
            self.set_jit_inline_announce_blocked(
                self.mark_active.load(Ordering::Relaxed) && self.mark_bits.is_none(),
            );
        }
        // ---- G2c: THE NURSERY HAS TO BE WHERE ALLOCATION GOES -------------
        //
        // `Arena::alloc` serves the free list before the bump cursor, and that is
        // the right default -- after a sweep that could not move survivors, hole
        // reuse is the only thing keeping the arena from ratcheting. It is the
        // wrong default for a nursery: a young cycle's sweep is bounded below by
        // `gen_young_floor`, so an object the free list places BELOW the floor is
        // in the old region and no young cycle reclaims it. It waits for a major,
        // which is precisely the cost G2a had to document.
        //
        // Bump-first closes that. It is a preference and not a wall -- see
        // `Arena::prefer_bump` -- so it cannot turn a servable allocation into an
        // OutOfMemoryError, only change which space serves it; and the holes below
        // the floor are recovered by the same slide that promotes the nursery's
        // survivors out.
        //
        // Set here rather than in the constructor so the flag and the arena
        // cannot disagree: every path that turns the mode on or off, including a
        // test, goes through this one method.
        self.arena.lock().set_prefer_bump(on);
        if !on {
            self.reset_generational_state();
        }
        // zt t6/tc (GN-7): the JIT's young floor follows the mode. Raising it
        // (mode on) is sound at any time: the post gate stays 0 until a
        // promotion, and every promotion cards the promoted object. Lowering
        // it (mode off) follows the reset, which has already lowered the post
        // gate.
        self.publish_jit_young_floor(self.promotion_age());
    }

    /// zt t6/tc (GN-7): store the JIT reference-store plan's young floor for
    /// promotion age `promo` (see [`zgc_jit_young_floor`]).
    pub fn publish_jit_young_floor(&self, promo: u8) {
        self.store_jit_gate(
            |g| &g.young_floor,
            zgc_jit_young_floor(self.generational_enabled(), u32::from(promo)),
        );
    }

    /// Does generational mode, by itself, close the JIT's inline start-bit
    /// store? Only under `CRATONVM_ZGC_GEN_INLINE_ANNOUNCE=0` (zr z2/g2; see
    /// `zgc_gen_inline_announce_enabled`). The `blocked` word's other half is
    /// `mark_active`.
    pub fn gen_blocks_inline_announce(&self) -> bool {
        self.generational_enabled.load(Ordering::Relaxed) && !zgc_gen_inline_announce_enabled()
    }

    /// Is generational mode on for this heap?
    pub fn generational_enabled(&self) -> bool {
        self.generational_enabled.load(Ordering::Relaxed)
    }

    /// Collections an object must survive before it is old. Clamped into
    /// `1..=MAX_GC_AGE` on read -- see [`Self::promotion_age`].
    ///
    /// zt t6/tc (GN-7): the JIT's young floor mirrors the promotion age, so a
    /// LOWER age publishes its floor FIRST (a compiled store must never skip
    /// the helper for a receiver the helper would card) and a higher age
    /// publishes it AFTER the age.
    pub fn set_gen_promotion_age(&self, age: u32) {
        let new = age.clamp(1, cratonvm_types::MAX_GC_AGE as u32) as u8;
        if new < self.promotion_age() {
            self.publish_jit_young_floor(new);
            self.gen_promotion_age.store(age, Ordering::Relaxed);        } else {
            self.gen_promotion_age.store(age, Ordering::Relaxed);
            self.publish_jit_young_floor(new);        }
        // The bitmap's "old" is relative to the age it was built against.
        self.invalidate_old_bits();
    }

    /// zt t6/tc (GN-7): the young floor the JIT reads (tests).
    pub fn jit_young_floor(&self) -> u8 {
        self.jit_ref_store_gates.young_floor.load(Ordering::Acquire)
    }

    /// The old-start bitmap's counters -- `(enabled, valid, premarks,
    /// fallbacks, young_aged_sweeps, below_floor_visited, below_floor_freed_bytes,
    /// verify_unsound, verify_imprecise)`. zr z1/gn; `[GC] zgc-gen-old-bits:`.
    pub fn gen_old_bits_stats(&self) -> [usize; 9] {
        let c = &self.counters;
        [
            c.gen_old_bits_on.load(Ordering::Relaxed) as usize,
            c.gen_old_bits_valid.load(Ordering::Relaxed) as usize,
            c.gen_old_bits_premarks.load(Ordering::Relaxed),
            c.gen_old_bits_fallbacks.load(Ordering::Relaxed),
            c.gen_young_aged_sweeps.load(Ordering::Relaxed),
            c.gen_below_floor_visited.load(Ordering::Relaxed),
            c.gen_below_floor_freed_bytes.load(Ordering::Relaxed),
            c.gen_old_bits_verify_unsound.load(Ordering::Relaxed),
            c.gen_old_bits_verify_imprecise.load(Ordering::Relaxed),
        ]
    }

    /// The age-filtered nursery census (`CRATONVM_ZGC_NURSERY_CENSUS_AGE=1`):
    /// the same report shape as [`Self::nursery_survivor_report`], counting
    /// only survivors that were young when their cycle began.
    pub fn nursery_survivor_young_report(&self) -> generation::ZNurserySurvivorReport {
        self.counters.gen_nursery_survivors_young.report()
    }

    /// Is the age-filtered census on for this heap?
    pub fn nursery_census_age_enabled(&self) -> bool {
        self.counters.gen_census_age_on.load(Ordering::Relaxed)
    }

    /// Young cycles allowed between whole-heap ones -- see
    /// [`zgc_gen_minors_per_major`] for why there has to be a ceiling.
    pub fn set_gen_minors_per_major(&self, n: usize) {
        self.gen_minors_per_major.store(n, Ordering::Relaxed);
    }

    /// Is `obj_addr` carded right now?
    ///
    /// A test that asserts on `remembered_edge_count()` alone is asserting on a
    /// TOTAL, and the total moves for reasons the test did not cause: promotion
    /// cards the object it promotes, so a cycle that promotes anything changes
    /// it. Asking about one object is the question the tests actually have.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn is_carded_for_test(&self, obj_addr: usize) -> bool {
        let base = self.arena_base;
        if base == 0 || obj_addr < base {
            return false;
        }
        let page = ((obj_addr - base) / Self::Z_LOGICAL_PAGE_BYTES) as u64;
        let offset = (obj_addr - base) % Self::Z_LOGICAL_PAGE_BYTES;
        self.remembered
            .get(page)
            .is_some_and(|set| set.is_remembered(offset))
    }

    /// Microseconds the last collection spent sweeping -- `[GC] zgc-pause:`'s
    /// `sweep_us`, for a test that wants the phase rather than the pause.
    pub fn last_sweep_us(&self) -> u64 {
        self.counters.last_sweep_us.load(Ordering::Relaxed)
    }

    /// Generational counters -- `(young_cycles, minors_since_major,
    /// old_retained, remembered_roots, promotions, recards_after_relocation)`.
    ///
    /// `old_retained` is the one that says whether the feature did anything; see
    /// the field's own note.
    pub fn generational_stats(&self) -> (usize, usize, usize, usize, usize, usize) {
        (
            self.counters.young_cycles.load(Ordering::Relaxed),
            self.minors_since_major.load(Ordering::Relaxed),
            self.counters.gen_old_retained.load(Ordering::Relaxed),
            self.counters.gen_remembered_roots.load(Ordering::Relaxed),
            self.counters.gen_promotions.load(Ordering::Relaxed),
            self.counters
                .gen_recards_after_relocation
                .load(Ordering::Relaxed),
        )
    }

    /// The nursery's counters -- `(sweep_skipped, young_floor, old_live_bytes)`.
    ///
    /// `sweep_skipped` is the engagement counter: see the field. Zero with
    /// `young_cycles > 0` means the floor never moved and every young cycle swept
    /// the whole registry.
    pub fn nursery_stats(&self) -> (usize, usize, usize) {
        (
            self.counters.gen_sweep_skipped.load(Ordering::Relaxed),
            self.gen_young_floor.load(Ordering::Relaxed),
            self.gen_old_live_bytes.load(Ordering::Relaxed),
        )
    }

    /// The nursery survivor census — `(cycles, pages, pages_with_survivor,
    /// percent, cycles_observed, geometry_rejected)`.
    ///
    /// **Read the last two first.** `cycles` alone collapses three different
    /// situations into one zero, and only one of them is a fact about the heap:
    ///
    /// * `cycles_observed == 0` — nothing ever called the census. A statement
    ///   about the **wiring**, never about the workload.
    /// * `geometry_rejected != 0` — a caller handed over a frame that does not
    ///   describe a nursery (typically an unset young floor, i.e. `floor`
    ///   below the grid base). The cycles behind it are not evidence, and
    ///   `generation.rs` has already logged a `cratonvm::gc::guard` error
    ///   naming the frame. Before wave 3 such a frame was silently *clamped*,
    ///   which redefined the nursery as the whole arena and counted every
    ///   long-lived old object as a nursery survivor — reading as `Refuted`
    ///   for a reason with nothing to do with the workload.
    /// * `cycles == 0` with `cycles_observed > 0` and `geometry_rejected == 0`
    ///   — the honest "the nursery never filled". The census deliberately does
    ///   not count an empty nursery as a cycle.
    ///
    /// Only past those gates is `percent` admissible, and only with
    /// `cycles >= 3`: **`percent >= 90` refutes
    /// `docs/feature-designs/zgc-architecture.md`**,
    /// `<= 60` supports it. See [`generation::ZNurserySurvivorCensus`] for the
    /// full rule, and [`Self::gen_nursery_survivors`] for why the statistic is a
    /// bit per page rather than a survival rate.
    ///
    /// *The last two elements apply
    /// `docs/feature-designs/zgc-architecture.md`.*
    pub fn nursery_survivor_stats(&self) -> (u64, u64, u64, u64, u64, u64) {
        let r = self.gen_nursery_survivors.report();
        (
            r.cycles,
            r.pages,
            r.pages_with_survivor,
            r.survivor_page_percent(),
            r.cycles_observed,
            r.geometry_rejected,
        )
    }

    /// The whole [`generation::ZNurserySurvivorReport`], including the
    /// **per-cycle series** [`Self::nursery_survivor_stats`]'s tuple cannot
    /// carry.
    ///
    /// # Read this, not the aggregate, before quoting a verdict
    ///
    /// *Wave 4, applying `handoff-s-nursery-census-per-cycle.md`.*
    /// `survivor_page_percent()` sums `pages` and `pages_with_survivor` across
    /// cycles, and on this heap those cycles are **not** independent
    /// observations of a fresh nursery:
    ///
    /// 1. [`Self::gen_young_floor`] moves only at a whole-heap collection, so
    ///    "the nursery" is everything allocated since the last **major**, not
    ///    since the last young cycle;
    /// 2. promotion here is a header label rather than a copy into another
    ///    space, so a promoted — genuinely old — object still sits inside
    ///    `[floor, end)` and is still counted by `note_survivor` at
    ///    [`Self::gen_nursery_survivors`]'s call site. The probe that produced
    ///    the published figures recorded **2.05M promotions**;
    /// 3. so the per-page bit is monotone within a major epoch, and later
    ///    cycles — which examine the most pages — dominate the sum.
    ///
    /// The aggregate is therefore biased **toward `Refuted`** by an amount
    /// that has nothing to do with the workload.
    /// [`generation::ZNurserySurvivorReport::ratchet_margin`] is the
    /// admissibility test and `first_cycle_percent` is the closest thing this
    /// census has to a *marginal* reading. `verdict()` is deliberately
    /// unadjusted — the thresholds were fixed before the runs, and re-scoring
    /// them now would be the same offence in the other direction.
    ///
    /// This matters because `proposal-d-real-young-space.md` was marked
    /// **REFUTED** on that aggregate and the refutation was propagated to the
    /// roadmap and the round summary. Lane S's
    /// `gc/tests/zgc_s_nursery_census_ratchet.rs` exhibits a workload with a
    /// constant 1-in-5 marginal survivor rate — which would strongly *support*
    /// the proposal — that this census scores `Refuted`. Until the measurement
    /// is re-run with `survivor_pct_first` recorded, the honest standing of
    /// that proposal is *refuted on an aggregate whose admissibility has not
    /// been checked*.
    pub fn nursery_survivor_report(&self) -> generation::ZNurserySurvivorReport {
        self.gen_nursery_survivors.report()
    }

    /// Survivors promoted by a slide -- G2's engagement counter. See
    /// [`Self::gen_promotions_by_slide`].
    /// Zero only a dead object's header on a young sweep — see
    /// [`zgc_sweep_header_zero`]. Seeded from `CRATONVM_ZGC_SWEEP_HEADER_ZERO`.
    pub fn set_gen_header_zero_only(&self, on: bool) {
        self.gen_header_zero_only.store(on, Ordering::Relaxed);
    }

    /// Merge adjacent dead spans on a young sweep — see [`zgc_sweep_dead_runs`].
    /// Seeded from `CRATONVM_ZGC_SWEEP_DEAD_RUNS`.
    pub fn set_gen_dead_runs_enabled(&self, on: bool) {
        self.gen_dead_runs_enabled.store(on, Ordering::Relaxed);
    }

    /// `(bytes_not_memset, free_list_runs, dead_objects_those_runs_covered)` —
    /// the engagement counters for the two young-sweep cost reductions.
    ///
    /// Read them the way their fields say: `bytes_not_memset == 0` with
    /// `young_cycles > 0` means the header-only zeroing is inert, and
    /// `runs == objects` means the run merge is.
    /// Dead objects whose header the complement sweep left unwritten under
    /// `CRATONVM_ZGC_SWEEP_SKIP_DEAD_HEADERS` (zr z1/al), cumulative.
    pub fn sweep_dead_headers_skipped(&self) -> usize {
        self.counters
            .sweep_dead_headers_skipped
            .load(Ordering::Relaxed)
    }

    /// `(prune_key_cycles, keys_probed, keys_dead, region_words_skipped,
    /// mark_blocks_forgotten)` for `CRATONVM_ZGC_SWEEP_LIVE_PROPORTIONAL` (zr
    /// z5/sw), cumulative. The first is the key probe's engagement (> 0 on any
    /// run with an inflated monitor and the switch on, 0 at `=0`); the fourth
    /// is the region step's (> 0 only with the dead-header store also off);
    /// the fifth says the per-cycle summary reset ran at all.
    pub fn sweep_live_proportional_stats(&self) -> (usize, usize, usize, usize, usize) {
        let c = &self.counters;
        (
            c.sweep_prune_key_cycles.load(Ordering::Relaxed),
            c.sweep_prune_keys_probed.load(Ordering::Relaxed),
            c.sweep_prune_keys_dead.load(Ordering::Relaxed),
            c.sweep_region_words_skipped.load(Ordering::Relaxed),
            c.mark_summary_blocks_forgotten.load(Ordering::Relaxed),
        )
    }

    pub fn gen_sweep_cost_stats(&self) -> (usize, usize, usize) {
        (
            self.counters.gen_zero_bytes_skipped.load(Ordering::Relaxed),
            self.counters.gen_dead_runs.load(Ordering::Relaxed),
            self.counters.gen_dead_objects.load(Ordering::Relaxed),
        )
    }

    pub fn promotions_by_slide(&self) -> usize {
        self.counters
            .gen_promotions_by_slide
            .load(Ordering::Relaxed)
    }

    /// Bytes promoted out of the young generation, cumulative — the arm
    /// `VmHeap::bytes_promoted_total` delegates to for this collector.
    ///
    /// # Why the answer is `0` outside `CRATONVM_ZGC_GENERATIONAL`, and why
    /// that zero is not the defect this closes
    ///
    /// Without the generational split there is no old generation to promote
    /// into, the sweep's `cfg.gen_on` arm never runs, and nothing is promoted —
    /// so `0` is the TRUTH rather than a missing counter. The defect was the
    /// dispatcher's catch-all answering the same `0` on a run where the split
    /// IS on and objects genuinely cross the age threshold; see
    /// `ZCounters::gen_promoted_bytes`.
    ///
    /// Counted at the sweep's aging site, so a byte is counted on the single
    /// cycle whose `age_survivor` carried the object across the promotion age,
    /// never again. `promotions_by_slide` counts a different event — survivors
    /// a RELOCATION moved below the nursery floor — and the two are not
    /// summable.
    pub fn bytes_promoted_total(&self) -> u64 {
        self.counters.gen_promoted_bytes.load(Ordering::Relaxed) as u64
    }

    /// `(collections the nursery trigger asked for, the nursery budget in bytes)`
    /// -- G2d's engagement counter. Zero triggers on a generational run means
    /// every collection still came from the whole-heap predicate, so young cycles
    /// are as rare as full collections were.
    /// Size the nursery directly, in bytes — see [`zgc_gen_nursery_percent`] for
    /// the percentage this is normally derived from, and `0` to disable the
    /// trigger.
    ///
    /// Per heap for the reason `set_relocation_enabled` is: the percentage is
    /// process-wide and latched on first read, so a test that set it would decide
    /// the question for every other test in the binary.
    pub fn set_gen_nursery_bytes(&self, bytes: usize) {
        self.gen_nursery_bytes.store(bytes, Ordering::Relaxed);
    }

    /// The worst the nursery has been over its budget when a collection ran —
    /// see [`Self::gen_nursery_overshoot_max`]. Read against the budget on
    /// [`Self::nursery_trigger_stats`]; alone it says nothing.
    pub fn nursery_overshoot_max(&self) -> usize {
        self.counters
            .gen_nursery_overshoot_max
            .load(Ordering::Relaxed)
    }

    pub fn nursery_trigger_stats(&self) -> (usize, usize) {
        (
            self.counters.gen_nursery_triggers.load(Ordering::Relaxed),
            self.gen_nursery_bytes.load(Ordering::Relaxed),
        )
    }

    /// zt t20/gn7 (D7): collection intervals in which
    /// `CRATONVM_ZGC_GEN_NURSERY_AFTER_WARMUP` held the nursery clause back
    /// because the warm-up steps were not spent (`[GC] zgc-nursery-trigger:`
    /// `nursery_warmup_held=`). Zero on a run with the switch off.
    pub fn nursery_warmup_held(&self) -> usize {
        self.counters
            .gen_nursery_warmup_held
            .load(Ordering::Relaxed)
    }

    /// zt t20/gn7: `CRATONVM_ZGC_GEN_NURSERY_AFTER_WARMUP` for THIS heap
    /// (tests).
    pub fn set_gen_nursery_after_warmup(&self, on: bool) {
        self.counters
            .gen_nursery_after_warmup
            .store(on, Ordering::Relaxed);
    }

    /// Apply the ZGC **load barrier** to one reference slot, in place.
    ///
    /// This is the read path Phase 4 is about. Given the address of an 8-byte
    /// reference word, it runs `barrier::load_barrier_fast_bad`; on the fast
    /// path (the overwhelmingly common case, and the only one reachable while
    /// no cycle is armed) it returns the word unchanged. On the slow path it
    /// calls `barrier::load_barrier_slow`, which forwards the offset through
    /// [`Self::forward`], publishes it to the marker if a mark is running, and
    /// **self-heals** the slot by CAS-ing the corrected colored word back.
    ///
    /// # Colored slots, and why the word returned here is not a pointer
    ///
    /// A colored word is `Z_COLORED_TAG | color | 42-bit OFFSET`. The barrier
    /// hands back a bare offset, so a caller wanting a machine address must
    /// add the heap base. That conversion is the reason
    /// `ZMarkContext::heap_base` had to stop being `None`.
    ///
    /// # Cost while no cycle is armed
    ///
    /// One relaxed load, one AND, one compare. `bad_mask` is derived from a
    /// good mask that stays at `Z_REMAPPED`, under which a plain pointer
    /// classifies as good — so an unarmed run takes the fast path on every
    /// reference read and touches nothing else.
    ///
    /// Returns the **machine address** the slot should be read as, or `None`
    /// for null.
    ///
    /// # Why `pub` and not private
    ///
    /// [`cratonvm_gc_core::gpu_critical::VmHeap::load_ref_slot_barriered`] -- the backend-
    /// dispatching seam the JIT read helpers call, which lives in a sibling
    /// module -- delegates its armed `Zgc` arm here rather than re-deriving the
    /// barrier. That is deliberate: this body is the single in-tree place that
    /// gets the OFFSET -> ADDRESS conversion right (`z_load` returns a bare
    /// 42-bit offset, and returning it uncorrected truncates silently -- see
    /// `zgc/relocate.rs` and `zgc/mark.rs`) and the single place that resolves
    /// the `Good(0)` null-versus-heap-offset-0 ambiguity. A second copy in
    /// `vm_heap.rs` would be a second chance to get either one wrong.
    ///
    /// Deliberately NOT `pub`: nothing outside this crate should reach the
    /// barrier except through `VmHeap`, because doing so is the per-site ZGC
    /// special case `docs/feature-designs/zgc-jit-load-barrier.md` forbids.
    #[inline]
    pub fn load_barrier_slot(&self, slot_addr: usize) -> Option<usize> {
        use barrier::ZBarrierContext;
        // SAFETY: the caller supplies the address of an 8-byte-aligned
        // reference word inside a live object.
        let slot = unsafe { &*(slot_addr as *const std::sync::atomic::AtomicU64) };

        // ---- THE GATE, and it is not an optimisation ---------------------
        //
        // The barrier cannot be run over an UNCOLORED slot. `ZFastPath::Good`
        // carries a bare 42-bit OFFSET, and the classifier decides good-vs-bad
        // by testing the metadata bits — but a plain machine pointer into this
        // arena is well above 2^42, so its bits 42-46 are address bits that the
        // classifier would read as colors and `address_mask` would truncate.
        // Every reference read would take the slow path and every one of them
        // would resolve to the wrong object.
        //
        // So the barrier runs only once slots actually hold colored words,
        // which is what flipping the good mask off `Z_REMAPPED` declares.
        // Nothing in a default run flips it. The cost of the gate on an
        // unarmed run is one relaxed load and a compare.
        if !self.load_barrier_armed() {
            let raw = slot.load(Ordering::Relaxed) as usize;
            return (raw != 0).then_some(raw);
        }

        // ---- zt t18/zb1 (A2a): an UNCOLORED word on an armed heap ---------
        //
        // Plain-store mode (`arm_load_barrier_plain_stores`, the only arming a
        // production heap does) arms the read side before any store colors,
        // so an armed slot holds a plain machine pointer or null. The gate's
        // hazard above -- a plain pointer's address bits read as colors -- is
        // what bit 63 (`Z_COLORED_TAG`) exists to prevent: a colored word
        // always carries it, a user-space pointer never does. So a word
        // without it is answered as the pointer it is, and only a tagged word
        // reaches the classifier. Reached only while armed.
        let census = zgc_load_barrier_census_level() != 0;
        if census {
            self.counters.lb_routed.fetch_add(1, Ordering::Relaxed);
        }
        let observed_now = slot.load(Ordering::Relaxed);
        if !vaddr::is_colored_word(observed_now) {
            return self.load_barrier_plain_word(observed_now, census);
        }
        if census {
            self.counters.lb_colored.fetch_add(1, Ordering::Relaxed);
        }

        let bad = self.bad_mask();
        let mask = self.address_mask();
        match barrier::load_barrier_fast_bad(slot, bad, mask) {
            barrier::ZFastPath::Good(offset) => {
                if slot.load(Ordering::Relaxed) == vaddr::Z_NULL {
                    return None;
                }
                // zt t18/zb1: the immutable envelope base, not `heap_base()`
                // (which takes the arena lock) -- `forward` states why a
                // barrier path must not take that lock; the two are one address.
                let base = self.arena_base as u64;
                Some(base.wrapping_add(offset) as usize)
            }
            barrier::ZFastPath::Bad(observed) => {
                let offset =
                    barrier::load_barrier_slow(slot, observed, self, barrier::ZBarrierKind::Load);
                // zt t21/zb4: no `offset == 0 => None` here. `observed` is a
                // TAGGED word (the plain-word arm above returned every
                // untagged one), so it is never null, and offset 0 is the
                // object at the arena base -- the null-versus-offset-0
                // ambiguity `ZFastPath::Good` has does not exist on this arm.
                // Unreachable while the good color is pinned at `Remapped`
                // (no stored word is bad); the color flip makes it live.
                let base = self.arena_base as u64;
                Some(base.wrapping_add(offset) as usize)
            }
        }
    }

    /// **The slot resolver** (roadmap A0, proposal A stage 0; zr z1/br): the
    /// address of the bare 8-byte reference word that slot `index` of `obj`
    /// holds, or `None` when this slot is not provably one.
    ///
    /// | shape | word | `None` when |
    /// |---|---|---|
    /// | reference array element | `obj + ARRAY_DATA_OFFSET + index * 8` | not a reference array, `index >= length`, a reference width other than 8 |
    /// | compact instance field | `obj + disp` from the object's own layout | the field's storage is not an 8-byte reference |
    /// | legacy instance field | `cell + FIELD_CELL_PAYLOAD64_OFFSET` | `index >= num_slots`, or the cell's tag dword is not `FIELD_CELL_TAG_OBJECT` |
    ///
    /// The legacy tag gate is the hazard proposal A §2 names: a zero-filled
    /// cell has tag `0`, and healing it would manufacture an `Int` whose
    /// payload is a heap address. A compact object whose layout no longer
    /// resolves is `None` (its `num_slots` is a field count, not a cell
    /// count). Statics are not heap slots on this backend and are not served.
    ///
    /// No behaviour change: nothing on a store or load path calls this yet
    /// except the armed reference-array read in `get_array_element`, which
    /// used to compute the element address with the 16-byte legacy
    /// `SLOT_SIZE` stride (zr z1/br finding: element `i` of an 8-byte-wide
    /// reference array lives at `i * 8`, so every index above 0 read, and
    /// healed, the wrong word -- past the end for `i >= len / 2`).
    ///
    /// `obj` must be a live registered base of this heap.
    pub fn resolve_ref_slot(&self, obj: ObjectRef, index: usize) -> Option<*mut u64> {
        let base = obj.as_ptr() as usize;
        let header = self.header(obj);
        if header.kind() == ObjectKind::Array {
            if header.element_type() != ArrayElementType::Reference
                || index >= header.array_length() as usize
                || cratonvm_types::element_byte_size(ArrayElementType::Reference) != 8
            {
                return None;
            }
            return Some((base + ARRAY_DATA_OFFSET + index * 8) as *mut u64);
        }
        if let Some((off, storage)) = zgc_compact_field_storage(header, index) {
            return (storage.is_reference() && storage.size_runtime() == 8)
                .then_some((base + off) as *mut u64);
        }
        if cratonvm_types::is_compact_object(header) || index >= header.num_slots() as usize {
            return None;
        }
        let cell = base + HEADER_SIZE + index * SLOT_SIZE;
        // SAFETY: `index < num_slots` on a legacy body, so `cell` is the start
        // of a live 16-byte `Value` cell; its tag dword is 4-aligned.
        let tag = unsafe {
            (*((cell + cratonvm_types::FIELD_CELL_TAG_OFFSET)
                as *const std::sync::atomic::AtomicU32))
                .load(Ordering::Relaxed)
        };
        (tag == cratonvm_types::FIELD_CELL_TAG_OBJECT)
            .then_some((cell + cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET) as *mut u64)
    }

    /// A reference compare-and-swap on one 8-byte slot of an ARMED heap, as
    /// heal-then-CAS (nio21 wave 9 L3,
    /// `docs/feature-designs/zgc-architecture.md`).
    ///
    /// `expected` and `new` are MACHINE addresses (`0` is null), as every
    /// `Value::Object` carries them. Answers `Some((witness, swapped))`, the
    /// witness being the machine address the slot held (`expected` on
    /// success), or `None` when this heap is not armed (the caller's plain
    /// bit-level CAS is the right one then: unarmed slots hold plain
    /// pointers) or `new` cannot be encoded in the 42-bit offset field.
    ///
    /// # Why a plain CAS is wrong while armed
    ///
    /// An armed slot may hold a coloured word for the very object `expected`
    /// names, in a stale colour: a bit-level compare against any one encoding
    /// fails spuriously, and the read-compare-write lock path it used to fall
    /// to loses a plain store that lands between its read and its write.
    ///
    /// # The loop
    ///
    /// 1. Heal the slot through [`Self::load_barrier_slot`] (forward, mark if
    ///    marking, recolour to the good colour -- exactly a load's work).
    /// 2. Read the raw word `w`. Only a GOOD word (or null) is decoded here; a
    ///    bad one means a store landed after the heal, so go back to 1.
    /// 3. `decode(w) != expected`: fail with `decode(w)` as the witness.
    /// 4. Otherwise `CMPXCHG(slot, w, good_color(new))`. A failure means some
    ///    other writer won the slot after step 2: go back to 1, because the
    ///    new word may still decode to `expected` (a recolour, not a store).
    ///
    /// Lock-free: every retry is caused by another thread's successful write.
    /// `good_color(new)` is `heal_color() | (new - heap_base)`, the encoding the
    /// heal itself writes, so the next load takes the fast path.
    ///
    /// # What it assumes, and why its one caller is default-OFF
    ///
    /// That every writer of an armed slot stores a coloured word (the
    /// coverage rule `barrier::classify_bad_masked` states): a plain machine
    /// pointer whose bits 42..=45 happen to be clear classifies as good and
    /// decodes, truncated, to a different offset. For reference-array ELEMENTS
    /// that holds since JIT round 23 wave 2 (lane L4): `set_array_element`
    /// stores [`Self::armed_store_word`] while armed, so the vm-side arms
    /// (`hw_cas_reference_array_element`, `ref_slot_cas_fallback`) serve the
    /// armed heap under `CRATONVM_ZGC_ARMED_REF_CAS` (default ON, kill `0`).
    /// FIELD slots of a plain-store armed heap are not coloured by their
    /// stores, so their CAS does not come here: it is one plain machine-word
    /// compare-exchange (vm `armed_ref_cas_route`, JIT r23 wave 6). On a
    /// colored-store heap (`CRATONVM_ZGC_LOAD_BARRIER=colored`) `set_field`
    /// colours them, so their CAS comes here too (zt t31/zc11).
    pub fn cas_ref_slot_healed(
        &self,
        slot_addr: usize,
        expected: usize,
        new: usize,
    ) -> Option<(usize, bool)> {
        use barrier::ZBarrierContext;
        if !self.load_barrier_armed() {
            return None;
        }
        // zt t18/zb1: a plain-store heap's slots hold plain pointers, and the
        // swap below would store a COLORED word into one -- which every
        // accessor that is not routed yet (compiled inline loads) would then
        // dereference. The caller's plain bit CAS is the right one there,
        // exactly as on an unarmed heap.
        if self.counters.lb_plain_stores.load(Ordering::Acquire) {
            return None;
        }
        let base = <Self as mark::ZMarkContext>::heap_base(self)? as usize;
        let address_mask = self.address_mask();
        let new_word = if new == 0 {
            vaddr::Z_NULL
        } else {
            let off = (new as u64).checked_sub(base as u64)?;
            if off & !address_mask != 0 {
                return None;
            }
            self.heal_color() | off
        };
        // SAFETY: the caller supplies an 8-byte-aligned reference slot inside
        // a live object, valid for writes (as for `load_barrier_slot`).
        let slot = unsafe { &*(slot_addr as *const AtomicU64) };
        loop {
            // 1. Heal. The answer is re-derived from the raw word below,
            // because only that word can be the CAS comparand.
            let _ = self.load_barrier_slot(slot_addr);
            // 2. The raw word, decoded only if good.
            let w = slot.load(Ordering::Acquire);
            let decoded = match barrier::classify_bad_masked(w, self.bad_mask(), address_mask) {
                barrier::ZFastPath::Good(_) if w == vaddr::Z_NULL => 0usize,
                // zt t18/zb1: an uncolored word is the plain pointer it is
                // (the load barrier answers it the same way); classifying its
                // address bits as colors would truncate it.
                _ if !vaddr::is_colored_word(w) => w as usize,
                barrier::ZFastPath::Good(off) => base.wrapping_add(off as usize),
                barrier::ZFastPath::Bad(_) => {
                    std::hint::spin_loop();
                    continue;
                }
            };
            // 3. A different object (or null): the CAS fails here.
            if decoded != expected {
                return Some((decoded, false));
            }
            // 4. Same object, possibly in any good encoding: swap that word.
            match slot.compare_exchange(w, new_word, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return Some((expected, true)),
                Err(_) => std::hint::spin_loop(),
            }
        }
    }

    /// The word an ARMED heap stores for the machine address `addr` (`0` is
    /// null): `heal_color() | (addr - heap_base)`, the good-coloured encoding
    /// the load barrier's heal and [`Self::cas_ref_slot_healed`] write, so the
    /// next barriered load takes the fast path. `None` when `addr` is not an
    /// address of this arena (its offset does not fit the 42-bit field).
    ///
    /// JIT round 23 wave 2, lane L4
    /// (`docs/feature-designs/zgc-architecture.md` item 1).
    pub fn armed_store_word(&self, addr: usize) -> Option<u64> {
        use barrier::ZBarrierContext;
        if addr == 0 {
            return Some(vaddr::Z_NULL);
        }
        let base = <Self as mark::ZMarkContext>::heap_base(self)?;
        let off = (addr as u64).checked_sub(base)?;
        if off & !self.address_mask() != 0 {
            return None;
        }
        Some(self.heal_color() | off)
    }

    /// The reference-ELEMENT store of an ARMED heap: element `index` of the
    /// reference array `obj` gets [`Self::armed_store_word`] of `value`, one
    /// 8-byte atomic store. `false` (nothing stored; the caller's plain store
    /// runs) when the barrier is not armed, the slot does not resolve, or the
    /// address is not this arena's.
    ///
    /// JIT round 23 wave 2, lane L4: `set_array_element` wrote a PLAIN machine
    /// pointer into an armed slot (`write_prim_element`). The armed read
    /// (`get_array_element` -> `load_barrier_slot`) classifies that word by
    /// bits 42..=46, which are address bits of a plain pointer, so it could
    /// decode to a different object; and the heal-then-CAS
    /// (`cas_ref_slot_healed`) assumes every writer of an armed slot stores a
    /// coloured word. With this, every element writer of an armed heap does:
    /// this accessor, the heal, the heal-then-CAS; the interpreter's quickened
    /// `aastore` and the JIT's inline `aastore` decline while armed, and the
    /// JIT `aastore` helper routes here while armed.
    pub fn armed_store_ref_element(
        &self,
        obj: ObjectRef,
        index: usize,
        value: Option<ObjectRef>,
    ) -> bool {
        if !self.load_barrier_armed() {
            return false;
        }
        // zt A2a (`CRATONVM_ZGC_LOAD_BARRIER` plain-store mode) arms with the
        // good color left `Remapped` and every store plain; a colored-store
        // heap writes its own word in `set_array_element`. Only a flipped
        // good color without colored stores (the tests' arming) lands here.
        if self.load_barrier_colored_stores()
            || self.barrier_good_mask.load(Ordering::Acquire) == vaddr::Z_REMAPPED
        {
            return false;
        }
        let Some(slot) = self.resolve_ref_slot(obj, index) else {
            return false;
        };
        let Some(word) = self.armed_store_word(value.map_or(0, |r| r.as_ptr() as usize)) else {
            return false;
        };
        // SAFETY: `resolve_ref_slot` answered the 8-aligned reference word of
        // element `index` of the live array `obj`.
        unsafe { (*(slot as *const AtomicU64)).store(word, Ordering::Release) };
        true
    }

    /// Fast raw read of compact `Reference` slot 0 (`referent`) when the load
    /// barrier is disarmed and no forwarding word is present, avoiding
    /// `ObjectRef::from_raw` / `Value` boxing on `Reference.get0()`.
    #[inline]
    pub fn fast_read_compact_ref_slot0(
        &self,
        obj_addr: usize,
        resolve_off: impl FnOnce(u32) -> Option<usize>,
    ) -> Option<usize> {
        if obj_addr < self.arena_base || obj_addr >= self.arena_end || (obj_addr & 7) != 0 {
            return None;
        }
        if self.load_barrier_armed() {
            return None;
        }
        // SAFETY: `obj_addr` is 8-byte aligned and inside the ZGC arena, and
        // `Reference.get0()` is only called on a live `Reference` receiver.
        let header = unsafe { &*(obj_addr as *const ObjectHeader) };
        if header.is_forwarded()
            || header.kind() != ObjectKind::Object
            || !cratonvm_types::is_compact_object(header)
        {
            return None;
        }
        let off = resolve_off(header.class_id.as_u32())?;
        // SAFETY: `off` was validated against this class's compact layout.
        let raw =
            unsafe { &*((obj_addr + off) as *const AtomicU64) }.load(Ordering::Relaxed) as usize;
        if raw != 0 {
            if self.mark_active.load(Ordering::Relaxed) {
                self.referent_keep_alive(raw);
            }
            if raw >= self.arena_base && raw < self.arena_end && (raw & 7) == 0 {
                // SAFETY: `raw` is 8-byte aligned inside the ZGC arena.
                let r_hdr = unsafe { &*(raw as *const ObjectHeader) };
                if r_hdr.is_forwarded() {
                    return None;
                }
            }
        }
        Some(raw)
    }

    /// Is the read-path load barrier armed?
    ///
    /// Only ever true when the good mask has been flipped off `Z_REMAPPED`,
    /// which nothing in a default run does. Exposed so a test can assert the
    /// unarmed cost claim rather than take it on trust.
    pub fn load_barrier_armed(&self) -> bool {
        self.barrier_armed.load(Ordering::Acquire)
    }

    /// zt t18/zb1 (zr1br A2a): **arm the read barrier with stores left plain**
    /// -- the first non-test caller of [`Self::set_barrier_color`], reached
    /// from [`Self::new_shared_with_initial`] under the opt-in
    /// `CRATONVM_ZGC_LOAD_BARRIER` (see [`ZLoadBarrierMode`]).
    ///
    /// The good color stays `Remapped`, the quiescent mask, so nothing is
    /// "bad" but a colored `Marked0`/`Marked1` word, and no production store
    /// writes one: every slot holds a plain pointer or null, which
    /// [`Self::load_barrier_slot`] answers verbatim (bit 63 clear). What the
    /// arming changes is ROUTING, which is the A2a exit: every read the heap
    /// serves goes through the seam, the JIT's read bounds are cleared (so
    /// compiled field reads take the helpers), the codegen gate is raised,
    /// and the raw fast arms that test `load_barrier_armed` decline.
    /// [`Self::cas_ref_slot_healed`] declines in this mode (its swap would
    /// store a colored word).
    ///
    /// `mark_on_load` is A3 in its additive form: while a concurrent mark is
    /// running, a plain word a routed load returns is published to the
    /// marker through the SATB ingress. SATB stays on, so this can only add
    /// floating garbage.
    ///
    /// # The safepoint obligation `set_barrier_color` states
    ///
    /// Discharged by WHEN this runs: at heap construction, before any Java
    /// thread exists and before the first method is compiled, so no inline
    /// sequence can be live. Any later caller inherits the obligation.
    ///
    /// Refused (`false`, nothing armed) under compressed oops, which
    /// `VmHeap::load_ref_slot_barriered` asserts never meets an armed heap.
    pub fn arm_load_barrier_plain_stores(&self, mark_on_load: bool) -> bool {
        if cratonvm_types::narrow_oop::narrow_oops_enabled() {
            return false;
        }
        self.counters
            .lb_mark_on_load
            .store(mark_on_load, Ordering::Release);
        self.counters.lb_plain_stores.store(true, Ordering::Release);
        self.set_barrier_color(Some(vaddr::ZColor::Remapped));
        true
    }

    /// Is this heap armed in plain-store mode? See
    /// [`Self::arm_load_barrier_plain_stores`].
    pub fn load_barrier_plain_stores(&self) -> bool {
        self.load_barrier_armed() && self.counters.lb_plain_stores.load(Ordering::Acquire)
    }

    /// zt t19/zb2 (zr1br A2b): **arm the read barrier with COLORED stores** --
    /// every reference store this heap's own accessors make
    /// (`set_field` / `set_field_no_satb`, `set_array_element`, and the JIT
    /// store helpers that delegate to them while this mode is on) writes the
    /// current good-colored word ([`Self::colored_store_word`]); every routed
    /// load decodes it ([`Self::load_barrier_slot`], whose plain-word arm still
    /// answers the plain pointers the GC's own writes and the not-yet-colored
    /// store sites leave behind).
    ///
    /// zt t20/zb3: **reachable from production** under the opt-in
    /// `CRATONVM_ZGC_LOAD_BARRIER=colored` ([`ZLoadBarrierMode::Colored`]).
    /// The readers that dereference a slot WITHOUT the barrier (zb2's R1-R9:
    /// the GC's mark, relocation, reference-processing, generational, census
    /// and diagnostic walks) read through
    /// `cratonvm_types::narrow_oop::read_ref_slot` or
    /// `cratonvm_types::read_compact_field`, which decode a colored word
    /// against the base this arming publishes
    /// (`narrow_oop::publish_colored_slot_base`). LEGACY 16-byte cells are
    /// not colored (their readers move whole `Value`s), so the colored words
    /// live only in compact reference fields and reference-array elements.
    ///
    /// The good color stays `Remapped` (no cycle flips it yet), so a stored
    /// word is `Z_COLORED_TAG | Z_REMAPPED | offset` and every routed load
    /// takes the barrier's fast path. Refused (`false`, nothing armed) under
    /// compressed oops, as the plain-store arming is, and when ANOTHER heap
    /// of this process already owns the decode base (one colored-store heap
    /// per process: the decode below `cratonvm-gc` has no heap handle). Same
    /// safepoint obligation as [`Self::arm_load_barrier_plain_stores`].
    pub fn arm_load_barrier_colored_stores(&self) -> bool {
        if cratonvm_types::narrow_oop::narrow_oops_enabled() {
            return false;
        }
        // zt t23/zb8 (stage 2f blocker (a)): refused when the arena is wider
        // than the 42-bit offset field, so every address of THIS heap encodes
        // (`colored_store_word`'s plain fallback then only ever answers an
        // address outside the arena, an object this collector never marks,
        // sweeps or moves): a colored store never leaves one of this heap's
        // objects behind as a plain word.
        if !colored_arena_fits_offset(self.committed_bytes()) {
            return false;
        }
        // zt t20/zb3: the decode base first, so no colored word can exist
        // before every walker can decode it.
        if !cratonvm_types::narrow_oop::publish_colored_slot_base(self.arena_base as u64) {
            return false;
        }
        self.counters
            .lb_mark_on_load
            .store(false, Ordering::Release);
        self.counters
            .lb_plain_stores
            .store(false, Ordering::Release);
        self.counters
            .lb_colored_stores
            .store(true, Ordering::Release);
        self.set_barrier_color(Some(vaddr::ZColor::Remapped));
        true
    }

    /// zt t19/zb2: is this heap armed with colored stores? `false` on every
    /// production heap (see [`Self::arm_load_barrier_colored_stores`]). One
    /// acquire load of the armed flag when unarmed, which is the default
    /// path's whole cost at each converted store site.
    #[inline]
    pub fn load_barrier_colored_stores(&self) -> bool {
        self.load_barrier_armed() && self.counters.lb_colored_stores.load(Ordering::Acquire)
    }

    /// zt t22/zb5 (stage 2e of `zgc-concurrent-collector-program-RETIRED-20261023.md`):
    /// **arm the color flip** on a colored-store heap -- from here on a
    /// concurrent cycle flips the good color to its mark bit at the mark-start
    /// pause ([`Self::color_flip_to_mark`]) and back to `Remapped` at the
    /// closing pause ([`Self::color_flip_to_remap`]), so the first load of
    /// each colored slot per phase takes the barrier's slow path
    /// (`barrier::load_barrier_slow`: mark if marking, self-heal with one CAS)
    /// instead of every load passing a pinned `Remapped`.
    ///
    /// Reached from [`Self::new_shared_with_initial`] under
    /// `CRATONVM_ZGC_COLOR_FLIP` (opt-in) and from tests. `false`, nothing
    /// armed, unless this heap is armed with colored stores and its compiled
    /// bad-mask cell could be published (another heap owns one).
    ///
    /// # What the flip changes, and what keeps it sound before stage 3
    ///
    /// * Every reader still answers the right ADDRESS for every color: the
    ///   GC readers (`narrow_oop::decode_colored_slot_word`, any color bit),
    ///   [`Self::load_barrier_slot`] (the `Bad` arm: identity forward, offset
    ///   0 is `base`), the JIT's flip arm (`jit/src/x64/licm.rs`,
    ///   `zgc_colored_flip_head_bytes`, which calls `jit_aaload` on a bad
    ///   word). Relocation is still the stop-the-world slide, which rewrites
    ///   every live slot and root, so a stale color's offset is current.
    /// * SATB stays on: the load barrier's marks during the phase are
    ///   ADDITIVE (floating garbage at worst), exactly as `=mark`'s.
    /// * The flips happen with every mutator stopped (the [`vaddr::ZGoodMask`]
    ///   ordering rule), and the cell is published BEFORE the good mask moves
    ///   ([`vaddr::ZCompiledBadMask`]'s rule).
    ///
    /// Same safepoint obligation as [`Self::arm_load_barrier_plain_stores`].
    pub fn arm_load_barrier_color_flip(&self) -> bool {
        if !self.load_barrier_colored_stores() {
            return false;
        }
        // Quiescent first (`Remapped` good), then visible to the JIT.
        self.counters
            .lb_bad_cell
            .publish_for_good(vaddr::Z_REMAPPED);
        let cell = self.counters.lb_bad_cell.word_address();
        if !cratonvm_types::narrow_oop::publish_colored_bad_mask_cell(cell) {
            return false;
        }
        self.counters.lb_color_flip.store(true, Ordering::Release);
        // zt t32 (2f): load-barrier marking rides on the flip (every load of
        // a word a mark has not visited is then bad, so its slow path marks).
        if zgc_load_barrier_marking_requested() {
            self.counters.lbm.store(true, Ordering::Release);
            self.counters.lbm_no_load_mark.store(
                false,
                Ordering::Release,
            );
        }
        true
    }

    /// zt t32 (2f, `CRATONVM_ZGC_LOAD_BARRIER_MARKING`): is load-barrier
    /// marking armed on this heap? One relaxed load.
    #[inline]
    pub fn load_barrier_marking(&self) -> bool {
        self.counters.lbm.load(Ordering::Relaxed)
    }

    /// zt t32 (2f): `(armed, colored_skipped, plain_logged)` for the
    /// `[GC] zgc-lbm:` line.
    pub fn load_barrier_marking_report(&self) -> (bool, usize, usize) {
        (
            self.load_barrier_marking(),
            self.counters.lbm_colored_skipped.load(Ordering::Relaxed),
            self.counters.lbm_plain_logged.load(Ordering::Relaxed),
        )
    }

    /// zt t32 (2f): the store side's pre-barrier under load-barrier marking,
    /// for a store about to overwrite the word at `slot` (an 8-byte
    /// reference word: a compact field, an array element, a legacy cell's
    /// payload). The word is read RAW -- never through the strong accessor,
    /// whose slow path would mark it -- and logged only when it is a non-null
    /// PLAIN word, which no load barrier marks (t23/zb8 report section 4.3:
    /// a mutator can hold a plain word's target only through a load that
    /// did not mark it). A colored word's deletion is not logged: its
    /// target, if a mutator holds it, was marked by the load that handed it
    /// out (a bad word) or was already marked / black (a good one).
    /// **Only while a mark is active**; the caller tests that.
    #[cold]
    pub fn lbm_pre_barrier_slot(&self, slot: *const u8) {
        // SAFETY: the caller's contract -- an 8-byte-aligned reference word
        // inside a live object (ZGC refuses compressed oops).
        let word = unsafe { (*(slot as *const AtomicU64)).load(Ordering::Acquire) };
        if word == 0 {
            return;
        }
        if vaddr::is_colored_word(word) {
            self.counters
                .lbm_colored_skipped
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.counters
            .lbm_plain_logged
            .fetch_add(1, Ordering::Relaxed);
        self.satb_pre_barrier(word as usize);
    }

    /// zt t32 (2f): [`Self::lbm_pre_barrier_slot`] for slot `index` of
    /// `obj`. A slot the resolver does not prove an 8-byte reference word (a
    /// primitive, a legacy cell with another tag) holds no reference, so
    /// there is nothing to log. **Only while a mark is active.**
    #[cold]
    pub fn lbm_pre_barrier(&self, obj: ObjectRef, index: usize) {
        if let Some(slot) = self.resolve_ref_slot(obj, index) {
            self.lbm_pre_barrier_slot(slot as *const u8);
        }
    }

    /// zt t22/zb5: is the color flip armed on this heap? See
    /// [`Self::arm_load_barrier_color_flip`].
    #[inline]
    pub fn load_barrier_color_flip(&self) -> bool {
        self.load_barrier_colored_stores() && self.counters.lb_color_flip.load(Ordering::Acquire)
    }

    /// zt t23/zb7 (stage 2f blocker (c) of
    /// `zgc-concurrent-collector-program-RETIRED-20261023.md`): keep a REFERENT the
    /// mutator just read out of a `java.lang.ref.Reference` alive for the
    /// running mark -- HotSpot's `Reference.get()` keep-alive (ZGC loads the
    /// referent with a keep-alive load barrier; G1 logs it, `G1ReferenceGet`).
    /// The marker hides referent slots during a cycle, so without this a
    /// referent stored behind an already-scanned object could be cleared and
    /// freed while strongly held.
    ///
    /// * No mark running: one relaxed load, return (as `satb_pre_barrier`).
    /// * Color flip armed (`CRATONVM_ZGC_COLOR_FLIP=1` on a colored-store
    ///   heap): the load barrier's own MARK step (`ZBarrierContext::mark_live`,
    ///   what `load_barrier_slow` runs for a bad word while marking) marks the
    ///   referent strongly -- whatever color the slot word had (a good word,
    ///   a bad word the load barrier already marked: idempotent; a plain word
    ///   the load barrier cannot mark: this is its only cover). That is the
    ///   keep-alive 2f needs once the SATB pre-barrier stops being the
    ///   mutator's marking contribution. Same ingress, same mark-end replay.
    /// * Otherwise: the SATB pre-barrier, byte for byte the old path.
    #[inline]
    pub fn referent_keep_alive(&self, addr: usize) {
        if !self.mark_active.load(Ordering::Relaxed) {
            return;
        }
        if self.load_barrier_color_flip() {
            self.referent_keep_alive_by_load_barrier(addr);
            return;
        }
        self.satb_pre_barrier(addr);
    }

    /// zt t23/zb7: the flip arm of [`Self::referent_keep_alive`].
    #[cold]
    pub fn referent_keep_alive_by_load_barrier(&self, addr: usize) {
        use barrier::ZBarrierContext;
        if addr == 0 {
            return;
        }
        // An address below the arena is not this heap's (the registry screen
        // in `mark_live` answers for the rest), exactly as the SATB arm's
        // registry screen drops it.
        let Some(off) = (addr as u64).checked_sub(self.arena_base as u64) else {
            return;
        };
        self.mark_live(off);
        if zgc_load_barrier_census_level() != 0 {
            self.counters
                .lb_referent_keep_alive
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// zt t22/zb5: the **mark flip**, in the mark-start pause
    /// ([`Self::start_concurrent_mark`], after the SATB barrier arms): the
    /// compiled bad-mask cell is published first, then the good color becomes
    /// the cycle's mark bit (`Marked0` / `Marked1`, alternating), so every
    /// `Remapped` word and last cycle's mark color are bad until a load heals
    /// them; a store during the phase writes the mark color
    /// (`colored_store_word` uses `heal_color()`). Roots hold decoded
    /// addresses, so nothing is recolored here. No-op unless the flip is
    /// armed. **Mutators stopped.**
    pub fn color_flip_to_mark(&self) {
        if !self.load_barrier_color_flip() {
            return;
        }
        let good = self
            .counters
            .lb_flip_mask
            .flip_to_mark_publishing(&self.counters.lb_bad_cell);
        self.barrier_good_mask.store(good, Ordering::Release);
        self.barrier_heal_mask.store(good, Ordering::Release);
        self.counters.lb_flips_mark.fetch_add(1, Ordering::Relaxed);
        // zt t31/zc11 (R4): a new mark; no slide skips by an older color.
        self.counters.rim_last_mark_color.store(0, Ordering::Relaxed);
    }

    /// zt t22/zb5: the **remap flip**, in the closing pause (when
    /// [`Self::finish_concurrent_mark`] closes the cycle, before the mark-end
    /// work, the sweep and the slide), and when a cycle is abandoned:
    /// `Remapped` is good again, so the slide's rewrite (`gc_colored_word`,
    /// `heal_color()`) writes `Remapped` words, and a slot the slide does not
    /// rewrite keeps its mark color and is healed by its first load (identity
    /// forward, no mark: `mark_active` is down). HotSpot flips at Pause
    /// Relocate Start; here mark end, reference processing, sweep and slide
    /// are one pause, so the earliest point in it is the same point. Taking
    /// it before the sweep also keeps zc2's deferred-sweep split
    /// (`defer_sweep_and_close_pause`) out of the question: both halves run
    /// with `Remapped` good. Idempotent (a no-op when `Remapped` is already
    /// good, i.e. on every stop-the-world cycle). **Mutators stopped**,
    /// except on `abandon_concurrent_mark`'s shutdown path, where the cell's
    /// publish-first rule makes a racing load err toward the slow path.
    pub fn color_flip_to_remap(&self) {
        if !self.counters.lb_color_flip.load(Ordering::Acquire) {
            return;
        }
        if self.barrier_good_mask.load(Ordering::Acquire) == vaddr::Z_REMAPPED {
            // zt t33: no mark color this pause (a stop-the-world mark healed
            // in Remapped). A color an earlier refused pause left unread must
            // not reach this pause's R4 slide or R6 plan: words naming their
            // objects read good already (`NoMarkColor`).
            self.counters.rim_last_mark_color.store(0, Ordering::Release);
            if self.counters.crl.enabled() && self.gc_log_enabled.load(Ordering::Relaxed) {
                eprintln!("[GC] zgc-crl-heal: flip-noop (Remapped already good)");
            }
            return;
        }
        // zt t31/zc11 (R4): the mark color this flip leaves, for the slide
        // of this pause (`remap_in_mark_begin_slide`).
        let marked = self.barrier_good_mask.load(Ordering::Acquire);
        if self.counters.rim.enabled() {
            self.counters
                .rim_last_mark_color
                .store(marked, Ordering::Release);
        }
        self.counters.lb_flip_mask.set_mark_complete();
        let good = self
            .counters
            .lb_flip_mask
            .flip_to_remap_publishing(&self.counters.lb_bad_cell);
        self.barrier_good_mask.store(good, Ordering::Release);
        // zt t33: until the plan's pause ends, heal and store in the mark
        // color this flip leaves (`barrier_heal_mask`). Only when R6 may plan:
        // a slide must write good words (`end_closing_heal_color`).
        let heal = if self.concurrent_relocate_may_plan() {
            marked
        } else {
            good
        };
        self.barrier_heal_mask.store(heal, Ordering::Release);
        if self.counters.crl.enabled() && self.gc_log_enabled.load(Ordering::Relaxed) {
            eprintln!("[GC] zgc-crl-heal: open heal={heal:#x} good={good:#x}");
        }
        self.counters.lb_flips_remap.fetch_add(1, Ordering::Relaxed);
    }

    /// zt t33: end the closing pause's heal color (`barrier_heal_mask`): heals
    /// and stores write the good color again. Called when a slide starts (its
    /// rewrite must write good words: a new address in the mark color would be
    /// forwarded as the old address of whatever moved from there), when a
    /// cycle is abandoned, and by the VM before it releases the mutators
    /// (`VmHeap::zgc_end_closing_heal_color`). Idempotent; one store.
    pub fn end_closing_heal_color(&self) {
        self.end_closing_heal_color_for("pause-end");
    }

    /// [`Self::end_closing_heal_color`], naming the caller on the
    /// `[GC] zgc-crl-heal:` line (`--verbose:gc`) when the window was open.
    pub fn end_closing_heal_color_for(&self, why: &'static str) {
        let good = self.barrier_good_mask.load(Ordering::Acquire);
        let was = self.barrier_heal_mask.swap(good, Ordering::AcqRel);
        if was != good && self.gc_log_enabled.load(Ordering::Relaxed) {
            eprintln!("[GC] zgc-crl-heal: close why={why} heal={was:#x} good={good:#x}");
        }
    }

    /// zt t19/zb2 (A2b): the colored word a reference store of the machine
    /// address `addr` writes on a colored-store heap: `Z_NULL` for null,
    /// otherwise `heal_color() | (addr - arena_base)` -- the encoding the
    /// barrier's own heal writes, so the next load takes the fast path.
    ///
    /// An address this arena cannot encode (outside it, or past the 42-bit
    /// offset field) is returned PLAIN: [`Self::load_barrier_slot`] answers an
    /// untagged word verbatim, so a plain word in a colored heap is read
    /// correctly by every routed reader. Callers test
    /// [`Self::load_barrier_colored_stores`] first; this function does not.
    #[inline]
    pub fn colored_store_word(&self, addr: usize) -> u64 {
        use barrier::ZBarrierContext;
        if addr == 0 {
            return vaddr::Z_NULL;
        }
        let off = (addr as u64).wrapping_sub(self.arena_base as u64);
        if addr < self.arena_base || off & !self.address_mask() != 0 {
            return addr as u64;
        }
        if zgc_load_barrier_census_level() != 0 {
            self.counters
                .lb_store_colored
                .fetch_add(1, Ordering::Relaxed);
        }
        self.heal_color() | off
    }

    /// `System.arraycopy` of `len` reference slots from `src_base[src_off..]`
    /// to `dst_base[dst_off..]` (array payload addresses) while the barrier is
    /// armed with colored stores: each source word through
    /// [`Self::load_barrier_slot`] (decode, heal, forward) and stored with
    /// [`Self::colored_store_word`], in memmove order (`backward` for an
    /// overlapping copy to a higher index). The per-slot work of
    /// `get_array_element` + `set_array_element`, minus their per-call
    /// validation and `Value` round trip; an auto-box wrapper is copied as the
    /// reference it is, as the unarmed raw copy does. `false`, nothing
    /// written, outside that state, with 4-byte reference slots, or while
    /// marking (whose pre-barriers the per-element accessors run). The caller
    /// validated both ranges and notes the store (`note_ref_store`).
    pub fn copy_reference_slots_colored(
        &self,
        src_base: usize,
        src_off: usize,
        dst_base: usize,
        dst_off: usize,
        len: usize,
        backward: bool,
    ) -> bool {
        if !self.load_barrier_colored_stores()
            || cratonvm_types::ref_element_size() != 8
            || self.mark_active()
        {
            return false;
        }
        for step in 0..len {
            let i = if backward { len - 1 - step } else { step };
            let addr = self.load_barrier_slot(src_base + (src_off + i) * 8);
            let word = self.colored_store_word(addr.unwrap_or(0));
            // SAFETY: the caller validated `dst_off + len` against the
            // destination's length; the slot is an 8-aligned payload word.
            unsafe {
                (*((dst_base + (dst_off + i) * 8) as *const AtomicU64))
                    .store(word, Ordering::Relaxed);
            }
        }
        true
    }

    /// zt t20/zb3: [`Self::colored_store_word`] for the COLLECTOR's own slot
    /// writes (the slide's rewrite pass): the same encoding, without the
    /// mutator store census (the caller counts `lb_gc_colored`). Callers test
    /// [`Self::load_barrier_colored_stores`] first.
    #[inline]
    pub fn gc_colored_word(&self, addr: usize) -> u64 {
        use barrier::ZBarrierContext;
        if addr == 0 {
            return vaddr::Z_NULL;
        }
        let off = (addr as u64).wrapping_sub(self.arena_base as u64);
        if addr < self.arena_base || off & !self.address_mask() != 0 {
            return addr as u64;
        }
        self.heal_color() | off
    }

    /// zt t19/zb2: the word a reference store of `value` writes into an
    /// 8-byte slot of a colored-store heap. `None` for a non-reference value
    /// (the caller's own path boxes or rejects it).
    #[inline]
    pub fn colored_store_word_of(&self, value: Value) -> Option<u64> {
        match value {
            Value::Object(o) => Some(self.colored_store_word(o.map_or(0, |r| r.as_ptr() as usize))),
            _ => None,
        }
    }

    /// zt t19/zb2: count one armed read that came through the JIT helpers'
    /// seam (`VmHeap::load_ref_slot_barriered`), under the census. The
    /// engagement number of the compiled-code read routing: `0` under
    /// `--nojit`, non-zero once compiled `aaload` / `getfield` run armed.
    #[inline]
    pub fn note_load_barrier_jit_slot(&self) {
        if zgc_load_barrier_census_level() != 0 {
            self.counters.lb_jit_slot.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The uncolored-word arm of [`Self::load_barrier_slot`] (zt t18/zb1).
    /// Reached only while armed.
    #[inline]
    pub fn load_barrier_plain_word(&self, raw: u64, census: bool) -> Option<usize> {
        if raw == 0 {
            return None;
        }
        if census {
            self.counters.lb_plain.fetch_add(1, Ordering::Relaxed);
            // zt t23/zb6 (2f census): a plain word read during a mark on a
            // colored-store heap -- a target the load barrier cannot mark.
            if self.counters.lb_colored_stores.load(Ordering::Relaxed)
                && self.mark_active.load(Ordering::Relaxed)
            {
                self.counters.lb_plain_in_mark.fetch_add(1, Ordering::Relaxed);
            }
        }
        // A3, additive. `satb_pre_barrier` is the store barrier's own
        // publication (registry screen, per-thread buffers, the mark-active
        // re-test), so a load publishes exactly what a store would have.
        if self.counters.lb_mark_on_load.load(Ordering::Relaxed)
            && self.mark_active.load(Ordering::Relaxed)
        {
            self.satb_pre_barrier(raw as usize);
            if census {
                self.counters.lb_marked.fetch_add(1, Ordering::Relaxed);
            }
        }
        Some(raw as usize)
    }

    /// zt t18/zb1: the census's bypass level (`CRATONVM_DBG_ZGC_LOAD_BARRIER=2`)
    /// -- an armed accessor serves this read raw and counts it. The positive
    /// control for `lb_unrouted`; `false` on every other setting.
    #[inline]
    pub fn load_barrier_census_bypass(&self) -> bool {
        // zt t19/zb2: never on a colored-store heap, where the raw word is
        // not the pointer (the level is sound only in plain-store mode).
        if zgc_load_barrier_census_level() != 2
            || self.counters.lb_colored_stores.load(Ordering::Relaxed)
        {
            return false;
        }
        self.counters.lb_unrouted.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// zt t18/zb1 (A2a): an armed read of the 8-byte reference word at
    /// `slot_addr`, as the `Value` the unbarriered accessors return. The
    /// element arm of `get_array_element` builds the same value inline.
    #[inline]
    pub fn load_barrier_ref_value(&self, slot_addr: usize) -> Value {
        match self.load_barrier_slot(slot_addr) {
            None => Value::Object(None),
            // SAFETY: the barrier returned a machine address it resolved from
            // a live slot; `from_raw` is the construction the unbarriered
            // path performs.
            Some(addr) => Value::Object(Some(unsafe { ObjectRef::from_raw(addr as *mut u8) })),
        }
    }

    /// zt t23/st1 (item 29a, `CRATONVM_ZGC_REFERENT_READS_WEAK`): the
    /// [`barrier::ZBarrierKind::Weak`] twin of [`Self::load_barrier_slot`] --
    /// the same answer (forwarded, never a bare offset, `None` for null), but
    /// it NEVER marks: an uncolored word is answered as the pointer it is (no
    /// `lb_mark_on_load` publication), and a colored word is tested against
    /// the weak bad mask and, when bad, takes the slow path with the `Weak`
    /// kind, which forwards and heals but does not call `mark_live` (and,
    /// while marking, does not heal either -- exit 5). For the VM's
    /// reference round, which reads a referent slot to DECIDE its fate.
    /// Unarmed (every default run): the raw word, exactly as the strong read.
    #[inline]
    pub fn load_barrier_slot_weak(&self, slot_addr: usize) -> Option<usize> {
        use barrier::ZBarrierContext;
        // SAFETY: as `load_barrier_slot`: the caller supplies the address of
        // an 8-byte-aligned reference word inside a live object.
        let slot = unsafe { &*(slot_addr as *const std::sync::atomic::AtomicU64) };
        let observed_now = slot.load(Ordering::Relaxed);
        if !self.load_barrier_armed() || !vaddr::is_colored_word(observed_now) {
            return (observed_now != 0).then_some(observed_now as usize);
        }
        let base = self.arena_base as u64;
        match barrier::load_barrier_fast_bad(slot, self.weak_bad_mask(), self.address_mask()) {
            barrier::ZFastPath::Good(offset) => {
                if slot.load(Ordering::Relaxed) == vaddr::Z_NULL {
                    return None;
                }
                Some(base.wrapping_add(offset) as usize)
            }
            barrier::ZFastPath::Bad(observed) => {
                // A tagged word, never null (see `load_barrier_slot`'s arm).
                let offset =
                    barrier::load_barrier_slow(slot, observed, self, barrier::ZBarrierKind::Weak);
                Some(base.wrapping_add(offset) as usize)
            }
        }
    }

    /// zt t23/st1 (item 29a): a NON-MARKING read of reference field `index`
    /// of `obj` -- [`GarbageCollector::get_field`] except that, while the
    /// read barrier is armed, the slot is read through
    /// [`Self::load_barrier_slot_weak`] instead of the strong barrier.
    ///
    /// Why it exists: under the color flip (`CRATONVM_ZGC_COLOR_FLIP`) the
    /// VM's pre-collection reference pass of a concurrent cycle's closing
    /// collection runs BEFORE `finish_concurrent_mark` (so before the remap
    /// flip and while `mark_active` is still up). A referent slot no mutator
    /// loaded during the phase still holds a `Remapped` word, which is bad
    /// under the flip, so the strong read's slow path MARKED the referent it
    /// was about to judge: every such weak referent survived and
    /// `WeakReference.get()` answered non-null after `System.gc()` (x19 item
    /// 29a; HotSpot 25: `null`). A slot the resolver does not prove an 8-byte
    /// reference word (not a reference, a legacy cell with another tag), and
    /// every unarmed read, falls back to the strong accessor, which reads
    /// such a slot without any barrier.
    pub fn get_field_weak(&self, obj: ObjectRef, index: usize) -> Value {
        if self.load_barrier_armed() {
            if let Some(slot) = self.resolve_ref_slot(obj, index) {
                return match self.load_barrier_slot_weak(slot as usize) {
                    None => Value::Object(None),
                    // SAFETY: as `load_barrier_ref_value`.
                    Some(addr) => {
                        Value::Object(Some(unsafe { ObjectRef::from_raw(addr as *mut u8) }))
                    }
                };
            }
        }
        <Self as GarbageCollector>::get_field(self, obj, index)
    }

    /// zt t32: [`Self::get_field_weak`] for element `index` of a reference
    /// array -- a read that never marks, for the VM's pause-side walks (the
    /// stop-the-world probe from the finalizable objects): under the color
    /// flip the closing collection's mark is still up while they run, and a
    /// strong read of a `Remapped` element marked everything a dead
    /// finalizable object reaches, so its weak referents were never cleared.
    pub fn get_array_element_weak(&self, obj: ObjectRef, index: usize) -> Result<Value, i32> {
        if self.load_barrier_armed() {
            if let Some(slot) = self.resolve_ref_slot(obj, index) {
                return Ok(match self.load_barrier_slot_weak(slot as usize) {
                    None => Value::Object(None),
                    // SAFETY: as `load_barrier_ref_value`.
                    Some(addr) => {
                        Value::Object(Some(unsafe { ObjectRef::from_raw(addr as *mut u8) }))
                    }
                });
            }
        }
        <Self as GarbageCollector>::get_array_element(self, obj, index)
    }

    /// zt t18/zb1: `[GC] zgc-load-barrier:` -- the census line
    /// (`CRATONVM_DBG_ZGC_LOAD_BARRIER`), printed at shutdown. Keys are
    /// `lb_`-prefixed so they cannot collide with another summary line.
    pub fn load_barrier_census_line(&self) -> String {
        let c = &self.counters;
        format!(
            "[GC] zgc-load-barrier: lb_armed={} lb_plain_stores={} lb_mark_on_load={} \
             lb_routed={} lb_plain={} lb_colored={} lb_marked={} lb_unrouted={} \
             lb_colored_stores={} lb_colored_refused={} lb_store_colored={} lb_jit_slot={} \
             lb_gc_colored={} lb_decode_base={} lb_color_flip={} lb_flips_mark={} \
             lb_flips_remap={} lb_plain_in_mark={} lb_referent_keep_alive={}",
            u8::from(self.load_barrier_armed()),
            u8::from(c.lb_plain_stores.load(Ordering::Relaxed)),
            u8::from(c.lb_mark_on_load.load(Ordering::Relaxed)),
            c.lb_routed.load(Ordering::Relaxed),
            c.lb_plain.load(Ordering::Relaxed),
            c.lb_colored.load(Ordering::Relaxed),
            c.lb_marked.load(Ordering::Relaxed),
            c.lb_unrouted.load(Ordering::Relaxed),
            u8::from(c.lb_colored_stores.load(Ordering::Relaxed)),
            u8::from(c.lb_colored_refused.load(Ordering::Relaxed)),
            c.lb_store_colored.load(Ordering::Relaxed),
            c.lb_jit_slot.load(Ordering::Relaxed),
            c.lb_gc_colored.load(Ordering::Relaxed),
            // zt t20/zb3: does THIS heap own the colored-slot decode base?
            u8::from(
                cratonvm_types::narrow_oop::colored_slot_base() == self.arena_base as u64
                    && self.arena_base != 0
            ),
            // zt t22/zb5: the color flip -- armed, and the flips taken.
            u8::from(c.lb_color_flip.load(Ordering::Relaxed)),
            c.lb_flips_mark.load(Ordering::Relaxed),
            c.lb_flips_remap.load(Ordering::Relaxed),
            // zt t23/zb6: plain words read during a mark (2f census).
            c.lb_plain_in_mark.load(Ordering::Relaxed),
            // zt t23/zb7: referent keep-alives on the load barrier's mark.
            c.lb_referent_keep_alive.load(Ordering::Relaxed),
        )
    }

    /// Arm or disarm the read-path barrier by flipping the good mask.
    ///
    /// `Some(color)` arms it for that mark parity; `None` returns to
    /// `Z_REMAPPED`, the quiescent "addresses are plain" state.
    pub fn set_barrier_color(&self, color: Option<vaddr::ZColor>) {
        let mask = match color {
            Some(vaddr::ZColor::Marked0) => vaddr::Z_MARKED0,
            Some(vaddr::ZColor::Marked1) => vaddr::Z_MARKED1,
            // `Finalizable` is not a phase: it marks a pointer reached ONLY
            // through a finalizable object, and is never the cycle's good
            // color. Treating it as one would make every ordinary reference
            // bad for a whole cycle.
            Some(vaddr::ZColor::Finalizable) | Some(vaddr::ZColor::Remapped) | None => {
                vaddr::Z_REMAPPED
            }
        };
        self.barrier_good_mask.store(mask, Ordering::Release);
        self.barrier_heal_mask.store(mask, Ordering::Release);
        self.barrier_armed.store(color.is_some(), Ordering::Release);
        if color.is_none() {
            // zt t18/zb1: disarming ends a plain-store arming too; a later
            // `Some` from a test arms the colored model it always meant.
            self.counters
                .lb_plain_stores
                .store(false, Ordering::Release);
            self.counters
                .lb_mark_on_load
                .store(false, Ordering::Release);
            // zt t19/zb2: and a colored-store arming.
            let was_colored = self
                .counters
                .lb_colored_stores
                .swap(false, Ordering::AcqRel);
            // zt t20/zb3: and gives its decode base back (owner-checked), so
            // the next colored-store heap can publish its own. Words this heap
            // colored are then read verbatim again -- the unarmed answer.
            if was_colored {
                cratonvm_types::narrow_oop::retract_colored_slot_base(self.arena_base as u64);
            }
            // zt t22/zb5: and a color-flip arming -- the cell back to the
            // quiescent bad mask, the rotation back to `Remapped`, and the
            // cell's address withdrawn from the JIT (owner-checked).
            if self.counters.lb_color_flip.swap(false, Ordering::AcqRel) {
                let _ = self
                    .counters
                    .lb_flip_mask
                    .flip_to_remap_publishing(&self.counters.lb_bad_cell);
                cratonvm_types::narrow_oop::retract_colored_bad_mask_cell(
                    self.counters.lb_bad_cell.word_address(),
                );
            }
        }
        // Couple the JIT's READ-side bounds table to the barrier state, in
        // this order: the table must be EMPTY before any thread can observe
        // the armed flag, because a cleared table is what stops ALREADY
        // COMPILED inline sequences -- which the emission gate
        // (`narrow_oops_block_inline_fields`) cannot reach. See
        // [`zgc_jit_read_bounds_enabled`] for the full argument.
        //
        // OBLIGATION for the first non-test caller: arming must not race a
        // live inline sequence. A thread can pass the containment compare and
        // be preempted before its raw load, so the transition has to happen
        // where no Java thread is running compiled code -- i.e. at a
        // safepoint, which is where a real mark-start pause arms anyway.
        if zgc_jit_read_bounds_enabled() {
            if color.is_some() {
                cratonvm_gc_core::jit_bounds::clear_jit_read_bounds();
            } else {
                cratonvm_gc_core::jit_bounds::publish_jit_read_bounds(0, self.arena_base, self.arena_end);
            }
        }
        // ...and the process-wide codegen gate. The JIT decides whether to
        // emit a raw inline reference load while holding no heap handle, so
        // this one fact has to be reachable without one -- see
        // `crate::zgc_read_barrier_armed` for why that exception is safe.
        cratonvm_types::set_zgc_read_barrier_armed(color.is_some());
    }

    /// Is the default-off stop-the-world compaction sub-flag set?
    ///
    /// `CRATONVM_ZGC_RELOCATE=1`. This is the sub-flag the production plan's
    /// R5 row insists on, and which that row's own wording made stale: it said
    /// "default-off within the already-default-off `zgc` feature", and `zgc`
    /// has been default-ON since 2026-08-10. This switch is no longer
    /// belt-and-braces; it IS the belt.
    ///
    /// Deliberately NOT `zgc_relocation_permitted`. That gate answers a
    /// *safety* question (is the JIT off?) and lives in `vm_init`; this one
    /// answers an *intent* question and lives here. A caller needs both.
    pub fn relocation_requested(&self) -> bool {
        // PER HEAP, seeded from the variable below at construction. Two reasons
        // it is not read straight from the environment here: a host may own more
        // than one heap, and a test that needs addresses to hold still for the
        // length of an assertion has no other way to ask for that -- the
        // variable is process-wide and a test that set it would decide the
        // question for every other test in the binary.
        //
        // AND the GPU veto: a device DMA against the heap arena that the
        // collector's bounded wait could not outlast forbids this cycle's
        // slide. `false` in every build without `gpu-offload`. See
        // `vm_heap::gpu_relocation_forbidden`.
        self.relocation_enabled.load(Ordering::Relaxed)
            && !self.external_gpu.gpu_relocation_forbidden()
    }

    /// Turn the stop-the-world slide on or off for THIS heap.
    ///
    /// The process-wide default is `CRATONVM_ZGC_RELOCATE` (on unless set to
    /// `0`/`off`/`false`/`no`).
    pub fn set_relocation_enabled(&self, on: bool) {
        self.relocation_enabled.store(on, Ordering::Relaxed);
    }

    /// Turn the LARGE-OBJECT end's compactor on or off for THIS heap.
    ///
    /// The process-wide default is `CRATONVM_ZGC_HIGH_COMPACTION` (on unless
    /// set to `0`/`off`/`false`/`no`). Per heap for the reason
    /// [`Self::set_relocation_enabled`] is: the A/B this switch exists to be
    /// has to be runnable inside one test binary.
    pub fn set_high_compaction_enabled(&self, on: bool) {
        self.high_compaction_enabled.store(on, Ordering::Relaxed);
    }

    /// Turn the publication of the slide's vacated spans on or off for THIS
    /// heap. Process-wide default: `CRATONVM_ZGC_PUBLISH_VACATED` (on unless
    /// `0`/`off`/`false`/`no`).
    pub fn set_publish_vacated_enabled(&self, on: bool) {
        self.publish_vacated_enabled.store(on, Ordering::Relaxed);
    }

    /// Test and diagnostic override of `CRATONVM_ZGC_READABLE_GIVE_BACK` for
    /// this heap (see [`zgc_readable_give_back_enabled`]). Turning it ON on a
    /// heap that did not publish the JIT read bound is harmless -- it only
    /// changes how pages go back -- but the switch buys nothing there.
    pub fn set_readable_give_back(&self, on: bool) {
        self.arena.lock().set_readable_give_back(on);
    }

    /// The process-wide default for [`Self::set_relocation_enabled`].
    pub fn relocation_requested_by_default() -> bool {
        // DEFAULT-ON since 2026-08-13, for the gauntlet.
        //
        // `CRATONVM_ZGC_RELOCATE=0` (or `off`/`false`/`no`) is the kill switch
        // and restores the non-moving behaviour byte for byte -- the sweep
        // simply returns an empty `PointerMap` as it always did, so the A/B is
        // a re-run and not a rebuild.
        //
        // **What to watch on the first gauntlet.** This is the first
        // configuration in which a ZGC cycle returns a NON-EMPTY pointer map,
        // so every consumer of one now runs for this collector: JIT frame
        // maps, monitor tables, external root providers and native side
        // tables. Those consumers are collector-agnostic and already run for
        // the generational moving-young path, and the two `VmHeap::Zgc`
        // predicates that take a pre-GC address were audited and tested (R6) --
        // but "already runs for another collector" is not "has run for this
        // one". A crash or a stale-reference warning that appears only with
        // this flag on is this change, and `=0` is the bisect.
        match None::<std::ffi::OsString> {
            Some(raw) => {
                let v = raw.to_string_lossy().trim().to_ascii_lowercase();
                !matches!(v.as_str(), "0" | "off" | "false" | "no")
            }
            None => true,
        }
    }
}
/// May a ZGC cycle compact while a compiled frame is live, on the strength of
/// the collection's per-cycle coverage proof?
///
/// Default ON. `CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT=0` (or `off`/`false`/
/// `no`) restores the older refusal -- compact only when no compiled frame is
/// live at all -- which is the bisect for anything that appears with this on.
/// A kill switch and not an opt-in: default-off here would leave the default
/// configuration (this collector, JIT on) with no defragmentation, which is the
/// defect rather than a conservative posture.
///
/// Read per CYCLE rather than latched in a `OnceLock`. It is consulted once per
/// collection, so the cost is nothing, and a latch would make the switch
/// untestable -- the first test to touch it would decide the answer for every
/// later test in the binary.
/// `CRATONVM_ZGC_PAGE_PINNED_RELOCATE=0` -- restore the per-CYCLE relocation
/// refusal. Default ON.
///
/// The kill switch for [`ZgcRealHeap::coverage_incompleteness_is_page_pinnable`].
/// With it off, any incomplete coverage proof refuses the whole cycle again,
/// which is what the collector did before -- so a bisect is a re-run rather
/// than a rebuild, and anything that appears with this on is this change.
///
/// Read per cycle rather than latched, for `zgc_relocate_under_proven_jit`'s
/// reason: it is consulted once per collection, so the cost is nothing, and a
/// latch would let the first test to touch it decide the answer for every later
/// test in the binary.
pub fn zgc_page_pinned_relocate() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

impl ZgcRealHeap {
    /// Incompleteness reasons that page pinning **cannot** cover, as a mask
    /// over [`cratonvm_gc_core::gc_quiescence::incomplete_reason`].
    ///
    /// Everything not in here is a reason of the form "this collector cannot
    /// prove where the oops in that frame are" -- which is precisely the regime
    /// pin-by-value was designed for, and precisely what
    /// `pinned_jit_roots_snapshot` already withholds a page for.
    pub const UNPINNABLE_COVERAGE_REASONS: usize = {
        use cratonvm_gc_core::gc_quiescence::incomplete_reason as r;
        // A peer OS-SUSPENDED in JIT code, and a blocked peer's helper window.
        // `unrewritable_peer_state`'s own doc scopes the 2026-07-03 promotion
        // gate to exactly these two and says why: such a peer is excused from
        // the safepoint barrier, so it never re-reads its own registers -- and
        // a register holding only a DERIVED/interior pointer names no base, so
        // pin-by-value does not protect the base. The resumed peer then keeps
        // loading through a stale derived pointer.
        //
        // LIFTING `XT_HELPER_WINDOW` OUT OF THIS SET WAS TRIED, MEASURED, AND
        // REVERTED (2026-09-02). The argument was the D5 one, one level down:
        // the derived-pointer hazard above is a statement about the PIN SET,
        // not about the cycle -- `is_obj` accepts only bases, so an interior
        // pointer is never pinned and its base moves. So `helper_window_pass`
        // was made to publish every in-heap WORD of the frozen band, base or
        // not, on the reasoning that an interior pointer lands on the same page
        // as the object it points into and the page would then be withheld.
        //
        // It corrupts the heap. `probes/OopMapHelperWindow.java` drives the
        // path for real (`helper-window pass: 1-2 window(s), 186-384
        // conservative root(s)` on 5-8 collections of a run, where
        // `OopMapPeerCoverage` reports `0 window(s)` and exercises nothing) and
        // the arms are unambiguous:
        //
        //     lift enabled                 2 of 3 runs WRONG, NullPointerException
        //                                  reading an array element written one
        //                                  statement earlier
        //     PAGE_PINNED_RELOCATE=0       3 of 3 correct
        //     ZGC_RELOCATE=0               3 of 3 correct
        //
        // So page pins are NOT sufficient for a frozen peer, and the reason is
        // not yet established -- the leading suspect is that
        // `memory::roots::collect_roots` calls `clear_pinned_jit_roots()` and
        // the JIT-frame scan then REPUBLISHES the collector thread's entry
        // wholesale, discarding pins the helper-window pass added earlier on
        // that same thread. Whoever picks this up should establish that first;
        // if it holds, the pins need an entry of their own rather than a share
        // of the collector's.
        //
        // `XT_TAKEOVER` was never lifted, and an unmeasured extension of the
        // argument to a second path is how a sound repair becomes an unsound
        // one -- which is what the row above records happening to the first.
        (1 << r::XT_TAKEOVER)
            | (1 << r::XT_HELPER_WINDOW)
            // A blanket statement that the JIT's relocation contract is not
            // strong enough. It is not a claim about one frame, so no per-page
            // decision can answer it.
            | (1 << r::JIT_RELOCATION_UNSUPPORTED)
            // A JIT frame with no `JitEntryGuard` is a frame the scan does not
            // know is there. Already its own term in the refusal chain; kept
            // here so the mask alone is a complete statement.
            | (1 << r::UNREGISTERED_JIT_FRAME)
            // The runtime oracle caught a frame whose `fully_oop_covered` bit
            // was false. Conservative scanning would in fact find such an
            // address, but a proof that has been directly refuted is not
            // evidence for anything, so this stays a refusal.
            | (1 << r::COVERAGE_ORACLE_REFUTED)
    };

    /// May this cycle relocate the pages that are NOT pinned, despite an
    /// incomplete coverage proof?
    ///
    /// # The refusal this replaces, and what it cost
    ///
    /// `relocate_stw`'s gate refused the WHOLE CYCLE whenever
    /// `moving_young_coverage_incomplete()` was true, and the dominant reason
    /// is [`CROSS_THREAD_JIT_PEER`](cratonvm_gc_core::gc_quiescence::incomplete_reason::CROSS_THREAD_JIT_PEER)
    /// -- "some peer is somewhere inside compiled code", which is true of
    /// nearly every cycle in a warmed-up server workload. `relocate_stw`'s own
    /// comment says so: "it marks CROSS_THREAD_JIT_PEER incomplete whenever any
    /// thread OTHER than the collection initiator is in compiled code, so a
    /// many-threaded workload still compacts rarely."
    ///
    /// On this collector compaction is also the only defragmentation there is,
    /// so that refusal is why the default configuration accumulated
    /// `CRATONVM_ZGC_TARGETED_COMPACTION`, `CRATONVM_ZGC_HIGH_COMPACTION`,
    /// `CRATONVM_ZGC_PUBLISH_VACATED` and `CRATONVM_ZGC_TLAB_STARVED_RECYCLE` --
    /// four repairs for fragmentation that compaction was supposed to handle.
    /// The first of them is documented as engaging ZERO times on every workload
    /// measured.
    ///
    /// # Why a per-page answer is available, and was already built
    ///
    /// `relocate_stw` **already withholds** every logical page that a
    /// conservatively-discovered JIT root lands on -- that is the
    /// `pinned_jit_roots_snapshot()` consumer added when compaction shipped,
    /// after G1 learned the same lesson on 2026-08-11. Pinning by raw ADDRESS
    /// VALUE is exactly what protects a frame whose oops the collector cannot
    /// locate: it does not matter whether the address is a base, an interior
    /// pointer or a `long` that happens to look like one, because the page it
    /// falls in is not evacuated either way.
    ///
    /// So the machinery for a graceful answer was in place and the gate in
    /// front of it was binary. This makes the gate ask the question the pin set
    /// can actually answer.
    ///
    /// # The three things it still refuses
    ///
    /// 1. **State pin-by-value cannot protect.**
    ///    [`Self::UNPINNABLE_COVERAGE_REASONS`] carries the argument per code;
    ///    the load-bearing pair is the OS-suspended peer, whose registers may
    ///    hold a derived pointer that names no base. `unrewritable_peer_state()`
    ///    is checked as well as the mask, because it is set through a path of
    ///    its own and the two are not redundant.
    ///
    /// 2. **An incompleteness with no reason recorded.**
    ///    `mark_moving_young_coverage_incomplete` sets the flag and records
    ///    nothing, so the mask is zero and there is no obligation to reason
    ///    about. Everything below is an argument about WHICH obligation failed;
    ///    with none named, the fail-closed answer is the old refusal.
    ///
    /// 3. **A pin set that is empty because nobody looked.** An empty
    ///    `pinned_jit_roots_snapshot` means either "no conservative root
    ///    exists" or "no conservative scan ran", and the set cannot tell them
    ///    apart -- so a consumer that treats empty as a licence would relocate
    ///    everything while believing it was protected. That is the
    ///    zero-from-an-instrument-armed-where-it-cannot-fire shape, and
    ///    `gc_quiescence::conservative_jit_scans()` is the discriminator: with
    ///    compiled frames live and no scan published, this refuses.
    pub fn coverage_incompleteness_is_page_pinnable(&self) -> bool {
        if !zgc_page_pinned_relocate() {
            return false;
        }
        if cratonvm_gc_core::gc_quiescence::unrewritable_peer_state() {
            return false;
        }
        // The one take-over licence (gc-common w3-g/w4-g,
        // `handoff-w3g-zgc-takeover-licence`): a pause whose take-over left an
        // unread peer, or a pin set that is incomplete or exact-base only, may
        // not relocate on page pins. Stricter than or equal to the mask below
        // (every take-over that marks `XT_TAKEOVER` / `XT_HELPER_WINDOW` is
        // already refused there); it is here so all three backends answer "is
        // a frozen peer safe?" from one predicate. A consumer, not a
        // relaxation: the mask is NOT replaced by it (page pins were measured
        // insufficient for a frozen peer -- see `UNPINNABLE_COVERAGE_REASONS`).
        if cratonvm_gc_core::gc_quiescence::may_move_under_takeover(true)
            == cratonvm_gc_core::gc_quiescence::MoveLicence::NonMoving
        {
            return false;
        }
        let mask = cratonvm_gc_core::gc_quiescence::moving_young_incomplete_reason_mask();
        // AN UNREASONED INCOMPLETENESS IS UNPINNABLE. `mark_moving_young_
        // coverage_incomplete` sets the flag without recording anything, so a
        // zero mask means "the proof failed and nobody said why" -- and the
        // whole argument below is an argument about WHICH obligation failed.
        // With no obligation named there is nothing to reason from, and the
        // fail-closed answer is the old refusal.
        if mask == 0 || mask & Self::UNPINNABLE_COVERAGE_REASONS != 0 {
            return false;
        }
        // THE VACUOUS-ZERO SCREEN. See point 2 above.
        if cratonvm_gc_core::gc_quiescence::is_active() && cratonvm_gc_core::gc_quiescence::conservative_jit_scans() == 0
        {
            return false;
        }
        true
    }

    /// Cycles that relocated the unpinned pages despite an incomplete coverage
    /// proof, because page pinning covered the shortfall.
    ///
    /// The engagement counter for [`Self::coverage_incompleteness_is_page_pinnable`].
    /// Zero on a run whose `relocation_skip_reasons[COVERAGE_INCOMPLETE]` is
    /// high means the new path never opened and the finding is unfixed; high
    /// with `skipped_jit` low means it is carrying the workload.
    pub fn relocation_on_page_pins(&self) -> usize {
        self.counters
            .relocation_on_page_pins
            .load(Ordering::Relaxed)
    }

    /// `(slides_verified, survivors_walked, missed_rewrites, unregistered_targets)`
    /// for [`Self::verify_no_dangling_slots_after_slide`].
    ///
    /// The first two are the DENOMINATOR. `missed_rewrites=0` with
    /// `slides_verified=0` means the verifier never ran — which is what a
    /// release build under `CRATONVM_DBG_ZGC_VERIFY_SLIDE=1` looked like from
    /// the outside before this existed, since the clean-case log line is a
    /// `debug!` and `release_max_level_info` deletes it.
    pub fn slide_verification_stats(&self) -> (usize, usize, usize, usize) {
        (
            self.counters.slide_verifications.load(Ordering::Relaxed),
            self.counters
                .slide_verified_survivors
                .load(Ordering::Relaxed),
            self.counters
                .slide_verify_missed_rewrites
                .load(Ordering::Relaxed),
            self.counters
                .slide_verify_unregistered
                .load(Ordering::Relaxed),
        )
    }
}

pub fn zgc_relocate_under_proven_jit() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

