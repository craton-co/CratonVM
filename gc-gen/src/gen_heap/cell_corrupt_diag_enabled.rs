#[allow(unused_imports)]
use super::*;

impl GenerationalHeap {
    /// CORRECTION (gengc-round4/cards, 2026-09-23): this paragraph used to
    /// open with "The remembered-set fast path is backed by a complete
    /// old-to-young scan" — the pre-2026-09-02 default, which the section
    /// below records being turned OFF. It is not backed by one unless
    /// `CRATONVM_GC_FULL_RSET_SCAN=1`. The history still stands: a missed
    /// write barrier has manifested as silently emptied Stream/ArrayList
    /// results under small-heap concurrency, which is why
    /// `CRATONVM_GC_VERIFY_RSET` exists.
    #[inline]
    /// Should a young collection ALSO walk the whole old generation for
    /// old->young edges, after the dirty-card scan has already answered?
    ///
    /// # Default OFF since 2026-09-02 (gc-genpause F2)
    ///
    /// This used to be `!card_table_only`, i.e. ON unless someone opted out,
    /// and what it turned on was `scan_all_old_to_young`: a full
    /// `OldGen::walk_objects()` -- which materialises a `Vec` of every tenured
    /// object -- scanning every reference slot of every one of them, on every
    /// young collection, duplicating work `scan_dirty_cards` had just done.
    /// Young pause time therefore grew permanently with OLD-generation size,
    /// inside a pause that is supposed to grow with the YOUNG live set.
    ///
    /// Measured (`OldGenRsetProbe 19 700 16`, `-Xmx1g`, `CRATONVM_DBG=gcpause`):
    /// `full_old_rset_scan` was 41-61 ms of a ~229 ms steady-state pause, on a
    /// heap that genuinely held ZERO old->young edges -- the card scan
    /// returned `dirty_scanned=0 old_to_young_edges=0` and the full walk found
    /// nothing either. Removing it, one binary, one env var, A,B,A,B,A,B
    /// interleaved, identical program checksums and identical `minor=14` in
    /// both arms: 15714 -> 13940 ms minimum, 16138 -> 15080 ms median, and all
    /// three paired rounds favoured the card-table-only arm.
    ///
    /// # What replaces the insurance
    ///
    /// It was never a bug -- it was a standing premium against a missed write
    /// barrier, and a missed barrier really would free a reachable young
    /// object. What replaces it is `CRATONVM_GC_VERIFY_RSET`
    /// ([`Self::verify_old_to_young_rset`]): the same whole-old-gen walk, run
    /// as a CHECK that prints `edges=N missing=M`, so the premium is paid
    /// while auditing rather than by every collection forever. That is exactly
    /// the shape G1 already uses for its own remembered set
    /// (`CRATONVM_G1_DBG_RSET`).
    ///
    /// `CRATONVM_GC_FULL_RSET_SCAN=1` is the revert lever.
    /// `CRATONVM_CARD_TABLE_ONLY=1` keeps its documented meaning and still
    /// forces the scan off, so a script that sets it is unaffected -- it now
    /// asks for what it already gets.
    pub fn full_old_rset_scan_enabled() -> bool {
        gc_flags().full_rset_scan && !gc_flags().card_table_only
    }

    /// `CRATONVM_GC_VERIFY_RSET=1` -- did the card table deliver every
    /// old->young edge this collection needed?
    ///
    /// Walks the whole old generation and compares the `(referrer, slot)`
    /// pairs it finds pointing into from-space against the ones the seeding
    /// actually produced. Prints
    /// `[rset-verify] site=.. edges=N missing=M seeded=S`, and names the first
    /// missing edge's referrer, class and slot.
    ///
    /// # Why it prints `edges` and not just `missing`
    ///
    /// A checker whose output cannot distinguish "nothing was missing" from
    /// "there was nothing to miss" is worthless -- a green `missing=0` on a
    /// run whose old generation held no old->young edges at all proves
    /// nothing, and that is the most common shape. `edges` is how many the
    /// walk found; a soak that only ever reports `edges=0` has not tested the
    /// card table, whatever `missing` says.
    ///
    /// # It is vacuous with `CRATONVM_GC_FULL_RSET_SCAN=1`
    ///
    /// It runs AFTER the seeding, and the full scan seeds every old->young
    /// edge in the heap by definition -- so with both flags on, `missing` is
    /// structurally zero and proves nothing about the card table. Run this
    /// flag on the DEFAULT configuration, which is the one being checked.
    ///
    /// Costs a full old-gen walk per young collection -- precisely the cost
    /// [`Self::full_old_rset_scan_enabled`] used to pay unconditionally.
    ///
    /// Returns `(edges, missing)` so a test can prove the checker is capable
    /// of reporting a miss -- a verifier nobody has ever seen fail is a
    /// verifier nobody knows works.
    pub fn verify_old_to_young_rset(
        old_gen: &OldGen,
        young_from: &Arena,
        seeded: &[(ObjectRef, usize, usize)],
        site: &str,
    ) -> (usize, usize) {
        let mut have: FxHashSet<(usize, usize)> = FxHashSet::default();
        for &(obj, slot, _) in seeded {
            have.insert((obj.as_ptr() as usize, slot));
        }
        let mut edges = 0usize;
        let mut missing = 0usize;
        let mut first: Option<(usize, u32, usize, usize)> = None;
        // gengc-round4/cards: the two barriers decide "is the holder old?" by
        // DIFFERENT predicates. `write_barrier` tests the holder's ADDRESS
        // against the old-gen arena; compiled reference stores skip the
        // barrier call unless the holder's header carries `GC_FLAG_OLD_GEN`
        // (this heap's `jit_ref_store_gates.publish_masked(GC_FLAG_OLD_GEN)`,
        // and the inline card barrier's young-receiver test). An old-arena
        // object WITHOUT the flag is therefore a holder whose compiled stores
        // never dirty a card — a missed barrier this checker would otherwise
        // only see as a `missing` edge, with no hint of why. Count them on the
        // walk this checker already pays for.
        let mut unflagged = 0usize;
        let mut first_unflagged: Option<(usize, u32)> = None;
        for (obj_ptr, _size) in old_gen.walk_objects() {
            // SAFETY: `walk_objects` yields valid old-gen object starts.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            if header.gc_flags() & GC_FLAG_OLD_GEN == 0 {
                unflagged += 1;
                if first_unflagged.is_none() {
                    first_unflagged = Some((obj_ptr as usize, header.class_id.as_u32()));
                }
            }
            // SAFETY: the object/header pair is valid for this STW walk.
            unsafe {
                for_each_ref_slot(obj_ptr, header, |raw, slot_idx| {
                    if raw.is_null() || !young_from.contains(raw) {
                        return;
                    }
                    edges += 1;
                    // `for_each_ref_slot` and `scan_dirty_cards` use the SAME
                    // slot encoding per layout (element index for reference
                    // arrays, body byte offset for compact objects, slot index
                    // for legacy cells), so this comparison is exact rather
                    // than approximate.
                    if !have.contains(&(obj_ptr as usize, slot_idx)) {
                        missing += 1;
                        if first.is_none() {
                            first = Some((
                                obj_ptr as usize,
                                header.class_id.as_u32(),
                                slot_idx,
                                raw as usize,
                            ));
                        }
                    }
                });
            }
        }
        eprintln!(
            "[rset-verify] site={site} edges={edges} missing={missing} seeded={}",
            seeded.len(),
        );
        if let Some((referrer, class_id, slot, referent)) = first {
            eprintln!(
                "[rset-verify]   first MISSING old->young edge: referrer=0x{referrer:x} \
                 class_id={class_id} slot={slot} -> young 0x{referent:x}. The card table did \
                 not deliver it, so this cycle would reclaim a reachable young object. Re-run \
                 with CRATONVM_GC_FULL_RSET_SCAN=1 to confirm the workload is clean under the \
                 old belt-and-braces seeding.",
            );
        }
        if let Some((obj, class_id)) = first_unflagged {
            eprintln!(
                "[rset-verify]   old_gen_objects_without_GC_FLAG_OLD_GEN={unflagged}; first at \
                 0x{obj:x} class_id={class_id}. Compiled reference stores into such a holder \
                 skip the card barrier (the JIT gate tests the flag, not the address).",
            );
        }
        (edges, missing)
    }

    /// Append all old-to-young slots using the same slot encoding as the card
    /// scanner. Duplicates from the card fast path are harmless: forwarding is
    /// idempotent.
    pub fn scan_all_old_to_young(
        old_gen: &OldGen,
        young_from: &Arena,
        extra_roots: &mut Vec<(ObjectRef, usize, usize)>,
    ) {
        for (obj_ptr, _total_size) in old_gen.walk_objects() {
            // SAFETY: the old-generation walk yields initialized object starts.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            // SAFETY: the object/header pair remains valid during this STW scan.
            unsafe {
                for_each_ref_slot(obj_ptr, header, |raw, slot_idx| {
                    if !raw.is_null() && young_from.contains(raw) {
                        let obj_ref = ObjectRef::from_raw(obj_ptr);
                        extra_roots.push((obj_ref, slot_idx, 0));
                    }
                });
            }
        }
    }

    /// Scan dirty cards in the card table for old→young references.
    ///
    /// For each dirty card, walks the objects in that card region and collects
    /// slots containing references into young from-space.
    ///
    /// This is the **card refinement pass**, and this wrapper is where it is
    /// priced (`cratonvm_gc_core::gc_metrics`): wall time, the remembered-set metadata the
    /// table retains, and the old→young edges the walk actually found. All
    /// three are unconditional — the pass runs once per collection, so the
    /// accounting is unmeasurable against the pause it is measuring, and gating
    /// it would make the default report empty (which is the failure this item
    /// exists to prevent). The per-store barrier counter is the one that IS
    /// gated; see [`cratonvm_gc_core::gc_metrics`]'s module header.
    ///
    /// Edges are counted as the growth of `extra_roots` across the call rather
    /// than tallied inside the walk, so no counting code sits inside the
    /// per-slot loops.
    ///
    /// gen r5w1/oldgen5: returns the young words found in old-gen WALK GAPS
    /// (bytes behind a header no walk can size, which a dirty range reached);
    /// see [`CardScanGapRoots`]. Empty on a sane heap. A caller that ignores
    /// it behaves exactly as before this change.
    pub fn scan_dirty_cards(
        card_table: &CardTable,
        old_gen: &OldGen,
        young_from: &Arena,
        extra_roots: &mut Vec<(ObjectRef, usize, usize)>,
    ) -> CardScanGapRoots {
        Self::scan_dirty_cards_with(card_table, old_gen, young_from, extra_roots, None, None)
    }

    /// [`Self::scan_dirty_cards`], with the walk split across `par`'s
    /// `(pool, workers)` when given (gcj j1yg-pause, see `par_card_scan`),
    /// and with `values` receiving each new edge's young value at the edge's
    /// index (what `card_pair_value_and_card` reads back), so the lazy proof
    /// need not re-read every slot.
    pub fn scan_dirty_cards_with(
        card_table: &CardTable,
        old_gen: &OldGen,
        young_from: &Arena,
        extra_roots: &mut Vec<(ObjectRef, usize, usize)>,
        par: Option<(&cratonvm_gc_core::evac_pool::EvacPool, usize, &mut Vec<CardScanPart>)>,
        values: Option<&mut Vec<usize>>,
    ) -> CardScanGapRoots {
        let started = std::time::Instant::now();
        let edges_before = extra_roots.len();
        // t1 lane D (`CRATONVM_GC_WHOLE_HEAP_CARDS`): compiled code stores card
        // bytes of a hull table without raising the self-maintained bound, so
        // publish the old generation's high-water mark first: every compiled
        // mark that names a real card lies inside an old object, below it.
        // The bound is monotone, so a mark lowered by compaction never shrinks
        // what this scan (and the `clear_all` after it) covers.
        if card_table.whole_heap() {
            card_table.set_scan_bound_bytes(old_gen.high_water());
        }
        cratonvm_gc_core::gc_metrics::record_remembered_set_bytes(card_table.retained_bytes() as u64);
        // gengc-round4/cards: the card scan is bounded by a self-maintained
        // bound (`CardTable::scan_bound_cards`). Under the rset audit flag,
        // say how far it reaches and whether it is armed at all, so a
        // `[rset-verify] ... missing=0` line can be read against it: a missing
        // edge with `raw_escaped=false` and a referrer card at or above
        // `bound` is the signature of a writer that bypassed the table.
        // Printed BEFORE the take, which is what decides what the scan sees.
        if gc_flags().verify_rset {
            // gen r4w3/cards3: plus the summary map's skips, whether marks are
            // element-precise, and the old gen's block-offset table — the
            // three things this lane changed about what a pause reads.
            let bot = old_gen.block_offset_stats();
            eprintln!(
                "[rset-verify] cards: bound={} of {} skipped_total={} raw_escaped={} \
                 summary={} summary_skipped_total={} precise_arrays={} bot={} \
                 bot_valid_cards={} bot_walks={} bot_fallbacks={} bot_anchored={} \
                 bot_rederived={} bot_anomalies={} bot_gap_windows={}",
                card_table.scan_bound_cards(),
                card_table.num_cards(),
                card_table.cards_skipped_by_scan_bound(),
                card_table.raw_card_address_escaped(),
                card_table.options().summary,
                card_table.cards_skipped_by_summary(),
                card_table.precise_ref_array_marks(),
                bot.enabled,
                bot.valid_cards,
                bot.walks,
                bot.fallbacks,
                bot.anchored_ranges,
                bot.rederived_objects,
                bot.anomalies,
                bot.gap_windows,
            );
        }
        let gap_roots =
            Self::scan_dirty_cards_inner(card_table, old_gen, young_from, extra_roots, par, values);
        cratonvm_gc_core::gc_metrics::record_old_to_young_edges(
            extra_roots.len().saturating_sub(edges_before) as u64,
        );
        // `as_nanos()` is a u128; a single refinement pass cannot plausibly run
        // for 584 years, but saturate rather than wrap if it somehow does.
        let nanos = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        cratonvm_gc_core::gc_metrics::record_refinement(nanos);
        if !gap_roots.young_words.is_empty() {
            note_card_walk_gap_young_words(&gap_roots);
        }
        gap_roots
    }

    /// The refinement walk itself. Split out from [`Self::scan_dirty_cards`] so
    /// the accounting wrapper closes over every early return without holding a
    /// borrow of `extra_roots` across the walk.
    pub fn scan_dirty_cards_inner(
        card_table: &CardTable,
        old_gen: &OldGen,
        young_from: &Arena,
        extra_roots: &mut Vec<(ObjectRef, usize, usize)>,
        par: Option<(&cratonvm_gc_core::evac_pool::EvacPool, usize, &mut Vec<CardScanPart>)>,
        mut values: Option<&mut Vec<usize>>,
    ) -> CardScanGapRoots {
        // CONSUME the dirty cards (`take_dirty_cards`) rather than only read
        // them (`dirty_card_indices()`): the take resets every consumed card's
        // bitmap byte to CARD_CLEAN. This matters for the NON-MOVING sweep,
        // which (unlike the moving path) never calls `clear_all()`: a byte left
        // dirty would be returned, and its card rescanned, by every later
        // collection, and the sweep's re-dirty of the surviving old→young
        // edges is what keeps exactly those cards for the next one (see
        // `CardTable::take_dirty_cards` docs — the bt18 premature-reclamation
        // fix).
        //
        // gcd d3/m (2026-09-27): since gcd d1/d there is no tracking list;
        // `take_dirty_cards` answers from the byte scan alone. This comment
        // said "drain the tracking list" and "clears BOTH the tracking list
        // and the bitmap bytes" until then.
        //
        // CORRECTION (gengc-round3, 2026-09-21): this comment used to claim the
        // call "is O(dirty) instead of O(total cards)". **It is not, and never
        // was.** `CardTable::take_dirty_cards` sweeps
        // `cells.cards[..scan_bound_cards()]` in `SCAN_GROUP`-sized chunks, so
        // its cost is linear in the cards covering the old-gen high-water mark
        // — every collection, whether one card is dirty or none is.
        // `scan_bound_cards()` is that high-water mark (raised by
        // `cover_card_index` from every mark path), not a dirty count;
        // `group_has_dirty` cuts the constant by 8, not the order. The
        // correction is lane `oldgen2`'s, filed as
        // `docs/feature-designs/generative-gc-architecture.md`.
        // The reason the cards are consumed here is the CLEARING contract in
        // the paragraph above, not an asymptotic one.
        //
        // gen r4w3/cards3 (2026-09-23): with the card table's SUMMARY map on
        // (`CRATONVM_GC_CARD_SUMMARY`, default) the sweep reads one summary
        // byte per 64 cards up to the bound and card bytes only in groups that
        // hold a mark, so its cost is ~bound/64 + 64 x (groups with a mark).
        // Still linear in the bound, at 1/64 of the constant.
        // gcj j1yg-pause: under `CRATONVM_DBG_GCPAUSE` the scan's parts go to
        // the `[gcpause]` counts (`cs_*`), in microseconds.
        // Only on the moving cycle's call (`par` given), whose `[gcpause]`
        // line takes the counts.
        let cs_t0 = (gc_flags().dbg_gcpause && par.is_some()).then(std::time::Instant::now);
        let dirty_indices = card_table.take_dirty_cards();
        // gcj j2/jt2 residual 4: the cards the take-over's register completion
        // dirtied since the last scan; their header-holders are read whole
        // below even when element cards are trusted. Taken with the cards, so
        // a completion is consumed by the scan that reads its card.
        let completion_whole_cards = card_table.take_completion_whole_scan_cards();
        cratonvm_gc_core::gc_metrics::record_cards_found_dirty(dirty_indices.len() as u64);
        if dirty_indices.is_empty() {
            return CardScanGapRoots::default();
        }

        let card_base = card_table.base_addr();
        let card_size = cratonvm_gc_core::card_table::CARD_SIZE;
        let old_base = old_gen.base_ptr() as usize;

        // Round-11 perf: instead of `walk_objects()` (which allocates a Vec
        // of *every* old-gen object and then filters per-object), translate
        // the dirty card indices into byte-offset ranges relative to the
        // old-gen data buffer and let `walk_objects_in_card_ranges` collect
        // only the objects that actually start inside a dirty card.
        //
        // The card table covers a region starting at `card_base`; old-gen
        // object pointers may sit at `old_base >= card_base`. Convert each
        // dirty card's `[card_start, card_end)` address window into an
        // offset window relative to `old_base`, clamped to `[0, capacity)`.
        // Cards entirely below the old gen contribute nothing.
        //
        // gcd d2/h (2026-09-27): no sort. `take_dirty_cards` answers from one
        // ascending byte scan since gcd d1/d (it debug-asserts ascending and
        // distinct), and the coalescing below needs exactly that order; the
        // `sort_unstable` that stood here was a full pass over every dirty
        // card per young pause for an already-sorted list.
        debug_assert!(
            dirty_indices.windows(2).all(|w| w[0] < w[1]),
            "scan_dirty_cards: take_dirty_cards must answer ascending"
        );
        let old_cap = old_gen.capacity();
        let mut dirty_ranges: Vec<(usize, usize)> = Vec::with_capacity(dirty_indices.len());
        for &card_idx in &dirty_indices {
            let card_start = card_base.saturating_add(card_idx * card_size);
            let card_end = card_start.saturating_add(card_size);
            // Skip cards that end at or before the old-gen base.
            if card_end <= old_base {
                continue;
            }
            let lo = card_start.saturating_sub(old_base).min(old_cap);
            let hi = card_end.saturating_sub(old_base).min(old_cap);
            if lo < hi {
                // Coalesce with the previous range if the cards are
                // contiguous (adjacent dirty cards are common).
                if let Some(last) = dirty_ranges.last_mut() {
                    if last.1 >= lo {
                        last.1 = last.1.max(hi);
                        continue;
                    }
                }
                dirty_ranges.push((lo, hi));
            }
        }
        if dirty_ranges.is_empty() {
            return CardScanGapRoots::default();
        }

        // Walk old gen, collecting only objects whose start offset lands in
        // a dirty card range. Preserves the original semantics (an object
        // is a root iff the card containing its *header* is dirty).
        //
        // gen r4w3/cards3 (2026-09-23): two changes, both behind per-heap
        // switches with the old behaviour as the other arm.
        //
        // * The walk starts each dirty range at its card's block-offset
        //   anchor (`OldGen::walk_card_ranges`) instead of decoding every
        //   old-gen header from offset 0 up to the last dirty range
        //   (`CRATONVM_GC_OLD_BOT=0` restores that). Same object set.
        // * When the card table carries element-precise marks
        //   (`CardTable::precise_ref_array_marks`), a reference array that
        //   OVERLAPS a dirty range without its header being in one is also
        //   reported, with the overlap as a byte window, and only the
        //   elements whose slot starts inside that window are read. An object
        //   whose header card is dirty is still scanned whole, which is what
        //   every header-card mark (the collector's deferred re-marks, JIT
        //   `aastore`, field stores) relies on.
        let precise = card_table.precise_ref_array_marks();
        // gen r5w1/oldgen5: plus the gap windows — old-gen bytes behind a
        // header no walk can size that a dirty range reaches. Before this they
        // were skipped, so a young object referenced only from an object in
        // there was not a root
        // (`gengc-r4w4-oldgen4-a-walk-gap-hides-old-to-young-roots-from-the-card-scan`).
        // Their words are scanned conservatively AFTER the precise hits, and
        // returned; they are never added to `extra_roots`, whose consumers
        // rewrite slots.
        // gcd d5/s (cards3 item 3, the consumer; opt-in
        // `CRATONVM_GC_PRECISE_ARRAY_HEADER_CARD`, needs an element-precise
        // table): trust element cards for a reference array whose HEADER card
        // is dirty too. Such a hit is read only in the dirty ranges it
        // overlaps (`array_dirty_windows`, the header's range included)
        // instead of whole. Sound only while every producer of a reference
        // array's old->young edge marks the ELEMENT's card on such a table
        // (producers (a)-(g) of the page); off, a header mark still means
        // "scan the whole object", byte-for-byte as before.
        let trust_element_cards = precise
            && false;
        let young_lo = young_from.base_ptr() as usize;
        let hit_scan = CardHitScan {
            young: (young_lo, young_lo + young_from.capacity()),
            trust_element_cards,
            completion_whole_cards: &completion_whole_cards,
            dirty_ranges: &dirty_ranges,
            card_base,
            card_size,
            old_base,
        };
        // gcj j1yg-pause (`CRATONVM_GC_PAR_CARD_SCAN`, default ON, `=0`
        // kills): the walk and the slot reads below, split across the
        // evacuation workers by contiguous groups of dirty ranges. The groups'
        // edges, concatenated in group order, are exactly the serial loop's,
        // in its order. `None` -- no group's output used -- when a group met
        // a header it cannot size (the serial walk reports that stretch as a
        // gap window) or the block-offset table refuses.
        if let Some(t0) = cs_t0 {
            moving_phase_count_push("cs_cards", dirty_indices.len() as u128);
            moving_phase_count_push("cs_ranges", dirty_ranges.len() as u128);
            moving_phase_count_push("cs_take_us", t0.elapsed().as_micros());
        }
        if let Some((pool, workers, parts)) = par {
            if let Some(groups) = par_card_scan(
                old_gen,
                &dirty_ranges,
                precise,
                &hit_scan,
                pool,
                workers,
                values.is_some(),
                parts,
            ) {
                let parts = &parts[..groups];
                extra_roots.reserve(parts.iter().map(|p| p.edges.len()).sum());
                if let Some(v) = values.as_deref_mut() {
                    v.reserve(parts.iter().map(|p| p.vals.len()).sum());
                }
                for part in parts {
                    extra_roots.extend_from_slice(&part.edges);
                    if let Some(v) = values.as_deref_mut() {
                        v.extend_from_slice(&part.vals);
                    }
                }
                if let Some(t0) = cs_t0 {
                    moving_phase_count_push("cs_total_us", t0.elapsed().as_micros());
                }
                return CardScanGapRoots::default();
            }
        }
        let (hits, gap_windows) = old_gen.walk_card_ranges_with_gaps(&dirty_ranges, precise);
        for hit in hits {
            // SAFETY: a hit of `walk_card_ranges_with_gaps` over this pause's
            // dirty ranges.
            unsafe { hit_scan.scan(hit, extra_roots, values.as_deref_mut()) };
        }
        if gap_windows.is_empty() {
            return CardScanGapRoots::default();
        }
        card_walk_gap_young_words(old_base, &gap_windows, young_from)
    }

    /// The capacity of each young semi-space.
    pub fn young_semi_capacity(&self) -> usize {
        self.lock_young_from().capacity()
    }

    /// The capacity of the old generation.
    pub fn old_gen_capacity(&self) -> usize {
        self.old_gen.lock().capacity()
    }

    /// Old-generation free bytes including usable in-place growth space, for
    /// the allocation-failure crawl backstop. Occupancy and young-trigger
    /// policies continue to use the current capacity.
    pub fn old_gen_headroom_with_growth(&self) -> usize {
        self.old_gen.lock().headroom_with_growth()
    }

    /// The number of bytes currently used in the young from-space.
    pub fn young_from_used(&self) -> usize {
        self.lock_young_from().used()
    }

    /// gcj j4/rf (`gcj-j1rf-gen-reference-and-finalizer-review-residuals`
    /// item 4) — is the next collection certain to collect NO old storage?
    /// Conservative: this thread has no requested major
    /// (`gc_quiescence::major_gc_requested`), and the old generation would
    /// stay untriggered even if the whole young from-space extent were
    /// promoted (`OldGen::young_pause_certainly_leaves_old`). Both trigger
    /// sites decide with exactly those inputs plus this pause's real
    /// promotions, which the extent bounds.
    /// gcj j1yg-pause round 3 (`CRATONVM_GEN_MINOR_PROMISE`, default ON, `=0`
    /// kills): [`Self::next_pause_certainly_minor`], or -- when that cannot be
    /// proven only because this pause's own promotions might make an occupancy
    /// trigger due ([`OldGen::young_pause_may_promise_minor`]) -- a PROMISE
    /// that it stays minor: its trigger decision then defers such a major to
    /// the next pause. The reference passes rely on the answer (old referents
    /// kept in their slots, old rows set aside); were a major to run anyway
    /// (an allocation failure arising inside the pause), those referents are
    /// simply kept one more cycle, never freed under a live slot. Records the
    /// promise for this pause; called once, by the pre-collection reference
    /// pass.
    pub fn decide_next_pause_minor(&self) -> bool {
        let certain = self.next_pause_certainly_minor();
        let promised = !certain
            && !self.minor_promise_just_deferred.load(Ordering::Acquire)
            && !cratonvm_gc_core::gc_quiescence::major_gc_requested()
            && {
                let promotable = self.young_from_used();
                self.old_gen.lock().young_pause_may_promise_minor(promotable)
            };
        self.minor_promised.store(promised, Ordering::Release);
        certain || promised
    }

    /// gcj j1yg-pause round 3: majors a minor promise deferred so far.
    pub fn minor_promise_deferrals(&self) -> u64 {
        self.minor_promise_deferrals.load(Ordering::Relaxed)
    }

    pub fn next_pause_certainly_minor(&self) -> bool {
        if cratonvm_gc_core::gc_quiescence::major_gc_requested() {
            return false;
        }
        let promotable = self.young_from_used();
        self.old_gen.lock().young_pause_certainly_leaves_old(promotable)
    }

    /// Bytes on the young from-space free list.
    ///
    /// ORCHESTRATOR, 2026-09-21 — this exists because `young_from_used()` alone
    /// cannot express what a NON-MOVING cycle did. `used()` is the arena's bump
    /// high-water; the non-moving sweep reclaims dead objects onto the free
    /// list *inside* that extent and never retracts the cursor, so `used()` is
    /// unchanged across the collection **by construction**.
    ///
    /// That is not a cosmetic point. The first `--verbose:gc` run under the
    /// repaired `CRATONVM_NO_MOVING_YOUNG` printed
    /// `young=3410256->3410256` identically on cycles 50 through 1500 — a
    /// field that reads as "this collection reclaimed nothing" on a run that
    /// completed 1511 collections in a 256 MB heap without an
    /// `OutOfMemoryError`, i.e. one that was plainly reclaiming the whole
    /// time. Pair the two numbers and the line says something true.
    pub fn young_from_free_bytes(&self) -> usize {
        self.lock_young_from().free_list_bytes()
    }

    /// The number of bytes currently used in the old generation.
    pub fn old_gen_used(&self) -> usize {
        self.old_gen.lock().used()
    }

    /// Check if a pointer is in the young from-space.
    pub fn is_in_young(&self, ptr: *const u8) -> bool {
        self.lock_young_from().contains(ptr)
    }

    /// Check if a pointer is in EITHER young semispace. A PRE-GC young
    /// address sits in the buffer that became the (empty) to-space after the
    /// Cheney swap, which `is_in_young` (from-space only) misses — reference
    /// processing uses this to detect stale/dead pre-GC Reference addresses
    /// (in young + not in the pointer map ⇒ the object did not survive).
    pub fn is_in_young_either(&self, ptr: *const u8) -> bool {
        self.lock_young_from().contains(ptr) || self.young_to.lock().contains(ptr)
    }

    /// Check if a pointer is in the old generation.
    pub fn is_in_old(&self, ptr: *const u8) -> bool {
        self.old_gen.lock().contains(ptr)
    }

    /// Walk all live objects in both young and old generations.
    /// Returns a Vec of (raw pointer, total byte size) for each object.
    /// Must be called during a GC safepoint (all mutator threads paused).
    pub fn walk_objects(&self) -> Vec<(*mut u8, usize)> {
        let mut result = self.walk_young_objects();

        // Walk old generation
        {
            let old = self.old_gen.lock();
            result.extend(old.walk_objects());
        }

        result
    }

    /// gci i14/hd (`gci-i14hd-g1-heap-diagnostics-review-findings-FIXED-20261016`
    /// item 4): [`Self::walk_objects`] as a visitor, the same objects in the
    /// same order, for the heap dump and the class histogram. The young half
    /// is still listed (its size is bounded by the young generation), the old
    /// half is visited in slices of 64 Ki objects
    /// (`OldGen::walk_objects_from`), so the walk no longer needs a list of
    /// every old object first. The old-generation lock is held only while a
    /// slice is gathered, never while `f` runs, so `f` may use accessors that
    /// take it. Stop-the-world only (the `walk_objects` contract): nothing
    /// frees or slides old storage between slices, so a returned resume
    /// offset stays an object start.
    pub fn for_each_object(&self, f: &mut dyn FnMut(*mut u8, usize)) {
        const GEN_VISIT_SLICE: usize = 64 * 1024;
        for (ptr, size) in self.walk_young_objects() {
            f(ptr, size);
        }
        let mut slice: Vec<(*mut u8, usize)> = Vec::with_capacity(GEN_VISIT_SLICE);
        let mut from = 0usize;
        loop {
            slice.clear();
            let next = {
                let old = self.old_gen.lock();
                old.walk_objects_from(from, GEN_VISIT_SLICE, |p, s| slice.push((p, s)))
            };
            for &(ptr, size) in &slice {
                f(ptr, size);
            }
            match next {
                // A resume that visited nothing cannot make progress.
                Some(n) if !(slice.is_empty() && n == from) => from = n,
                _ => break,
            }
        }
    }

    /// Walk all objects in the young from-space only (to-space is GC scratch).
    /// Returns a Vec of (raw pointer, total byte size) for each object.
    /// Must be called during a GC safepoint (all mutator threads paused).
    pub fn walk_young_objects(&self) -> Vec<(*mut u8, usize)> {
        self.walk_young_objects_and_gaps().0
    }

    /// [`Self::walk_young_objects`], plus the from-space stretches the walk
    /// could NOT parse and stepped over (absolute `[lo, hi)`, ascending): the
    /// span from an anomaly to the free block it re-anchored at, or to `used`
    /// when nothing was left to re-anchor on (and after a `HumongousFiller`
    /// sentinel). The enumeration is exactly `walk_young_objects`'s.
    ///
    /// gen r5w6/conc10: `collect_young_to_old_roots` word-scans these (see
    /// there); the objects inside one are real objects the enumeration drops.
    pub fn walk_young_objects_and_gaps(&self) -> (Vec<(*mut u8, usize)>, Vec<(usize, usize)>) {
        YOUNG_WALK_ENTRIES[4].fetch_add(1, Ordering::Relaxed);
        let mut result = Vec::new();
        let mut gaps: Vec<(usize, usize)> = Vec::new();

        // Walk young generation (from-space only — to-space is GC scratch).
        //
        // BUGFIX [gc-genheap]: after `sweep_young_non_moving` the from-space is
        // NOT a dense run of live objects from offset 0. The non-moving sweep
        // leaves (a) zeroed dead spans published on the free list and (b)
        // sub-`HEADER_SIZE` GAP_FILLER sentinels left in place (see
        // `install_tail_filler` / the sweep loop ~4129-4152). Walking linearly
        // from offset 0 with no hole-skipping and no sentinel handling reads a
        // zeroed free span as a `num_slots=0` 40-byte object (or worse, a
        // class_id=0/kind=Object 0-size header → break, or a non-grid stride →
        // desync into a live object's interior). A zeroed-but-unpublished gap
        // can even decode as size 0 and spin.
        //
        // Mirror `OldGen::walk_objects` / the sweep loop / `clear_all_mark_bits_in_arena`:
        // take `free_blocks_sorted()` and skip known free blocks, stride over
        // GAP_FILLER sentinels by their offset-4 length, and use
        // `gen_object_total_size` (which flags corrupt headers as size 0) with
        // the same `total_size < HEADER_SIZE` corruption stop.
        {
            let young = self.lock_young_from();
            let base = young.base_ptr() as usize;
            let used = young.used();
            let free_blocks = young.free_blocks_sorted();
            let mut free_iter = free_blocks.iter().peekable();
            let mut offset: usize = 0;
            // gen r5w1/young5: the grid rules are `probe_grid_at`'s, with this
            // walk's policy unchanged: a malformed GAP filler is parsed (and
            // lands in the corrupt-header arm), and a hole-crossing header is
            // still emitted as an object, as it always was here.
            //
            // DoHead walk-desync hardening (2026-07-02): an unlisted zeroed
            // span is not parseable — re-anchor at the next free block instead
            // of breaking (which would silently drop every later object from
            // the enumeration) or striding phantoms.
            //
            // A run of EMPTY objects is not a desync. This walk has no live set
            // either (it enumerates a from-space, live and dead alike), so what
            // carries it is again that an empty object has NO FIELDS. That is
            // exactly what this walk's callers consume.
            // `collect_young_to_old_roots` reads REF SLOTS to build the
            // concurrent old-gen marker's young->old roots, which the call site
            // calls mandatory — "a missed mark root here = cleanup frees a live
            // object" — and a run of empty objects contributes none. Whereas
            // the anomaly arm re-anchors at the next free block and so drops
            // every REAL object between the run and that anchor from the
            // enumeration, roots and all. Resuming on-grid strictly reduces
            // what is lost. The run's own members are skipped rather than
            // emitted as HEADER_SIZE entries: `hprof::dump_heap` is the other
            // caller, and a multi-megabyte run of dead empty objects would
            // balloon the dump for no fidelity a consumer relies on.
            let rules = GridRules {
                bad_gap_filler_is_anomaly: false,
                ..GridRules::unmarked(base, used)
            };
            let mut no_phantom_probe = 0usize;
            while offset < used {
                // Skip known free blocks — their bytes are stale dead spans and
                // must never be parsed as object headers. DoHead walk-desync
                // hardening (2026-07-02): robust skip (handles overshoot).
                if skip_free_blocks(&mut offset, &mut free_iter).0 {
                    continue;
                }
                let ptr = (base + offset) as *mut u8;
                // SAFETY: `ptr` is within `young_from.used()` region; reading the header is valid.
                let header = unsafe { &*(ptr as *const ObjectHeader) };
                // Round-9 gc CRIT-1: HumongousFiller is a walker sentinel
                // installed by the regional GC. The generational heap
                // never allocates humongous, but defensively stop the
                // scan if the kind byte is the sentinel value (treating
                // it as a real object would mis-parse the rest of the
                // stripe).
                if header.kind() == ObjectKind::HumongousFiller {
                    // gen r5w6/conc10: the rest of the arena is not walked.
                    gaps.push((base + offset, base + used));
                    break;
                }
                let next_free = free_iter.peek().map(|&&(off, _)| off);
                // `offset < used` inside mapped from-space (`probe_grid_at`'s
                // contract). `gen_object_total_size` (inside the probe) returns
                // 0 for an array with an implausible length or a kind=Object
                // header with a non-zero array_length / oversized num_slots;
                // that is the `BadSize` arm, which re-anchors the walk
                // (matching the sweep).
                let verdict = probe_grid_at(offset, next_free, &rules, &mut no_phantom_probe);
                let total_size = match verdict {
                    GridProbe::Gap { len } => {
                        offset += len;
                        continue;
                    }
                    GridProbe::EmptyRun { resume } => {
                        offset = resume;
                        continue;
                    }
                    GridProbe::Object { size } | GridProbe::Phantom { size, .. } => size,
                    // gcf f4/y (young5 P1): a hole-crossing header is not
                    // emitted; its stretch up to the free block is a GAP, which
                    // the callers already treat conservatively.
                    GridProbe::ZeroSpan { .. }
                    | GridProbe::BadSize { .. }
                    | GridProbe::BadGapFiller
                    | GridProbe::CrossesFreeBlock { .. } => {
                        if matches!(verdict, GridProbe::ZeroSpan { .. }) {
                            LATE_WALK_ZERO_RUNS[2].fetch_add(1, Ordering::Relaxed);
                        }
                        // gen r5w6/conc10: the stretch the re-anchor steps over.
                        let stretch_lo = offset;
                        if resync_to_next_free_block(&mut offset, &mut free_iter) {
                            gaps.push((base + stretch_lo, base + offset.min(used)));
                            continue;
                        }
                        gaps.push((base + stretch_lo, base + used));
                        break;
                    }
                };
                result.push((ptr, total_size));
                offset += total_size;
            }
        }

        (result, gaps)
    }

    /// Collect every young-gen object's reference to an OLD-gen object.
    ///
    /// fork6 GC_STRESS fix — these are mandatory roots for the concurrent
    /// old-gen mark (`ConcurrentMarker::initial_mark` / `remark`): those
    /// phases filter the thread-root list with `old_gen.contains`, so an old
    /// object whose only path from a root goes THROUGH a young object
    /// (root → young holder → old target) was invisible and the concurrent
    /// sweep freed it live. Selective promotion mass-produces exactly that
    /// shape (it tenures a young object's children while the pinned holder
    /// stays young), so under allocation pressure the old cycle reclaimed
    /// live promoted objects — the all-zero-header `class_id=0` receivers
    /// and silently-dropped static writes in the Fork6Hard GC_STRESS lane.
    ///
    /// Walks ALL young objects (live or dead): a dead young holder's old refs
    /// only over-retain (floating garbage until the next cycle), never corrupt.
    /// Must be called during a GC safepoint (all mutator threads paused, so
    /// young object bodies are stable to read).
    pub fn collect_young_to_old_roots(&self) -> Vec<usize> {
        let (young_objs, stretches) = self.walk_young_objects_and_gaps();
        let mut out = Vec::new();
        let old = self.old_gen.lock();
        for (ptr, _sz) in young_objs {
            // SAFETY: `ptr` came from the hardened young walk above; the
            // header and body are readable, and no mutator runs (STW).
            let header = unsafe { &*(ptr as *const ObjectHeader) };
            // SAFETY: `ptr`/`header` are a valid young object under STW.
            unsafe {
                for_each_ref_slot(ptr, header, |r, _| {
                    if old.contains(r as *const u8) {
                        out.push(r as usize);
                    }
                });
            }
        }
        // gen r5w6/conc10 — the objects of an unparseable stretch are not in
        // the enumeration above; their words are seeds (see the function).
        let _ = y2o_live::seed_old_words_of_young_stretches(&stretches, &old, &mut out);
        out
    }

    /// gen r5w6/conc10 — the class ids of every young object the walk could
    /// parse, and whether it parsed ALL of from-space (no unparseable
    /// stretch). A `false` means some real young object's class is missing
    /// from the set, so a caller that turns classes into live loaders (the
    /// concurrent class unload's `young_instance_loaders`) must not trust the
    /// set to be complete. Inside a pause, no heap lock held.
    pub fn young_object_class_ids(&self) -> (FxHashSet<u32>, bool) {
        let (objs, stretches) = self.walk_young_objects_and_gaps();
        let mut ids = FxHashSet::default();
        for (ptr, _size) in objs {
            // SAFETY: an object start from the hardened young walk; the world
            // is stopped, so the header is readable and stable.
            ids.insert(unsafe { (*(ptr as *const ObjectHeader)).class_id.as_u32() });
        }
        (ids, stretches.is_empty())
    }

    /// gcd d2/g — the class ids of every object the old generation holds, and
    /// whether the walk covered ALL of it (no region it could not parse). The
    /// old half of the stop-the-world retained-layout census
    /// (`ConcurrentGcState::complete_stw_layout_census`); the young half is
    /// [`Self::young_object_class_ids`]. Inside a pause, no heap lock held.
    pub fn old_object_class_ids(&self) -> (FxHashSet<u32>, bool) {
        let old_gen = self.old_gen.lock();
        let (objs, gaps) = old_gen.walk_objects_with_gaps();
        let mut ids = FxHashSet::default();
        for (ptr, _size) in objs {
            // SAFETY: an object start from the old generation's walk, under
            // its lock, with the world stopped.
            ids.insert(unsafe { (*(ptr as *const ObjectHeader)).class_id.as_u32() });
        }
        (ids, gaps.is_empty())
    }

    /// gcstress residual face-1 diagnostics (`CRATONVM_DBG_CELLCORRUPT`) —
    /// dump everything known about the HOLDER of a corrupt Value cell plus a
    /// generation-containment probe of the stale pointer the cell carries.
    /// Cold path: runs only when the gate is set AND the cell already failed
    /// `read_value_checked`.
    pub fn dump_corrupt_cell_holder(
        &self,
        obj_ref: ObjectRef,
        header: &ObjectHeader,
        index: usize,
        cell_ptr: *mut u8,
    ) {
        // SAFETY: `cell_ptr` is a readable 16-byte slot (get_field bounds-checked).
        let raw = unsafe { std::ptr::read(cell_ptr as *const [u64; 2]) };
        let class_name = cratonvm_gc_core::gc::resolve_class_info(header.class_id.as_u32())
            .map(|(n, _)| n)
            .unwrap_or_else(|| "<unresolved>".to_string());
        let target = raw[0] as usize;
        let t = target as *const u8;
        let (yf, yt, og) = (
            self.lock_young_from().contains(t),
            self.young_to.lock().contains(t),
            self.old_gen.lock().contains(t),
        );
        let holder_addr = obj_ref.as_ptr() as usize;
        let (hyf, hog) = (
            self.lock_young_from().contains(holder_addr as *const u8),
            self.old_gen.lock().contains(holder_addr as *const u8),
        );
        // Neighbor window: a misaligned-by-8 tagged Value write (the bc
        // math-ec "0x4" family — receiver pointer off by 8) leaves its
        // discriminant in the PREVIOUS cell's payload word and its payload in
        // THIS cell's discriminant word; the ±2-cell dump makes that pattern
        // (small disc at odd word positions) directly visible.
        // SAFETY: the window lies within the holder's body ± one cell; the
        // holder is a live validated object and heap arenas pad allocations,
        // so the reads stay inside mapped arena memory.
        let win: [u64; 8] = unsafe { std::ptr::read((cell_ptr as usize - 16) as *const [u64; 8]) };
        eprintln!(
            "[CELLCORRUPT] holder=0x{holder_addr:x} (young_from={hyf} old={hog}) \
             class_id={} class={class_name} kind=0x{:02x} num_slots={} array_len={} \
             gc_flags=0x{:x} index={index} raw0=0x{:016x} raw1=0x{:016x} | \
             raw0-target: young_from={yf} young_to={yt} old={og}\n\
             [CELLCORRUPT]   window cell-1..cell+2: {:016x},{:016x} | {:016x},{:016x} | \
             {:016x},{:016x} | {:016x},{:016x}\n{}",
            header.class_id.as_u32(),
            ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)),
            header.num_slots(),
            header.array_length(),
            header.gc_flags(),
            raw[0],
            raw[1],
            win[0],
            win[1],
            win[2],
            win[3],
            win[4],
            win[5],
            win[6],
            win[7],
            std::backtrace::Backtrace::force_capture(),
        );
        // Shift test — the neighbor windows show corrupt cells decoding as an
        // 8-BYTE-SHIFTED body ({payload_k, disc_k+1}). Mechanically test: do
        // this holder's cells parse as valid Values when read at ±8? A
        // consistent hit means the body content sits 8 bytes off its slots
        // (overlapping/shifted allocation — the A2 double-serve family), not
        // a per-cell stray write.
        {
            let n = (header.num_slots() as usize).min(8);
            let base = holder_addr + HEADER_SIZE;
            let mut plus8 = 0usize;
            let mut minus8 = 0usize;
            let mut aligned = 0usize;
            for k in 0..n {
                let c = base + k * SLOT_SIZE;
                // SAFETY: within the holder body ±8; arena-mapped.
                unsafe {
                    if cratonvm_types::read_value_checked(c as *const Value).is_some() {
                        aligned += 1;
                    }
                    if cratonvm_types::read_value_checked((c + 8) as *const Value).is_some() {
                        plus8 += 1;
                    }
                    if cratonvm_types::read_value_checked((c - 8) as *const Value).is_some() {
                        minus8 += 1;
                    }
                }
            }
            eprintln!(
                "[CELLCORRUPT]   shift-test over {n} cells: valid@aligned={aligned} \
                 valid@+8={plus8} valid@-8={minus8}",
            );
        }
        // If the stale target is still inside a CURRENT generation, dump its
        // header too — its identity often names the mis-writing code path.
        if yf || og {
            // SAFETY: `target` is inside a live arena (checked above); the
            // first 40 header bytes of any in-arena address are readable.
            let th = unsafe { std::ptr::read(target as *const ObjectHeader) };
            let tname = cratonvm_gc_core::gc::resolve_class_info(th.class_id.as_u32())
                .map(|(n, _)| n)
                .unwrap_or_else(|| "<unresolved>".to_string());
            eprintln!(
                "[CELLCORRUPT]   target-header: class_id={} class={tname} kind=0x{:02x} \
                 num_slots={} array_len={} gc_flags=0x{:x}",
                th.class_id.as_u32(),
                ObjectHeader::kind_tag(th.mark_word.load(Ordering::Relaxed)),
                th.num_slots(),
                th.array_length(),
                th.gc_flags(),
            );
        }
    }
}
/// gcstress residual face-1 gate — see `dump_corrupt_cell_holder`.
#[inline]
pub fn cell_corrupt_diag_enabled() -> bool {
    gc_flags().dbg_cellcorrupt
}

/// gcstress residual face-1 hunt (`CRATONVM_DBG_CELLCORRUPT`) — validate a
/// legacy object's Value cells at a GC copy SOURCE. A corrupt cell caught
/// here proves the corruption happened before this copy and reports the
/// victim's pre-copy (young, often bootstrap-page-stable) address — the
/// address a follow-up `CRATONVM_DBG_WATCH_CELL` run must watch to catch the
/// forming write. Rate-capped; no-op unless the gate is set.
pub fn validate_copy_source_cells(obj_ptr: *const u8, header: &ObjectHeader, site: &str) {
    if !cell_corrupt_diag_enabled()
        || header.kind() != ObjectKind::Object
        || is_compact_object(header)
    {
        return;
    }
    static HITS: AtomicUsize = AtomicUsize::new(0);
    for k in 0..header.num_slots() as usize {
        let c = obj_ptr as usize + HEADER_SIZE + k * SLOT_SIZE;
        // SAFETY: within the source object's body (walk-validated under STW).
        if unsafe { cratonvm_types::read_value_checked(c as *const Value) }.is_none() {
            if HITS.fetch_add(1, Ordering::Relaxed) < 16 {
                // SAFETY: same cell, raw read.
                let raw = unsafe { std::ptr::read(c as *const [u64; 2]) };
                eprintln!(
                    "[CELLCORRUPT:{site}] PRE-COPY corrupt cell: src_obj=0x{:x} class_id={} \
                     num_slots={} slot={k} cell=0x{c:x} raw={:016x},{:016x}",
                    obj_ptr as usize,
                    header.class_id.as_u32(),
                    header.num_slots(),
                    raw[0],
                    raw[1],
                );
            }
        }
    }
}

impl Default for GenerationalHeap {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for GenerationalHeap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let yf = self.lock_young_from();
        let yt = self.young_to.lock();
        let og = self.old_gen.lock();
        f.debug_struct("GenerationalHeap")
            .field("young_from_used", &yf.used())
            .field("young_from_capacity", &yf.capacity())
            .field("young_to_used", &yt.used())
            .field("young_to_capacity", &yt.capacity())
            .field("old_gen_used", &og.used())
            .field("old_gen_capacity", &og.capacity())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Size / slot access helpers
// ---------------------------------------------------------------------------

/// Selective-promotion fixup: rewrite every reference field of `obj` that
/// points at an evacuated (forwarded) young object to its new old-gen address,
/// via `fwd_of` (returns `Some(new_addr)` for an evacuated young target, else
/// `None`). When `points_young` is `Some`, it is set `true` if any field still
/// points at a *non-evacuated* young object after fixup — the caller uses that
/// to maintain the old→young card invariant when `obj` itself now lives in old
/// gen. `in_young` classifies an address as young-from-space.
///
/// SAFETY: `obj` is a valid object header; its reference slots lie within the
/// object's body. No mutator runs (STW).
pub fn fixup_object_fields(
    obj: *mut u8,
    header: &ObjectHeader,
    fwd_of: &dyn Fn(usize) -> Option<usize>,
    in_young: &dyn Fn(usize) -> bool,
    mut points_young: Option<&mut bool>,
) {
    // Arrays + compact objects use 8-byte pointer slots; legacy objects use
    // 16-byte Value cells. `forward_ref_slots` rewrites a slot only when the
    // closure returns Some (an evacuated target's new address); a non-evacuated
    // young referent flips `points_young` and leaves the slot unchanged.
    // SAFETY: `obj`/`header` are a valid object under STW (no mutator).
    unsafe {
        forward_ref_slots(obj, header, |ref_ptr| {
            let target = ref_ptr as usize;
            if let Some(nw) = fwd_of(target) {
                Some(nw as *mut u8)
            } else {
                if let Some(p) = points_young.as_deref_mut() {
                    if in_young(target) {
                        *p = true;
                    }
                }
                None
            }
        });
    }
}

/// Selective-promotion verify (CRATONVM_SP_VERIFY): count reference fields of
/// `obj` that still point at a forwarded (evacuated) young object after the
/// fixup pass. A nonzero count is a MISSED fixup — a dangling reference into a
/// reclaimed young slot, the bintrees18 wrong-checksum smoking gun.
pub fn forwarded_ref_count(obj: *mut u8, header: &ObjectHeader, is_y: &dyn Fn(usize) -> bool) -> usize {
    let mut n = 0usize;
    // SAFETY: `obj`/`header` are a valid object header under STW.
    unsafe {
        for_each_ref_slot(obj, header, |raw, _| {
            let t = raw as usize;
            if is_y(t) {
                // SAFETY: `is_y` confirmed `t` is a young-from-space heap address,
                // so it points at a valid `ObjectHeader`.
                let h = &*(t as *const ObjectHeader);
                if h.is_forwarded() {
                    n += 1;
                }
            }
        });
    }
    n
}

/// Invoke `f` with each non-null reference target address held by `obj`'s
/// reference fields (object Value slots or reference-array elements). Used by
/// the CRATONVM_SP_VERIFY aliasing detector.
pub fn for_each_ref(obj: *mut u8, header: &ObjectHeader, mut f: impl FnMut(usize)) {
    // SAFETY: `obj`/`header` are a valid object header under STW.
    unsafe {
        for_each_ref_slot(obj, header, |raw, _| f(raw as usize));
    }
}

/// `CRATONVM_DBG_GCWRITE` gate (bc math-ec diagnostic), read from the latched
/// `GcFlags` snapshot so the per-object-copy check in `forward_object` pays a
/// field load rather than an `env::var_os` lookup on the hot GC path. (The doc
/// used to say "cached in a `OnceLock`", which it has not been since the flag
/// moved into `GcFlags`.)
#[inline]
pub fn gcw_enabled() -> bool {
    gc_flags().dbg_gcwrite
}

/// Cached CRATONVM_DBG_DESCTRACE gate (temp investigation aid, ALV5th GC
/// bug): trace every forward_object relocation of a
/// org/junit/runner/Description or java/util/concurrent/ConcurrentLinkedQueue
/// instance (old addr -> new addr, identity hash, promoted-or-not, age).
#[inline]
pub fn desc_trace_enabled() -> bool {
    gc_flags().dbg_desctrace
}

/// Cached `CRATONVM_DBG_FWDGUARD` gate (bc math-ec 0x4): log forward_object
/// candidates whose header class_id does NOT resolve (false interior/stale
/// roots that would get a forwarding_ptr smashed into live-object interiors).
#[inline]
pub fn fwdguard_enabled() -> bool {
    gc_flags().dbg_fwdguard
}

/// Cached `CRATONVM_FWD_RESOLVE_STRICT` gate: REJECT (leave unmoved, no
/// forwarding install) forward_object candidates with unresolvable class_id.
#[inline]
pub fn fwd_resolve_strict() -> bool {
    gc_flags().fwd_resolve_strict
}

/// Cached `CRATONVM_DBG_SEEDHUNT` gate (bc math-ec `0x4` seed-phase bisect).
/// When on, `collect_garbage_inner` scans the young + old arenas for object/
/// array reference slots holding `Object(Some(p))` with `0 < p < 0x1000` (the
/// `0x4` corruption signature) at three points — GC entry, after the Cheney
/// loop, and after a possible major GC — printing per-phase counts. A count
/// that JUMPS at a specific phase localizes the SEEDING collector path
/// (minor Cheney/promotion vs. major mark-compact) — see
/// bc-math-ec-gc-0x4-handoff.md §6.4.
#[inline]
pub fn seedhunt_enabled() -> bool {
    gc_flags().dbg_seedhunt
}

/// Moving-young diagnostic: after Cheney scanning but before from-space reset,
/// report any surviving heap reference that still points at a forwarded
/// young-from object. Nonzero means a heap reference rewrite was missed; zero
/// with a wrong result points at an unenumerated root/home outside the heap.
#[inline]
pub fn moving_young_dangling_verify_enabled() -> bool {
    gc_flags().moving_young_verify
}

/// Speculative to-space (or old-gen) copies the parallel evacuator ABANDONED
/// after losing the forwarding CAS, for the cycle currently being verified.
///
/// # Why the verifier above is unreadable without this
///
/// `ParEvac::evacuate` copies first and claims second, so a worker that loses
/// the race has already written a complete object at `new_addr` and simply
/// walks away from it: "to-space garbage reclaimed next cycle". Nothing
/// references it, so nothing scans it, so **every one of its reference slots
/// keeps the pre-move address** — and the verifier, which walks all of to-space
/// and old-gen, reports each of them as a missed heap rewrite.
///
/// That is exactly the shape
/// `internal/fixed-suite-bugs/netty/bytebuf-multiplethreads-npe-generational-blocked-wake-jit-remap-FIXED-20260908.md`
/// §6 published a root cause off and then withdrew: *"an object whose every
/// slot was missed"*, twice, never reproducible, and refuted by a scan-cursor
/// instrument that could not explain it either. An abandoned loser explains
/// both halves — whole-object, because the object was never scanned; and
/// harmless, because it is unreachable garbage and no mutator can dereference
/// it.
///
/// Populated only while the verifier is armed, so the ordinary evacuation path
/// pays nothing.
pub static ABANDONED_EVAC_COPIES: parking_lot::Mutex<Vec<usize>> = parking_lot::Mutex::new(Vec::new());

/// Record one abandoned CAS-loser copy. No-op unless the verifier is armed.
pub fn record_abandoned_evac_copy(addr: usize) {
    if !moving_young_dangling_verify_enabled() {
        return;
    }
    ABANDONED_EVAC_COPIES.lock().push(addr);
}

/// Take (and clear) this cycle's abandoned copies, as a set the verifier can
/// test membership against.
pub fn take_abandoned_evac_copies() -> rustc_hash::FxHashSet<usize> {
    let mut g = ABANDONED_EVAC_COPIES.lock();
    std::mem::take(&mut *g).into_iter().collect()
}

/// Scan a single object's reference slots for the `0x4` seed signature
/// (`Object(Some(0 < p < 0x1000))` in a field, or a raw `0 < x < 0x1000` in a
/// ref-array element). Returns the number of bad slots found; prints up to
/// `cap` victims (shared budget via `printed`).
pub fn seedhunt_scan_obj(
    optr: *mut u8,
    h: &ObjectHeader,
    label: &str,
    arena: &str,
    printed: &mut usize,
    cap: usize,
) -> usize {
    // gengc-round4-move (2026-09-23): through `for_each_ref_slot`, like the
    // `CRATONVM_DBG_RSET_AUDIT` walks next door. This used to hand-walk
    // `0..num_slots` as legacy 16-byte `Value` cells for every `kind = Object`
    // header — wrong for a compact-layout object (`GC_FLAG_COMPACT`), whose
    // `num_slots` is a field count over a packed body, so the loop strode past
    // the object's end: the exact SIGSEGV the stw-residual-close fix
    // (2026-07-23) removed from the rset audit, left behind in this helper.
    // `for_each_ref_slot` also caps a garbage array length, which the raw loop
    // did not. Null slots are skipped by the helper, as they were here.
    let is_array = h.kind() == ObjectKind::Array;
    let mut count = 0usize;
    // SAFETY: callers pass an object start from a walk that sized it
    // (`gen_object_total_size`) or from `OldGen::walk_objects`, so the header
    // is valid and its reference slots are in bounds — `for_each_ref_slot`'s
    // contract.
    unsafe {
        for_each_ref_slot(optr, h, |raw, slot_id| {
            let p = raw as usize;
            if p < 0x1000 {
                count += 1;
                if *printed < cap {
                    *printed += 1;
                    eprintln!(
                        "[seedhunt] {} {}{} @0x{:x} cid={} {}[{}] -> 0x{:x}",
                        label,
                        arena,
                        if is_array { "ARR" } else { "" },
                        optr as usize,
                        h.class_id.as_u32(),
                        if is_array { "arr" } else { "fld" },
                        slot_id,
                        p,
                    );
                }
            }
        });
    }
    count
}

/// Scan a contiguous, bump-allocated young arena (Cheney to-space, or the
/// post-swap from-space) for the `0x4` seed signature. Linear walk by
/// `gen_object_total_size`; bails on a malformed header (these spaces are
/// freshly compacted/contiguous so the walk normally reaches every object).
pub fn seedhunt_scan_young(
    base: *const u8,
    used: usize,
    label: &str,
    printed: &mut usize,
    cap: usize,
) -> usize {
    let mut count = 0usize;
    let mut cur = 0usize;
    // gen r5w1/young5: the grid rules are `probe_grid_at`'s (a contiguous
    // arena: no free list; `Parse` zero runs; any anomaly ends the scan).
    //
    // gengc-round4-move (2026-09-23): a PARALLEL cycle's to-space holds 8-byte
    // GAP-filler sentinels over retired buffer tails, which
    // `gen_object_total_size` cannot size; they are strided as every other
    // young walk does instead of ending the scan at the first one.
    let rules = GridRules {
        base: base as usize,
        used,
        zero_run_hi: used,
        side_sorted: &[],
        zero_runs: ZeroRunRule::Parse,
        bad_gap_filler_is_anomaly: true,
        phantom: false,
    };
    let mut no_phantom_probe = 0usize;
    while cur < used {
        // SAFETY: `cur < used`; pointer stays within the arena.
        let optr = unsafe { base.add(cur) as *mut u8 };
        let size = match probe_grid_at(cur, None, &rules, &mut no_phantom_probe) {
            GridProbe::Gap { len } => {
                cur += len;
                continue;
            }
            GridProbe::Object { size } => size,
            GridProbe::BadGapFiller
            | GridProbe::EmptyRun { .. }
            | GridProbe::ZeroSpan { .. }
            | GridProbe::BadSize { .. }
            | GridProbe::CrossesFreeBlock { .. }
            | GridProbe::Phantom { .. } => break,
        };
        // SAFETY: the probe parsed a plausible header at this in-bounds offset.
        let h = unsafe { &*(optr as *const ObjectHeader) };
        count += seedhunt_scan_obj(optr, h, label, "Y", printed, cap);
        cur += size;
    }
    count
}

/// xt-hardening follow-up (2026-07-03): reject a conservative candidate whose
/// header's ALWAYS-ZERO field is non-zero. It closes the residual (rarer,
/// non-zero-word0) slice of the `mark_young` conservative-candidate
/// false-positive family that the zero-word0 side-mark-set fix (2026-07-03,
/// same session) does not cover — a candidate whose garbage predecessor bytes
/// happen to satisfy the kind/num_slots/array_length/extent bounds but fail
/// this near-free check. Real objects always pass.
///
/// WHICH field, and how much it buys, have BOTH changed twice and the text here
/// tracked neither until 2026-09-08. It said `_padding` (offset 6-7) and
/// `_gc_reserved` (offset 22-23) — fields the 32 -> 24 -> 16 header shrink
/// deleted in 2026-08 — and quoted "~1/2^29" from summing their bytes with "5
/// undefined gc_flags bits", when `gc_flags` is a FOUR-bit field. The real
/// figure was 1 undefined bit, i.e. 1/2, off by 28 orders of magnitude; and
/// `GC_FLAG_HEADER` then took that bit too, leaving 0.
///
/// What the screen actually rests on now is `MARK_RESERVED_MASK`, the mark
/// word's two reserved bits, giving 3/4 — see
/// [`header_reserved_fields_plausible`]. Keep the arithmetic in one place and
/// state it as a rate, not as a folklore constant: a screen quoted at 1/2^29
/// while delivering 1/2 is a guard everyone believes is doing work it is not.
/// The screen every old-gen mark-worklist push must pass.
///
/// [`OldGen::contains`] is a bare bounds check — no alignment, no header
/// validation — so without this any word that happens to land inside old gen's
/// byte range gets its `gc_flags` blindly read-modify-written and is then fed to
/// `scan_object_for_old_refs` as an `ObjectHeader`. Two consequences, both
/// reproduced by the tests named below: the mark bit lands in an unrelated LIVE
/// object's PAYLOAD (an `int` field `0x41414141` becomes `0x43414141`), and
/// those payload bytes are decoded as a header, which is how a byte that is not
/// a valid `ObjectKind` discriminant reaches `gen_object_total_size`. See
/// `gc-old-gen-mark-accepts-unvalidated-addresses-FIXED.md`.
///
/// The root seed and the young->old conservative word scan already defended
/// themselves (and their comments name this hazard as the reason); this is that
/// same screen, factored out so the other seven push sites get it too.
///
/// `conservative` adds the zero-word0 neighbour disambiguation that only
/// register/stack-scanned GUESSES need. It is deliberately NOT applied to
/// precise references: a `ClassId(0)` ad-hoc container legitimately has an
/// all-zero word0, and rejecting one that a real reference slot points at would
/// free a live object. Every other clause is a property real objects always
/// have, so precise sites can take them all with no risk of a false reject —
/// which matters, because at a precise site a false reject IS a premature free.
pub fn old_gen_mark_candidate_plausible(ptr: *mut u8, old_gen: &OldGen, conservative: bool) -> bool {
    if (ptr as usize) & 0x7 != 0 || !old_gen.contains(ptr) {
        return false;
    }
    // gen r4w4 (orchestrator, 2026-09-24): the WHOLE header must be readable
    // old-gen storage, not just its first byte. `OldGen::contains` admits the
    // last `HEADER_SIZE - 1` bytes of the reservation, and `READ_SLACK` cannot
    // commit past the reservation's end, so on a completely full old
    // generation (`readable_end == len`) a conservative guess there passed and
    // the header read below faulted one byte into whatever follows the
    // reservation: SIGSEGV at exactly `old_base + capacity`, from
    // `GenR4W4HeapFullThrashProbe` under gc-stress. No object can start there
    // (every object is at least `HEADER_SIZE` bytes), so rejecting it never
    // drops a real one.
    if !old_gen.contains(ptr.wrapping_add(HEADER_SIZE - 1)) {
        return false;
    }
    // SAFETY: `ptr` is 8-aligned and inside old gen, so the header read is
    // in-bounds.
    let header = unsafe { &*(ptr as *const ObjectHeader) };
    // Compare `kind` as a RAW BYTE: an out-of-range discriminant is exactly the
    // case this screen exists to catch, and the optimiser is entitled to assume
    // the enum is in 0..=2 — which would fold the check away.
    // SAFETY: as above; `cratonvm_types::KIND_TAGS_BYTE_OFFSET` is inside the header.
    let kind_byte = unsafe { cratonvm_types::kind_tag_at(ptr) };
    if kind_byte > ObjectKind::Array as u8 {
        return false;
    }
    // gen r5w5 (pin9 cross-lane, applied by the orchestrator): the shape reads
    // below resolve a forwarded header; a candidate whose "forward" leaves old
    // gen is not a header (see `forward_chain_stays_inside`).
    let in_old = |t: usize| {
        old_gen.contains(t as *const u8)
            && old_gen.contains(t.wrapping_add(HEADER_SIZE - 1) as *const u8)
    };
    if !forward_chain_stays_inside(header, in_old) {
        return false;
    }
    if kind_byte == ObjectKind::Array as u8 {
        // HIB-DCAST-LATEPHASE.1: validate the raw `element_type` tag byte
        // too, before `gen_object_total_size(header)` below reads
        // `header.element_type` for its `array_data_size` call.
        //
        // # gengc-round2-core2 (2026-09-20): the ORIGINAL reason is gone; the
        // # screen stays, on the reason that survived.
        //
        // The text this replaces said an unvalidated `element_type` byte was
        // "instant UB the moment it is loaded", and cited a `SIGILL` on
        // `DefaultCatalogAndSchemaTest`. That was true of a `#[repr(u8)]`
        // field read through a typed `.read()`. It is no longer how the header
        // is decoded: the quartet lives in the atomic mark word and both
        // decoders are TOTAL — `ObjectHeader::kind_of` maps the one unmapped
        // 2-bit value to `Object`, and `ObjectHeader::element_type` is
        // `array_element_type_from_tag(tag).unwrap_or(Reference)`. Nothing on
        // this path transmutes a byte into an enum, so there is no
        // discriminant UB left to prevent, and a reader who believed otherwise
        // would think this screen must never be relaxed for a reason that has
        // evaporated. See
        // `docs/feature-designs/generative-gc-architecture.md`.
        //
        // What the screen still does, and why it is kept: a tag outside
        // `ArrayElementType` proves this address was never an object base. The
        // total decoder does not make such an address safe — it makes it
        // SILENT. `element_type().unwrap_or(Reference)` on garbage yields a
        // 4-or-8-byte element width applied to a garbage `array_length`, i.e.
        // exactly the out-of-bounds read HIB-DCAST-LATEPHASE.1 actually
        // measured as a SIGSEGV. That is a memory-safety argument about
        // EXTENTS, not about discriminants, and it is unaffected by the
        // decoders becoming total.
        //
        // Expected value: this reject should essentially never fire on a
        // healthy run — it is counted through `OLDMARK_BAD_KIND_HITS` at the
        // reporting sites, and a non-zero reading is a finding (a real
        // corruption, or a side table holding a non-base address), not tuning.
        // SAFETY: as above; `cratonvm_types::KIND_TAGS_BYTE_OFFSET` is inside the header.
        let elem_tag = unsafe { cratonvm_types::element_type_tag_at(ptr) };
        if array_element_type_from_tag(elem_tag).is_none() {
            return false;
        }
        if header.array_length() > i32::MAX as u32 {
            return false;
        }
    } else if header.num_slots() > (1 << 24) {
        return false;
    }
    if !header_reserved_fields_plausible(header) {
        return false;
    }
    if conservative {
        // SAFETY: reads the first header word at an 8-aligned in-bounds ptr.
        let word0 = unsafe { *(ptr as *const u64) };
        if word0 == 0 && victim8_neighbor_explains_zero_prefix(ptr, old_gen) {
            return false;
        }
    }
    let total = gen_object_total_size(header);
    // gen r4/mark (2026-09-23): `wrapping_add`, not `add`. The extent under
    // test is CLAIMED by a header this function has not yet accepted, and a
    // garbage `array_length` near `i32::MAX` claims gigabytes. `ptr.add(n)` is
    // UB unless the result stays inside the same allocation — which is the very
    // property `contains` is about to test — so the old `unsafe { ptr.add(..) }`
    // was UB on exactly the inputs this screen exists to reject. Wrapping
    // arithmetic has no such precondition and `contains` is a pure range check.
    //
    // gen r5w3/oldgen7: `contains_range` over the whole claimed extent —
    // the same answer as `contains(ptr + total - 1)` without holes, and a
    // refusal for an extent that crosses a granule an interior decommit
    // released (`OldGen::decommit_interior_free_runs`): the mark scans the
    // object this admits, up to that extent.
    total >= HEADER_SIZE && old_gen.contains_range(ptr, total)
}

/// `CRATONVM_DBG=sweep-referrers` (legacy `CRATONVM_DBG_SWEEP_REFERRERS=1`) —
/// word-scan the heap for referrers of every block an old-gen reclamation is
/// about to drop, on BOTH arms: the in-place sweep and the compactor. Off by
/// default: the scan is O(heap / 8) per cycle.
///
/// This used to be two names OR'd together, `CRATONVM_DBG_SWEEP_REFERRERS` and
/// `CRATONVM_DBG_COMPACT_REFERRERS` — one switch spelled twice, which is the
/// exact duplication `flag_groups` exists to undo. Neither was declared, so
/// neither was reachable through `CRATONVM_DBG=` or arrangeable by
/// `flags::with_thread_overrides`, and the second spelling appeared nowhere
/// outside this function. Collapsed to the one name a known-issue repro
/// already uses (`known-issues/h2/bug-h2-classid0-stale-address-family.md`).
pub fn doomed_referrers_dbg() -> bool {
    false
}

/// See [`doomed_referrers_dbg`]. Reports the first few (victim, referrer)
/// pairs with enough context to name the mark source that missed the edge.
pub fn report_doomed_referrers(
    label: &str,
    walked: &[(*mut u8, usize)],
    old_gen: &OldGen,
    young_from: &Arena,
    roots: &[ObjectRef],
) {
    use std::collections::HashSet;
    let doomed: HashSet<usize> = walked
        .iter()
        .filter(|&&(p, _)| {
            // SAFETY: `walk_objects` yielded `p` as a valid object start.
            unsafe { (*(p as *const ObjectHeader)).gc_flags() & GC_FLAG_MARKED == 0 }
        })
        .map(|&(p, _)| p as usize)
        .collect();
    if doomed.is_empty() {
        eprintln!("[{label}-referrers] nothing doomed this cycle");
        return;
    }
    // Bloom-style prefilter: 4 M bits over the doomed set.
    const FILTER_WORDS: usize = 1 << 16;
    let mut filter = vec![0u64; FILTER_WORDS];
    let hash = |a: usize| -> usize {
        ((a >> 3).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 42) as usize & ((FILTER_WORDS * 64) - 1)
    };
    for &d in &doomed {
        let h = hash(d);
        filter[h >> 6] |= 1u64 << (h & 63);
    }
    let bases: Vec<usize> = walked.iter().map(|&(p, _)| p as usize).collect();
    // Which walked object CONTAINS this address, if any.
    let owner_of = |addr: usize| -> Option<(usize, bool)> {
        let i = bases.partition_point(|&b| b <= addr);
        if i == 0 {
            return None;
        }
        let (base, size) = (bases[i - 1], walked[i - 1].1);
        (addr < base + size).then(|| {
            // SAFETY: a walked base.
            let m = unsafe { (*(base as *const ObjectHeader)).gc_flags() & GC_FLAG_MARKED != 0 };
            (base, m)
        })
    };

    // Categorise, do not just print the first N. The first sixteen words of an
    // ascending address scan are the sixteen LOWEST addresses, which says
    // nothing about whether any LIVE thing points into the doomed set — and
    // that is the only question worth asking here. On a real workload the
    // doomed set is a condemned subgraph tens of thousands of objects wide, so
    // doomed->doomed edges outnumber the interesting ones by ~10^5:1 and drown
    // them completely (measured: 1_298_639 "referrer words" of which every one
    // of the printed sixteen was doomed->doomed).
    //
    // Only three categories are defects, and each names a different mark source:
    //   live_old  a MARKED old-gen object still points at a doomed block — the
    //             mark BFS was handed the referrer and did not follow this edge,
    //             and `close_live_set` (which runs before this scan) did not
    //             rescue it either;
    //   young     a young-space word points at a doomed block —
    //             `mark_young_to_old_refs` missed a young->old edge;
    //   root      a root points straight at a doomed block — the root seed
    //             dropped it.
    // The other two are expected and are counted only so the interesting ones
    // can be read as a fraction of a known total:
    //   dead_old  doomed->doomed, i.e. a subgraph condemned together;
    //   unowned   a word in old-gen backing store that is inside no walked
    //             object at all (free-list space, padding, a reserved tail);
    //   stale     the containing object IS marked, and its own slot enumerator
    //             does not yield the victim. Overwhelmingly a 16-byte legacy
    //             `Value` cell that once held an `Object` and was overwritten
    //             by a narrower variant, leaving the old pointer in the cell's
    //             UNUSED upper half. The mutator can never read it and the GC
    //             correctly ignores it, so counting it as a defect is a false
    //             alarm — and before this split it was the ONLY thing
    //             `live_old` ever fired on (~1 per compaction, always the same
    //             class pair, byte-identical across three processes).
    let mut live_old = 0usize;
    let mut stale = 0usize;
    let mut dead_old = 0usize;
    let mut unowned = 0usize;
    let mut young = 0usize;
    let mut root_hits = 0usize;
    let mut printed = 0usize;

    let describe = |victim: usize| -> String {
        // SAFETY: `victim` is a walked base.
        let vh = unsafe { &*(victim as *const ObjectHeader) };
        format!(
            "class_id={} kind={} num_slots={}",
            vh.class_id.as_u32(),
            ObjectHeader::kind_tag(vh.mark_word.load(Ordering::Relaxed)),
            vh.num_slots(),
        )
    };

    // 1. Roots.
    for (i, r) in roots.iter().enumerate() {
        let a = r.as_ptr() as usize;
        if doomed.contains(&a) {
            root_hits += 1;
            if printed < 24 {
                printed += 1;
                eprintln!(
                    "[{label}-referrers] DEFECT root[{i}] points at DOOMED {a:#x} ({})",
                    describe(a),
                );
            }
        }
    }

    // 2. Old-gen backing store, then 3. the young space.
    //
    // gen r4/mark (2026-09-23): old gen is scanned to `high_water()`, not to
    // `capacity()`: nothing above the high-water mark can hold a referrer (it
    // is the one-past-the-end of every allocation ever made), and scanning
    // to it keeps this O(allocated).
    //
    // gen r4w6/oldpin6 (2026-09-24,
    // `gengc-r4w5-review5-old-gen-diagnostics-scan-to-high-water-past-a-shrink`):
    // ...and only its COMMITTED runs. `oldgen4`'s shrink decommits
    // `[keep_end, capacity)` and keeps `high_water`, so `[keep_end,
    // high_water)` faulted on the first touch after any shrink of a
    // generation whose peak was above the new committed end. The runs also
    // skip the holes the Windows commit-limit arm can leave below
    // `readable_end`. (The old comment's `set_len` over a `with_capacity`
    // buffer described the store before `HeapStore`; it no longer applies.)
    let old_spans = old_gen.committed_spans_below(old_gen.high_water());
    let old_scan_len: usize = old_spans.iter().map(|&(_, len)| len).sum();
    let mut scans: Vec<(usize, usize, bool)> = old_spans
        .into_iter()
        .map(|(a, len)| (a, len, true))
        .collect();
    scans.push((young_from.base_ptr() as usize, young_from.used(), false));
    for (base, len, is_old) in scans {
        let mut off = 0usize;
        while off + 8 <= len {
            // SAFETY: `[base+off, base+off+8)` is inside a mapped range: for
            // old gen a committed run `committed_spans_below` reported (granule
            // aligned at its start, so `base + off` is 8-aligned), for young
            // the used prefix of from-space.
            let w = unsafe { *((base + off) as *const u64) } as usize;
            if w == 0 {
                off += 8;
                continue;
            }
            let h = hash(w);
            if filter[h >> 6] & (1u64 << (h & 63)) == 0 || !doomed.contains(&w) {
                off += 8;
                continue;
            }
            let at = base + off;
            if is_old {
                match owner_of(at) {
                    // A word INSIDE the doomed block itself is not a referrer.
                    Some((owner, _)) if owner == w => {}
                    Some((owner, true)) => {
                        // Marked is necessary, not sufficient. Ask the object's
                        // own slot enumerator — the one every mark source
                        // funnels through — whether it actually yields this
                        // victim. See the `stale` note above for what the
                        // difference turned out to be.
                        // SAFETY: `owner` is an object base `walk_objects`
                        // yielded, so its header and body are valid.
                        let oh = unsafe { &*(owner as *const ObjectHeader) };
                        let mut yields_victim = false;
                        // SAFETY: as above — `for_each_ref_slot`'s contract.
                        unsafe {
                            for_each_ref_slot(owner as *mut u8, oh, |p, _| {
                                if p as usize == w {
                                    yields_victim = true;
                                }
                            });
                        }
                        if !yields_victim {
                            stale += 1;
                            if stale <= 2 {
                                // Print the raw cell once or twice so the
                                // reason is on the page rather than in a
                                // commit message. A legacy slot is a 16-byte
                                // `Value`, so a word at body offset `n*16 + 8`
                                // is the payload half and the discriminant sits
                                // 8 bytes below it.
                                let body_off = (at - owner).saturating_sub(HEADER_SIZE);
                                let cell = body_off / SLOT_SIZE;
                                let cell_at = owner + HEADER_SIZE + cell * SLOT_SIZE;
                                // SAFETY: the word is inside the object's body,
                                // which `walk_objects` sized, so the whole
                                // 16-byte cell is in bounds.
                                let (c0, c1) = unsafe {
                                    (*(cell_at as *const u64), *((cell_at + 8) as *const u64))
                                };
                                eprintln!(
                                    "[{label}-referrers] stale-padding: {owner:#x} \
                                     (class_id={}, {}) cell {cell} @{cell_at:#x} \
                                     raw=[{c0:#018x}, {c1:#018x}] holds {w:#x} in a half its \
                                     decoder does not read — NOT a referrer",
                                    oh.class_id.as_u32(),
                                    if cratonvm_gc_core::heap::with_compact_oop_scan(oh, |_, _| ()).is_some() {
                                        "compact"
                                    } else {
                                        "legacy"
                                    },
                                );
                            }
                            off += 8;
                            continue;
                        }
                        live_old += 1;
                        if printed < 24 {
                            printed += 1;
                            eprintln!(
                                "[{label}-referrers] DEFECT live old-gen object {owner:#x} \
                                 (marked) slot+{} points at DOOMED {w:#x} ({})",
                                at - owner,
                                describe(w),
                            );
                        }
                    }
                    Some((_, false)) => dead_old += 1,
                    None => unowned += 1,
                }
            } else {
                young += 1;
                if printed < 24 {
                    printed += 1;
                    eprintln!(
                        "[{label}-referrers] DEFECT young word {at:#x} points at DOOMED \
                         {w:#x} ({})",
                        describe(w),
                    );
                }
            }
            off += 8;
        }
    }

    // Cross-thread peer-scan coverage for THIS cycle. A sweep with
    // `UNCLASSIFIED > 0` marked from a root set that provably omitted a
    // running peer's JIT frames, and no amount of heap-side scanning can see
    // those roots — they are in registers and stack slots, not in the heap or
    // the root slice.
    let (xt_passes, xt_taken, xt_unclassified, xt_roots, xt_hw, xt_hw_roots) =
        cratonvm_gc_core::gc_quiescence::xt_cycle_coverage();

    // Report what was actually SCANNED beside what was found. A zero in the
    // young or root column is only an elimination if that scan covered
    // something: `young_from.used()` can be near zero right after a young
    // sweep, and a short root slice would make `root=0` vacuous. Without these
    // three numbers the line is an inert-lever reading dressed up as evidence.
    eprintln!(
        "[{label}-referrers] doomed={} DEFECTS(live_old={live_old} young={young} \
         root={root_hits}) benign(dead_old={dead_old} unowned={unowned} \
         stale_padding={stale}) \
         scanned(old_bytes={} young_bytes={} roots={}) \
         xt(passes={} taken_over={} UNCLASSIFIED={} xt_roots={} hw_windows={} hw_roots={})",
        doomed.len(),
        old_scan_len,
        young_from.used(),
        roots.len(),
        xt_passes,
        xt_taken,
        xt_unclassified,
        xt_roots,
        xt_hw,
        xt_hw_roots,
    );
}

/// H2-CID0 (2026-08-02) — resolve an INTERIOR conservative root to the old-gen
/// object that CONTAINS it.
///
/// `walked` is [`OldGen::walk_objects`]' ascending output for
/// this pause, so containment here is a derivation off the object grid rather
/// than a guess about a header's bytes — the same standard
/// [`rescue_mark_candidate_by_walk`] holds itself to.
///
/// Returns `None` for an address that is not STRICTLY inside an object: an
/// exact base is the other paths' business, and an address in a free block or
/// an alignment gap belongs to no object at all. Deliberately does NOT require
/// 8-alignment — a derived pointer into a `byte[]` is a perfectly ordinary
/// unaligned interior word, and the object it names is just as live.
#[inline]
pub fn old_gen_interior_root_base(ptr: *mut u8, walked: &[(*mut u8, usize)]) -> Option<(usize, usize)> {
    let a = ptr as usize;
    // gengc-round2-core2 (2026-09-20): this took the base list and the
    // `(base, size)` list — the SAME data, indexed in lockstep. It now
    // searches one slice by key; `i - 1` still names the same entry.
    let i = walked.partition_point(|&(p, _)| (p as usize) <= a);
    if i == 0 {
        return None;
    }
    let (base_ptr, size) = walked[i - 1];
    let base = base_ptr as usize;
    (a > base && a < base + size).then_some((base, size))
}

/// gen r4w5/oldcompact5 (2026-09-24) — one PINNED old-gen compaction on the
/// non-moving path: where its pins come from, and what it did.
///
/// `docs/feature-designs/generative-gc-architecture.md`.
/// The sliding compactor needs a root set it can rewrite, and the non-moving
/// collection (`sweep_old_gen_non_moving`) runs exactly when it cannot: a
/// conservative word names an old-gen object, and moving that object would
/// leave the word — a register image, a spilled stack slot, a lost-tag
/// interpreter local — pointing at whatever slid there next. A JIT-warm
/// process takes that path on nearly every cycle, and the allocation-failure
/// ladder's explicit major request forces it (`explicit_full_gc`), so a
/// fragmentation refusal armed a compaction that never ran.
///
/// This lets that path compact anyway, with every object a conservative word
/// names PINNED in place (`OldGen::compact_around_pins`). The pins are
/// retained for THIS pause only: `words` is read when the plan is built, the
/// resolved bases are collected by `old_gen_gc_inner`'s mark into a local
/// `Vec`, and the plan is dropped with the pause. No process global is added.
///
/// # Where the conservative words come from, and why each source is covered
///
/// The old-gen mark (`old_gen_gc_inner`'s root loop, which screens with
/// `old_gen_mark_candidate_plausible(.., conservative = true)` or the walked
/// grid) cannot tell a precise root from a conservative one: one slice holds
/// both. It keeps the INTERIOR ones (`interior_pins`) because it must, and
/// discarded which base words were conservative. So the plan asks the
/// sources, and pins:
///
/// * **every interior root's object** — always (the existing `interior_pins`);
/// * **every word the root gatherer published as conservative for this
///   pause** — `gc_quiescence::pinned_jit_roots_snapshot()`: each thread's
///   compiled-frame band words (the initiator's `roots[jit_scan_start..]`,
///   each parked or blocked peer's deposit) and the take-over's helper-window
///   words; each resolved against the walked grid, base or interior;
/// * **every old-gen base a young-arena stretch the walk could not parse
///   names** — `mark_young_to_old_refs`' conservative fallback. The fixup walk
///   over the same stretch (`fixup_young_old_refs`) rewrites any word equal to
///   a MOVED base, so such a base must not move;
/// * **every root in the caller's slice** (`pin_root_range`) when a
///   conservative source ran that publishes NOTHING: the interpreter's
///   conservative frame probe (`vm/src/memory/roots.rs`, step 1 when
///   `gc_quiescence::is_active()`, step 14a5 on an unregistered JIT frame),
///   and `CRATONVM_GC_G1_ONLY_JIT_PINS`, which withholds the JIT publication
///   on this backend. Those words sit in the slice unmarked as conservative,
///   so the only safe pin set is all of it. Promotion destinations appended
///   to the slice by `sweep_old_gen_non_moving` are NOT in the range: they
///   are precise, and the VM remaps them through the returned map.
///
///   gen r4w6/oldpin6 (2026-09-24): the interpreter probe's words are now
///   PUBLISHED by the collection door for its own thread
///   ([`OldPinRootLayout`], `vm/src/runtime/interpreter/gc_and_alloc.rs::
///   publish_old_pin_root_layout_for_pause`), so the range narrows from the
///   whole slice to the PEERS' part of it (their snapshots and the take-over
///   words, which the door cannot re-derive). The initiator's precise roots —
///   statics, side tables, handles, `Object`-tagged frame slots, all of which
///   the VM remaps through the returned map — no longer pin. Only
///   `CRATONVM_GC_G1_ONLY_JIT_PINS`, or a door that published no layout,
///   still pins the whole slice.
///
/// What is NOT covered, stated so it is not mistaken for coverage: a word in
/// a peer's root snapshot deposited while the interpreter probe was engaged,
/// on a pause where it no longer is (a thread blocked since then). That is a
/// lost-tag interpreter local, and the MOVING young collector already moves
/// young objects under exactly such a local on every no-JIT cycle; the
/// root-gatherer request in the review page removes it for both.
///
/// Refused outright (the in-place sweep runs, as before this wave) when the
/// take-over licence is `NonMoving` or the pause scanned un-rewritable peer
/// state (a frozen in-JIT peer's derived pointers): the same gate selective
/// promotion honours.
pub struct OldPinnedCompact {
    /// Conservative words published for this pause (see above).
    pub words: Vec<usize>,
    /// The root-slice entries at indices `pin_root_range.0 .. pin_root_range.1`
    /// are pinned too (gen r4w6/oldpin6: a range, was a prefix).
    pub pin_root_range: (usize, usize),
    /// Out: the addresses whose cards the caller dirties so the next young
    /// collection finds the survivors' references into young: a survivor's
    /// FINAL base (its header card, "scan the whole object"), or, for a
    /// reference array on an element-precise table, the final address of
    /// each young element, one per card (gcd d3/n, cards3 item 3 producer
    /// (d)).
    pub young_ref_holders: Vec<usize>,
    /// In (gcd d3/n): `Some(card_table.base_addr())` when the card table is
    /// element-precise (`precise_ref_array_marks()`), so the compaction
    /// records reference-array elements' cards instead of the header's; set
    /// by `sweep_old_gen_non_moving_impl` before the plan runs, `None` (header
    /// marks, the pre-d3 behaviour) otherwise.
    pub element_card_base: Option<usize>,
    /// Out: `Some` when the compaction ran — `(pinned, moved, moved_bytes,
    /// largest free before, largest free after)`; `None` when it was declined
    /// and the in-place sweep ran instead.
    pub compacted: Option<(usize, usize, usize, usize, usize)>,
    /// Out: why it was declined, when it was.
    pub refused: Option<&'static str>,
    /// Out (gen r5w2/alloc6): where the pins came from and what they held —
    /// see [`OldPinReport`]. Filled just before the compaction runs.
    pub report: OldPinReport,
    /// gcf f4/c (`CRATONVM_GC_OLD_PIN_UNREWRITABLE_ONLY`): published
    /// compiled-frame words this plan does NOT pin because a rewritable
    /// channel names their object and no unrewritable word does
    /// ([`movable_words_unpinned`]). 0 with the switch off.
    pub movable_words_unpinned: usize,
    /// This was planned only because fragmentation was present at major start;
    /// it is subject to a stricter pin budget and cadence after settlement.
    pub fragmentation_triggered: bool,
    pub reactive_fragmentation_repair: bool,
    pub fragmentation_slide_goal: Option<usize>,
}

/// gcf f4/c — `CRATONVM_GC_OLD_PIN_UNREWRITABLE_ONLY` (opt-in, default OFF),
/// step 3 of `gengc-r5w6-old10-proposal-pinned-compaction-pins-only-unrewritable-words`:
/// the pinned compaction pins only the objects UNREWRITABLE compiled-frame
/// words name. Read once per plan.
pub fn old_pin_unrewritable_only_enabled() -> bool {
    false
}

/// gcf f4/c — split the published compiled-frame words into the pins and the
/// words whose object may move: a word is dropped iff it EQUALS an object
/// address a rewritable channel published this pause (`is_movable`: an oop-map
/// slot or a shadow-stack cell, which `remap_active_jit_frames` /
/// `remap_register_image_words` / `ShadowStack::remap` rewrite after the pause
/// through the map this collection returns, on the initiator and on every
/// resuming peer) and NO unrewritable word names it (`is_unrewritable`, the
/// veto: a callee-saved image, a blind spill, an operand slot above the live
/// cursor). Exactly G1's region-pin filter (`jit_pinned_region_set`) and the
/// moving young collector's pin rule, per address. An interior or
/// one-past-the-end word never equals a published object address, so it stays
/// a pin. Returns `(pins, dropped)`. Since gcg g3/e's H2 the production
/// call is [`movable_words_unpinned_by_thread`] (applied by gci i1/pn and by
/// GC wave 9 lane gcD); this single-thread rule stays as its reference (the
/// unit tests compare the two).
#[cfg_attr(not(test), allow(dead_code))]
pub fn movable_words_unpinned(
    words: Vec<usize>,
    is_movable: impl Fn(usize) -> bool,
    is_unrewritable: impl Fn(usize) -> bool,
) -> (Vec<usize>, usize) {
    let before = words.len();
    let pins: Vec<usize> = words
        .into_iter()
        .filter(|&w| !(is_movable(w) && !is_unrewritable(w)))
        .collect();
    let dropped = before - pins.len();
    (pins, dropped)
}

/// gcg g3/e — [`movable_words_unpinned`] made sound across threads
/// (`gcg-g3e-unrewritable-only-plan-unpins-words-other-threads-published-FIXED-20261012`).
///
/// `is_movable` / `is_unrewritable` read the CALLING thread's thread-local
/// sets (`gc_quiescence::MOVABLE_JIT_ROOTS` / `UNREWRITABLE_JIT_ROOTS`,
/// filled by the collector's own `collect_roots`; a peer's deposit skips both
/// inserts since gcf f4/j). They say nothing about a word ANOTHER thread
/// published (its safepoint or blocking deposit, a take-over helper window):
/// that word's slot class is unknown here, and a peer's blind spill, XMM
/// image or operand slot above its live cursor is never rewritten. The f4/c
/// rule filtered the merged snapshot by VALUE, so a peer's unrewritable word
/// naming an object the caller also named movably was dropped with the
/// caller's copy, and the object slid under the peer.
///
/// `own` is the caller's published words, `others` everything else in the
/// snapshot. Only `own` words are filtered (the f4/c rule), and only when no
/// word in `others` names the same address; every `others` word pins.
/// Returns `(pins, dropped)` like [`movable_words_unpinned`] (equal to it
/// when `others` is empty).
///
/// Called by `OldPinnedCompact::for_this_pause` under
/// `CRATONVM_GC_OLD_PIN_UNREWRITABLE_ONLY`, with
/// `gc_quiescence::pinned_jit_roots_snapshot_split()` (hunks H1 + H2, both
/// applied; H2 by gci i1/pn and by GC wave 9 lane gcD).
pub fn movable_words_unpinned_by_thread(
    own: Vec<usize>,
    others: Vec<usize>,
    is_movable: impl Fn(usize) -> bool,
    is_unrewritable: impl Fn(usize) -> bool,
) -> (Vec<usize>, usize) {
    let foreign: FxHashSet<usize> = others.iter().copied().collect();
    let before = own.len();
    let mut pins: Vec<usize> = own
        .into_iter()
        .filter(|&w| foreign.contains(&w) || !(is_movable(w) && !is_unrewritable(w)))
        .collect();
    let dropped = before - pins.len();
    pins.extend(others);
    (pins, dropped)
}

/// gen r5w2/alloc6 — the sources a pin of [`OldPinnedCompact`] can come from:
/// one bit each, and `bit.trailing_zeros()` indexes [`OldPinReport::by_source`].
///
/// * `INTERIOR` — a root of the slice lies strictly inside the object;
/// * `RANGE` / `RANGE_END` — a root in the plan's `pin_root_range` is the
///   object's base / lies one byte past its end (`ptr - 1` inside it);
/// * `WORD` / `WORD_END` — a published conservative word (`plan.words`) is the
///   object's base or inside it / lies one byte past its end;
/// * `YOUNG_STRETCH` — a word of a young stretch the walk could not parse
///   (`mark_young_to_old_refs`' conservative fallback).
pub const OLD_PIN_SRC_INTERIOR: u8 = 1 << 0;
pub const OLD_PIN_SRC_RANGE: u8 = 1 << 1;
pub const OLD_PIN_SRC_RANGE_END: u8 = 1 << 2;
pub const OLD_PIN_SRC_WORD: u8 = 1 << 3;
pub const OLD_PIN_SRC_WORD_END: u8 = 1 << 4;
pub const OLD_PIN_SRC_YOUNG_STRETCH: u8 = 1 << 5;
/// The sources whose pin is kept only for an object the mark REACHED — see
/// [`settle_old_pins`]. `INTERIOR` and `YOUNG_STRETCH` are not among them:
/// the first is marked by the root loop anyway (unless the
/// `CRATONVM_GC_NO_OLD_INTERIOR_PINS` negative control is on, whose pins keep
/// their old meaning), and the second is the one source the mark does not
/// mark itself and relies on the compaction's Phase 0a for.
pub const OLD_PIN_SRC_LIVE_ONLY: u8 =
    OLD_PIN_SRC_RANGE | OLD_PIN_SRC_RANGE_END | OLD_PIN_SRC_WORD | OLD_PIN_SRC_WORD_END;
/// How many of the largest pinned objects [`OldPinReport::largest`] keeps.
pub const OLD_PIN_REPORT_LARGEST: usize = 4;

/// gen r5w2/alloc6 — rate limit of the "humongous compaction not taken by
/// default" line (`sweep_old_gen_non_moving_impl`). A log budget, not state.
pub static HUMONGOUS_DEFAULT_DECLINE_REPORTS: AtomicUsize = AtomicUsize::new(0);

// ---------------------------------------------------------------------------
// gcd d4/n (2026-09-28): a REQUESTED major marks young from the true roots
// ---------------------------------------------------------------------------
//
// `gcd-d4j-stw-major-keeps-dead-old-data-through-young-nepotism-FIXED-20261012`: the
// young phase of a pause treats the OLD generation as live (a card, an old
// owner's overlay / mirror / metadata edge, an old instance's loader keeps a
// young object), and the old-generation mark then seeds from EVERY young
// survivor (`mark_young_to_old_refs`). So a DEAD old object that names a young
// object keeps, through it, everything that young object reaches in old gen
// alive through the major; a chain alternating k times between the
// generations needs k majors, and the allocation ladder threw
// `OutOfMemoryError` after one with the dropped data still in place.
//
// For a REQUESTED major (`System.gc()`, the allocation ladder's majors: the
// thread-local `major_gc_requested`, which diverts to the non-moving cycle by
// default) the old mark instead seeds young from the TRUE roots: the root
// slice, the finalizable candidates, and -- to a fixed point -- the young
// objects a MARKED old object names (fields, overlays, loader, mirror and
// metadata pins), each traced through the same edges the young marker follows
// (`scan_young_object`). Those are exactly the young marker's own inputs with
// "every old object" replaced by "every marked old object", so a young object
// left out is one the young phase kept only for a dead old object: nothing
// live reaches it, and the next young cycle frees it. HotSpot Serial's full
// collection has the same verdict (it marks the whole heap from the roots).
//
// Fail-safe arms: any unparseable young stretch is treated as roots (every
// word in it, every owner in it, every header's loader: what the legacy seed
// does for it); an old walk with a gap or a compacting collection keeps the
// legacy seed. Default ON; `CRATONVM_GC_FULL_GC_TRUE_ROOTS=0` restores the
// legacy seed everywhere.
//
// gcd d9/a (2026-09-28, `gcd-d5r-proposal-true-root-seed-beyond-requested-
// non-promoting-majors`, steps 1 and 2): the two commonest fallbacks are gone
// (`CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE`, default on, `=0` restores them).
//
// * A PROMOTING cycle. The root slice, the finalizer list, the watched set and
//   the `mirror_pin` / `metadata_pin` owner keys still name a promoted
//   object's PRE-promotion young address during the pause (the VM remaps them
//   after the collector returns; the young sweep already rewrote every heap
//   slot, 3a-3c). So the seed resolves a young address inside a promoted
//   source extent to its destination (`TrueRootYoung::promoted`) and pushes
//   THAT to the old mark, instead of making every destination a root. A
//   destination nothing reaches is dead: the old sweep frees it and
//   `sweep_old_gen_non_moving_impl` drops `source -> destination` from the
//   pointer map it returns, so every post-collection consumer reads the
//   source as dead (it is: the young sweep zeroed it onto the free list).
//   The rows keyed by a promoted OWNER's source (`mirror_pin`,
//   `metadata_pin`) are followed for every destination the mark reaches.
// * A planned PINNED compaction (`CRATONVM_GC_OLD_PINNED_COMPACT`). The only
//   thing the legacy seed gave the plan that the true-root seed did not was
//   the pins of an unparseable young stretch's old words (the fixup walk can
//   rewrite only an exact moved base there): `seed_stretches` now pins them
//   the same way (`OLD_PIN_SRC_YOUNG_STRETCH`). A dropped destination is
//   reported by the compaction itself (`OldGen::compact_around_pins_watching`).
//
// Step 1 (`CRATONVM_GC_FULL_GC_TRUE_ROOTS_FREE_YOUNG`, which zeroed the young
// survivors the seed left out onto the young free list in the same pause) was
// REMOVED by gce e2/c: it did not make the one-major ladder pass and it broke
// `NativeGrowthReclaimProbe` with OSR off. The excluded survivors wait for the
// next young collection.

/// `CRATONVM_GC_FULL_GC_TRUE_ROOTS` (default on).
pub fn full_gc_true_roots_enabled() -> bool {
    true
}

/// gcd d9/a: `CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE` (default on): the
/// true-root seed also on a promoting cycle and under a planned pinned
/// compaction (see the section comment). `=0` restores the d4/n..d8 fallbacks.
pub fn full_gc_true_roots_wide_enabled() -> bool {
    true
}

/// gch h4/pc — `CRATONVM_GC_TRUE_ROOT_ROUNDS_IN_BFS` (default ON since gci
/// i1/og and jr20 wave 9 lane G, `=0` kills and restores the per-object
/// rounds; vh4-pc A parity and D cost passed on h4a: steady 297.7 vs 319.7 ms,
/// thrash 98.4 vs 136.4 ms per major; on w9c thrash median per-major 103.2 vs
/// 117.1 ms;
/// `gch-h3mj-proposal-true-root-rounds-rescan-every-marked-old-object-DONE-20261015`,
/// fixes 1 and 2). Under the true-root young seed, the old mark's BFS hands
/// the young targets of every object it pops to [`TrueRootYoung`] in the scan
/// it already makes (slots: [`GenerationalHeap::scan_object_for_old_refs_split_in`];
/// overlays, mirror and metadata pins: the lookups the BFS body already does;
/// the class's loader: one lookup in a map of the YOUNG loaders only, empty on
/// most heaps). `TrueRootYoung::round` then drains only what was seeded,
/// instead of rescanning every marked old object's slots and repeating four
/// side-table lookups for it (vh3b-mj block P: `mark-young-rounds` 77-92 ms of
/// a 170 ms P-C major). The young set reached, and so every mark, is the same
/// on both arms (see [`TrueRootYoung::seed_from_bfs`]). Read once per
/// true-root major.
pub fn true_root_rounds_in_bfs_enabled() -> bool {
    true
}

/// gcj j3/x2 R2: rewrite every entry of `closure` that is a promotion
/// destination of this pause (a value of `pointer_map` that `is_old_dest`
/// accepts) to its source key. Nothing is allocated for an empty closure or
/// map, which is every pause without a resurrection round.
pub fn map_promotion_destinations_to_sources(
    closure: &mut [usize],
    pointer_map: &cratonvm_types::PointerMap,
    is_old_dest: impl Fn(usize) -> bool,
) {
    if closure.is_empty() || pointer_map.is_empty() {
        return;
    }
    let mut wanted: Vec<usize> = closure.iter().copied().filter(|&a| is_old_dest(a)).collect();
    if wanted.is_empty() {
        return;
    }
    wanted.sort_unstable();
    wanted.dedup();
    let back: FxHashMap<usize, usize> = pointer_map
        .iter()
        .filter(|&(_, dest)| wanted.binary_search(dest).is_ok())
        .map(|(&src, &dest)| (dest, src))
        .collect();
    if back.is_empty() {
        return;
    }
    for a in closure.iter_mut() {
        if let Some(&src) = back.get(a) {
            *a = src;
        }
    }
}

/// gcj j2/rf2 (ec47) — `CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS` (default ON,
/// `runtime_flag_default_on`, `=0` kills). A Generational major made the
/// finalizer candidates (the true-root seed's `pass.candidates`) and, on the
/// moving path, the resurrected finalizables the young phase promoted
/// (`finalizers_in_old`) and the P-C finalizer seeds ROOTS of its strong
/// closure, so the OLD children of a dead young finalizable were strongly
/// marked: the reference processor kept their weak references through the
/// pause that found the finalizable dead and cleared them one major late,
/// after `finalize()` -- never, when `finalize()` resurrects the object
/// (`GcWeakToFinalizerReachableDeepProbe old`). HotSpot clears them before
/// `finalize()`. ON: those seeds are marked in the resurrection round
/// ([`seed_deferred_finalizer_seeds`]), and what only they reach is the
/// depth-2 set. The final mark is the same set (seeding is monotone), so
/// nothing more is freed; only the depth-2 set grows. Read once per old
/// collection that has a finalizer pass. See
/// `docs/feature-designs/generative-gc-architecture.md`.
pub fn gen_major_defers_finalizer_seeds_enabled() -> bool {
    true
}

/// gcj j2/rf2 (ec47): old-gen objects the resurrection round marked from the
/// deferred finalizer seeds ([`seed_deferred_finalizer_seeds`]), run total
/// (the positive control of `CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS`;
/// printed under `CRATONVM_DBG_WATCHREF=1`).
pub static MAJOR_DEFERRED_FINALIZER_MARKS: AtomicU64 = AtomicU64::new(0);

/// gcj j3/rf3 (ec47, the legacy seed) —
/// `CRATONVM_GEN_LEGACY_SEED_DEFERS_RESURRECTED` (default ON,
/// `runtime_flag_default_on`, `=0` kills; engaged only while
/// [`gen_major_defers_finalizer_seeds_enabled`] is). The legacy young seed of
/// a major (`mark_young_to_old_refs`: `CRATONVM_GC_FULL_GC_TRUE_ROOTS=0`, a
/// declined ask, a walk-gap or compaction fallback) marked the old referents
/// of EVERY young object, the young phase's resurrection set included, so the
/// old children of a dead young finalizable were strongly marked and their
/// weak references survived the pause that found it dead (rf2 finding 3).
/// ON: the members' old referents are marked in the resurrection round
/// (`OldGenFinalizerPass::young_resurrected` -> `deferred_seeds`). Same final
/// mark; only the depth-2 set grows. Read once per legacy-seeded major with a
/// finalizer pass and a non-empty resurrection set.
pub fn legacy_seed_defers_resurrected_enabled() -> bool {
    true
}

/// gcj j3/rf3 (ec47): old-gen referents the legacy young seed moved to the
/// resurrection round ([`legacy_seed_defers_resurrected_enabled`]), run total
/// (the positive control; printed under `CRATONVM_DBG_WATCHREF=1`).
pub static LEGACY_SEED_DEFERRED_REFERENTS: AtomicU64 = AtomicU64::new(0);

/// gcj j3/rf3: the current addresses of a young resurrection set recorded at
/// PRE-collection addresses (`closure`, the keys
/// `GenerationalHeap::note_resurrection_closure` collected this pause), mapped
/// through the pause's `pointer_map` (an unmapped key did not move), sorted
/// and deduplicated. Pure, for the tests.
pub fn young_resurrection_members_of(
    closure: &[usize],
    pointer_map: &cratonvm_types::PointerMap,
) -> Vec<usize> {
    let mut v: Vec<usize> = closure
        .iter()
        .map(|k| pointer_map.get(k).copied().unwrap_or(*k))
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// gcj j2/rf2 (ec47) — the resurrection round's half of
/// `CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS`, run by `old_gen_gc_inner` right
/// after `OldGenFinalizerPass::seed` with the strong closure's worklist empty.
///
/// * `deferred` (`OldGenFinalizerPass::deferred_seeds`): old-gen addresses are
///   marked and pushed (screened as every precise push site is; anything else
///   is ignored there), and under the true-root seed every one is also seeded
///   into the young trace (a post-copy young address, or a promotion source).
/// * `candidates`: the finalizer candidates the true-root seed did not seed
///   with the roots (empty when the switch is off). Seeded into the young
///   trace; a live one was reached already (idempotent).
///
/// The young trace is then drained, which pushes the old objects the seeded
/// young objects reach. Every push marks first, so the caller's loop pops each
/// of them while `resurrection_draining` is set and records it in `closure`.
/// Returns the number of old objects pushed.
pub fn seed_deferred_finalizer_seeds<G: OldMarkGrid + ?Sized>(
    deferred: &[usize],
    candidates: &[usize],
    young: Option<&mut TrueRootYoung>,
    ctx: &TrueRootCtx<'_, G>,
    worklist: &mut Vec<*mut u8>,
) -> usize {
    let before = worklist.len();
    for &addr in deferred {
        mark_and_push_old_gen_in(
            addr as *mut u8,
            ctx.old_gen,
            ctx.walked,
            worklist,
            "deferred finalizer seed",
        );
    }
    if let Some(y) = young {
        for &addr in candidates.iter().chain(deferred.iter()) {
            y.seed_word(addr);
        }
        y.drain(ctx, worklist);
    }
    let pushed = worklist.len().saturating_sub(before);
    if pushed > 0 {
        MAJOR_DEFERRED_FINALIZER_MARKS.fetch_add(pushed as u64, Ordering::Relaxed);
        if watchref_dbg() {
            eprintln!(
                "[watchref] major: deferred finalizer seeds marked {pushed} old object(s) in the resurrection round (total {})",
                MAJOR_DEFERRED_FINALIZER_MARKS.load(Ordering::Relaxed),
            );
        }
    }
    pushed
}

/// gch h4/pc — `CRATONVM_GC_TRUE_ROOT_SKIP_IDLE_POST_CLOSE` (default ON since
/// gci i1/og and jr20 wave 9 lane G, `=0` kills and restores the second
/// closure; vh4-pc A, Q and D passed on h4a). Under the true-root seed the old mark closes the live set twice when
/// the pause has finalizer candidates: once when the strong closure runs dry
/// (`pre_close`, repeated until it rescues nothing) and once more after the
/// finalizer round (`post_close`, gcf f1/c). When the finalizer round marked
/// NOTHING (every old candidate was already marked, the common case), the mark
/// state is exactly the one the last pre-close saw, so the post-close would
/// rescue nothing and return the same escape verdict: it is skipped and that
/// verdict reused (the sweep then reuses it as before). One O(live) pass per
/// such major. And when the finalizer round does mark (a dead candidate),
/// `OldGenFinalizerPass::seed`'s own `close_live_set` before it is skipped:
/// the pre-close has just closed that same mark state. Read once per
/// true-root major.
pub fn true_root_skip_idle_post_close_enabled() -> bool {
    true
}

/// gci i28/cl — `CRATONVM_GC_OLD_POST_CLOSE_INCREMENTAL` (opt-in, `=1`;
/// `gcg-g3e-proposal-post-resurrection-closure-scans-only-what-the-round-marked-20261001`,
/// rank P3/cl). After a NON-idle finalizer round the post-close
/// (gcf f1/c) re-read every walked header and every pre-round edge to re-learn
/// what the last pre-close proved (`n == 0`: every object marked then has every
/// walked referent marked). What it can still find is a referent of an object
/// marked SINCE: each was pushed (every push site marks before it pushes, and
/// `OldGenFinalizerPass::seed`'s own closure is skipped under the idle switch,
/// which this one requires), so each was popped in the round. Armed, the
/// post-close calls `OldGen::close_live_set_from` over those pops only; a
/// repeat (it rescued) over the pops since the previous call. Same marks, same
/// rescue set; the escape verdict is the pre-close's ORed with every
/// incremental call's (an escape depends only on the referrer being marked).
/// Read once per true-root major.
pub fn old_post_close_incremental_enabled() -> bool {
    false
}

/// gcj j1/og — `CRATONVM_GC_OLD_MARK_LOADER_SNAPSHOT` (opt-in, `=1` arms: the
/// orchestrator returned it from default ON after the j1c rows, which showed
/// no mark gain; gcj j3/og3 corrected this doc, which still said default ON):
/// the old mark's instance -> loader edge from one `loader_pin` snapshot per
/// collection instead of a registry read lock and probe per marked object (see
/// `old_mark_loader_rows` in `old_gen_gc_inner`). Read once per old-gen
/// collection.
pub fn old_mark_loader_snapshot_enabled() -> bool {
    false
}





/// gcj j3/og3 (B3 of `docs/feature-designs/generative-gc-architecture.md`)
/// — `CRATONVM_GEN_TENURED_POOL_MAX_FIXED` (opt-in, `=1` arms): `Tenured
/// Gen`'s pool `max` is the generation's growth limit (`OldGen::growth_limit`,
/// HotSpot Serial's fixed tenured maximum) instead of its current capacity.
/// Read once per pool snapshot (`jmx_memory_pools`).
pub fn serial_pool_maxima(
    total_bytes: usize,
    young: &cratonvm_gc_core::gen_young_sizing::YoungGenSizing,
    survivor_ratio: usize,
) -> (usize, usize) {
    let serial_young = serial_young_gen_bytes(total_bytes, young) & !(SERIAL_SPACE_ALIGNMENT - 1);
    let eden = serial_eden_bytes_at(serial_young, survivor_ratio);
    let copy_reserve = serial_young.saturating_sub(eden) / 2;
    (
        total_bytes.saturating_sub(copy_reserve),
        total_bytes.saturating_sub(serial_young),
    )
}



/// gch h4/pc — the escape verdict the post-resurrection closure takes from the
/// last pre-close without running (`Some`), or `None`: run it. Only when the
/// switch is `armed`, the finalizer round is `draining`, the post-close is not
/// `done`, nothing was popped in the round (`pops == 0`, so no mark was made
/// since that pre-close) and the pre-close has a verdict.
pub fn idle_post_close_verdict(
    armed: bool,
    draining: bool,
    done: bool,
    pops: usize,
    pre: Option<bool>,
) -> Option<bool> {
    if armed && draining && !done && pops == 0 {
        pre
    } else {
        None
    }
}

/// gcd d5/r: count a requested major that kept the legacy young seed
/// (`oldsz_true_root_fallbacks`) and say why, rate-limited per generation (the
/// first 8, then powers of two). `detail` is the fallback's size: promotion
/// destinations, or unwalked old-gen bytes. The `[GC] oldgen_sizing:` counter
/// alone could not tell the orchestrator which fallback a failing run took
/// (`GenR4W4NativeStringOomProbe` under `CRATONVM_GC_OVERHEAD_PROGRESS=0`).
/// gcd d9/a: counted per reason too (`oldsz_true_root_fb_<reason>`).
/// gch h4/pc (h3/mj review item 4): since `..._ALL_MAJORS` (default ON) and P-C
/// an occupancy major takes a fallback too, so the line no longer says
/// "requested"; a grep for `kept the legacy young seed` matches both texts.
pub fn note_true_root_fallback(
    old_gen: &mut OldGen,
    reason: crate::old_gen::TrueRootFallback,
    detail: usize,
) {
    let n = old_gen.note_true_root_fallback(reason);
    if n <= 8 || n.is_power_of_two() {
        tracing::info!(
            target: "cratonvm::gc",
            why = reason.why(),
            reason = reason.label(),
            detail,
            fallbacks = n,
            "major kept the legacy young seed (CRATONVM_GC_FULL_GC_TRUE_ROOTS)",
        );
    }
}

/// gcd d9/a — what `sweep_old_gen_non_moving_impl` hands the one
/// `old_gen_gc_inner` call of a true-root major (was a bare `bool`).
pub struct TrueRootAsk {
    /// The index of the first entry of the root slice that
    /// `sweep_old_gen_non_moving_impl` APPENDED (the promotion destinations
    /// and their source-keyed pin values). With the true-root seed on, the
    /// root loop and the census skip `roots[promo_tail..]`: the seed resolves
    /// the promotions instead. On a fallback inside `old_gen_gc_inner` they
    /// are roots exactly as before.
    pub promo_tail: usize,
    /// `(source, destination)` of every promotion into old gen this pause,
    /// ascending by source. Empty on a non-promoting cycle.
    pub promotions: Vec<(usize, usize)>,
    /// `CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE`, read once by the caller: may a
    /// planned pinned compaction keep the true-root seed?
    pub wide: bool,
    /// gci i1/og (`gch-h4pc-old-mark-and-true-root-review-findings-RETIRED-UNFIXED-20261018`
    /// item 3): the moving cycle's ask (P-C). Its young grid is the post-swap
    /// from-space, while the watched set holds PRE-copy addresses, which all
    /// lie in the evacuated (reset) arena, outside `[lo, hi)`; the moving
    /// cycle maps them through `pointer_map` into the root slice instead
    /// (`moving_true_root_extra_seeds`). So `old_gen_gc_inner` neither seeds
    /// them nor judges them (`watched_left_out_by_trace` could only answer
    /// empty): both were lookups per watched address for nothing.
    pub pre_copy: bool,
    /// gci i3/og (`CRATONVM_GC_MOVING_MAJOR_WATCHED_DEAD`, opt-in; P-C asks
    /// only): the post-copy YOUNG addresses of the watched set (each watched
    /// pre-copy address mapped through the pause's `pointer_map`), ascending,
    /// deduplicated. The moving cycle then does not root them; after the
    /// trace's fixed point `old_gen_gc_inner` reports the ones the trace did
    /// not reach (`TrueRootMarkOut::watched_left_out_post`). Empty otherwise.
    pub watched_post_copy: Vec<usize>,
    /// gci i3/og (`gci-i2og-old-gen-and-p-c-review-findings-FIXED-20261013` item 6):
    /// `Some(snapshot)` when the caller already took this pause's
    /// `gc_quiescence::watched_referents_snapshot()` (the moving cycle's P-C
    /// seeds); `old_gen_gc_inner` then uses it instead of cloning the set a
    /// second time. Nothing publishes to the set during the pause, so the two
    /// are equal. `None`: take it there, as before.
    pub watched_snapshot: Option<Option<cratonvm_gc_core::gc_quiescence::WatchedSnapshot>>,
    /// gci i4/og (`gci-i3og-true-root-major-residual-costs-FIXED-20261012` item 2):
    /// `true` from `sweep_old_gen_non_moving_impl` under
    /// `CRATONVM_GC_TRUE_ROOT_BATCHED_VERDICT` (default ON since gci i4): when the seed runs,
    /// `old_gen_gc_inner` hands the dead-promotion verdict's list back
    /// (`TrueRootMarkOut::judged_by_dest`) instead of the caller building it
    /// before the call. `false` on every other ask. It is also BV's one
    /// reading for `old_gen_gc_inner` (the grid-index reached set and the
    /// deferred `promoted_by_dest`, `true_root_lean` there).
    pub hand_back_judged: bool,
    /// gci i5/og (`gch-h3mj-...-review-findings` item 6, the `metadata_pin`
    /// half; P-C asks only): this pause's `metadata_pin` rows RE-KEYED by the
    /// owner's post-copy address, each value mapped the same way
    /// (`moving_metadata_rows_rekeyed`). `old_gen_gc_inner` uses it as its
    /// `metadata_pins` for the whole call (true-root trace, BFS, gap seeds,
    /// and the legacy seed on a fallback) instead of the registry snapshot,
    /// whose keys are pre-copy addresses no post-swap owner matches; the
    /// moving cycle then roots no moved owner's metadata value. `None`: take
    /// the snapshot there, as before (every non-P-C ask, or no row).
    pub metadata_rows: Option<FxHashMap<usize, Vec<usize>>>,
    /// gci i8/pa (`gch-h3mj-...-review-findings` item 6, the `mirror_pin`
    /// third; P-C asks under `CRATONVM_GC_MOVING_MAJOR_LOADER_REKEY` or a
    /// licensed moving `System.gc()` only): this pause's `mirror_pin` rows
    /// re-keyed by the loader's post-copy address, mirrors mapped the same
    /// way. `old_gen_gc_inner` uses them for every mirror lookup of the call
    /// (BFS, true-root trace, gap seed, the legacy seed on a fallback) instead
    /// of the registry, whose keys and values are pre-copy. `None`: the
    /// registry, as before.
    pub mirror_rows: Option<FxHashMap<usize, Vec<usize>>>,
    /// gci i8/pa (the `loader_pin` third; same asks): `class id -> defining
    /// loader`, this heap's rows, each loader mapped to its post-copy address.
    /// Used for the true-root trace's `loaders` and the BFS's instance ->
    /// loader edge. `None`: the registry, as before.
    pub loader_rows: Option<FxHashMap<u32, usize>>,
    /// gci i8/pa: the pinned loaders this pause PROMOTED (destinations), which
    /// the moving cycle no longer roots under the re-key. Rooted only on a
    /// fallback inside the call: the legacy young seed looks a young
    /// instance's loader up in the registry's pre-copy rows and would miss a
    /// promoted one. Empty on every other ask.
    pub fallback_loaders: Vec<usize>,
}

/// gcd d9/a — what that `old_gen_gc_inner` call reports back.
#[derive(Default)]
pub struct TrueRootMarkOut {
    /// The true-root seed ran (no fallback inside the call).
    pub ran: bool,
    /// The promotion DESTINATIONS a pinned compaction dropped as dead,
    /// ascending (only a compaction that ran fills it; after the in-place
    /// sweep the caller asks the free list instead).
    pub dropped_dests: Vec<usize>,
    /// gci i3/og (`CRATONVM_GC_MOVING_MAJOR_WATCHED_DEAD`): the addresses of
    /// `TrueRootAsk::watched_post_copy` the trace did not reach, ascending
    /// (post-copy; the moving cycle maps them back to their watched sources).
    pub watched_left_out_post: Vec<usize>,
    /// gci i4/og (`TrueRootAsk::hand_back_judged` only): every promotion of
    /// the ask as `(destination, source)`, ascending by destination, filled
    /// only when the seed ran. Empty otherwise.
    pub judged_by_dest: Vec<(usize, usize)>,
    /// gci i4/og: the microseconds `judged_by_dest` took to build (the
    /// `list=` field of `[gcphase] old-gen tr-judge:`).
    pub judged_list_us: u64,
}

/// gcd d9/a — what `sweep_old_gen_non_moving_impl` reports to
/// `run_non_moving_young_cycle` after a true-root major.
#[derive(Default)]
pub struct TrueRootSweepOut {
    /// Promotion SOURCES whose destination the collection freed: the caller
    /// removes them from the pointer map it returns.
    pub dead_sources: Vec<usize>,
    /// gch h4/wr: the watched young survivors (pre-collection addresses) the
    /// true-root trace did not reach, under
    /// `CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD`. Written by
    /// `old_gen_gc_inner` itself (this slot is reset by
    /// `sweep_old_gen_non_moving_impl` before that call); the caller installs
    /// them (`GenerationalHeap::install_true_root_watched_left_out`).
    pub watched_left_out: Vec<usize>,
}

thread_local! {
    /// Set by `run_non_moving_young_cycle` around the old sweep of a
    /// REQUESTED major; taken by `sweep_old_gen_non_moving_impl`.
    /// gcf f2/c: a bit set ([`ARMED_TRUE_ROOTS`] | [`ARMED_REQUESTED`]) -- the
    /// sweep also needs "this is a requested major" for
    /// `CRATONVM_GC_REQUESTED_MAJOR_COMPACTS`, with the true-root seed off too.
    pub static TRUE_ROOT_MAJOR_ARMED: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
    /// Set by `sweep_old_gen_non_moving_impl` for the one `old_gen_gc_inner`
    /// call it makes; taken there. (gcd d9/a: the promotions ride along.)
    pub static TRUE_ROOT_MARK_THIS_CALL: std::cell::RefCell<Option<TrueRootAsk>> =
        const { std::cell::RefCell::new(None) };
    /// Written by that `old_gen_gc_inner` call, taken by its caller.
    pub static TRUE_ROOT_MARK_OUT: std::cell::RefCell<TrueRootMarkOut> =
        const {
            std::cell::RefCell::new(TrueRootMarkOut {
                ran: false,
                dropped_dests: Vec::new(),
                watched_left_out_post: Vec::new(),
                judged_by_dest: Vec::new(),
                judged_list_us: 0,
            })
        };
    /// Written by `sweep_old_gen_non_moving_impl`, taken by
    /// `run_non_moving_young_cycle` right after the call.
    pub static TRUE_ROOT_SWEEP_OUT: std::cell::RefCell<TrueRootSweepOut> =
        const {
            std::cell::RefCell::new(TrueRootSweepOut {
                dead_sources: Vec::new(),
                watched_left_out: Vec::new(),
            })
        };
}

/// gcf f2/c — [`TRUE_ROOT_MAJOR_ARMED`]'s bits: the true-root young seed is
/// armed, and the sweep is a REQUESTED major's.
pub const ARMED_TRUE_ROOTS: u8 = 1;
pub const ARMED_REQUESTED: u8 = 2;

/// gch h2/og (lane ob's X3, `gcg-g3x-xlog-gc-review-findings-FIXED-20261016` item
/// 5) — the split of the last stop-the-world old-generation collection this
/// thread ran (`old_gen_gc_inner`), for the Serial `Phase 1..4` lines of
/// `-Xlog:gc+phases` that the VM printed as the whole pause plus three
/// `0.000ms`. Stamped on every old collection (three clock reads); taken by
/// the VM with [`take_last_old_gc_phase_split`] on the collecting thread, the
/// same producer/consumer pairing as [`take_last_pause_phases`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OldGcPhaseSplit {
    /// `old_gen_gc_inner`'s entry to the end of its mark: the walk, the root
    /// seed, the young seed, the closure and the finalizer round.
    pub mark_ns: u64,
    /// The reclamation: the in-place sweep (closure, free loop, coalesce), or
    /// a compaction (`compact_with_walked_objects` /
    /// `compact_around_pins_watching` plan and slide in one call, so the two
    /// are not separable here).
    pub reclaim_ns: u64,
    /// After a compaction: the young-reference and root fix-up. 0 in place.
    pub fixup_ns: u64,
    /// When the collection ended. A reader compares it with its own pause
    /// start, so a split left by an earlier collection is never printed.
    pub ended_at: std::time::Instant,
}

impl OldGcPhaseSplit {
    /// The four Serial phases of a full collection of `total_ns`: Phase 2
    /// ("Compute new object addresses") carries the reclamation, Phase 3
    /// ("Adjust pointers") the fix-up, Phase 4 ("Move objects") 0 (the slide is
    /// inside the reclamation), and Phase 1 ("Mark live objects") the rest of
    /// the pause (the young half, the root gathering, reference processing and
    /// the mark). Each clamped so the four sum to `total_ns`.
    pub fn serial_phases_ns(&self, total_ns: u64) -> [u64; 4] {
        let p2 = self.reclaim_ns.min(total_ns);
        let p3 = self.fixup_ns.min(total_ns - p2);
        [total_ns - p2 - p3, p2, p3, 0]
    }
}

thread_local! {
    /// See [`OldGcPhaseSplit`]. Written by `old_gen_gc_inner`, taken by the VM.
    pub static LAST_OLD_GC_PHASE_SPLIT: std::cell::Cell<Option<OldGcPhaseSplit>> =
        const { std::cell::Cell::new(None) };
}

/// Take (and clear) this thread's last [`OldGcPhaseSplit`].
pub fn take_last_old_gc_phase_split() -> Option<OldGcPhaseSplit> {
    LAST_OLD_GC_PHASE_SPLIT.with(|c| c.take())
}

/// `old_gen_gc_inner`'s three stamps (entry, mark end, reclaim end) and now.
pub fn store_old_gc_phase_split(
    t_entry: std::time::Instant,
    t_mark: std::time::Instant,
    t_reclaim: std::time::Instant,
) {
    let now = std::time::Instant::now();
    let ns = |a: std::time::Instant, b: std::time::Instant| {
        u64::try_from(b.saturating_duration_since(a).as_nanos()).unwrap_or(u64::MAX)
    };
    LAST_OLD_GC_PHASE_SPLIT.with(|c| {
        c.set(Some(OldGcPhaseSplit {
            mark_ns: ns(t_entry, t_mark),
            reclaim_ns: ns(t_mark, t_reclaim),
            fixup_ns: ns(t_reclaim, now),
            ended_at: now,
        }))
    });
}

/// Arms [`TRUE_ROOT_MAJOR_ARMED`] for one old sweep; always disarms on drop.
pub struct TrueRootMajorScope;

impl TrueRootMajorScope {
    /// A requested major with the true-root seed (`on`), or nothing. Tests
    /// only since gcf f2/c (production arms through [`Self::arm_requested`]).
    #[cfg(test)]
    pub fn arm(on: bool) -> Self {
        Self::arm_requested(on, on)
    }

    /// gcf f2/c — `requested`: the sweep is a requested major's;
    /// `true_roots`: and its young seed is the true-root one (only ever armed
    /// for a requested major). Tests only since gcf f4/c (production arms
    /// through [`Self::arm_major`]).
    #[cfg(test)]
    pub fn arm_requested(requested: bool, true_roots: bool) -> Self {
        Self::arm_major(requested, true_roots, false)
    }

    /// gcf f4/c — [`Self::arm_requested`] plus
    /// `CRATONVM_GC_FULL_GC_TRUE_ROOTS_ALL_MAJORS` (`all_majors`, opt-in): the
    /// true-root seed is armed for an OCCUPANCY major too
    /// (`gcd-d9a-proposal-true-root-seed-for-every-stw-major`, item 1). The
    /// requested bit stays the request's alone, so nothing keyed on it
    /// (`CRATONVM_GC_REQUESTED_MAJOR_COMPACTS`) widens.
    pub fn arm_major(requested: bool, true_roots: bool, all_majors: bool) -> Self {
        let bits = true_root_major_bits(requested, true_roots, all_majors);
        TRUE_ROOT_MAJOR_ARMED.with(|c| c.set(bits));
        Self
    }
}

/// gcf f4/c — the bits [`TrueRootMajorScope::arm_major`] arms: `ARMED_REQUESTED`
/// iff `requested`; `ARMED_TRUE_ROOTS` iff the true-root seed is on
/// (`CRATONVM_GC_FULL_GC_TRUE_ROOTS`, `true_roots`) and the major is requested
/// or `all_majors` widens it to every non-moving major.
pub fn true_root_major_bits(requested: bool, true_roots: bool, all_majors: bool) -> u8 {
    let mut bits = 0u8;
    if requested {
        bits |= ARMED_REQUESTED;
    }
    if true_roots && (requested || all_majors) {
        bits |= ARMED_TRUE_ROOTS;
    }
    bits
}



/// gch h4/wr — `CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD` (default ON since
/// gci i1/og and jr20 wave 9 (orchestrator-authorised flip), `=0` kills and
/// restores the d5/r seed; vh4-wr A, D and E passed on h4a/h4b;
/// `gch-h3jx-gen-true-root-major-keeps-unstarted-compat-threads-one-cycle-FIXED-20261012`).
///
/// Off (gcd d5/r, `=0`): the non-moving true-root major seeds its young trace with
/// the whole watched set (every address a weak table hands back after the
/// pause), so a watched young survivor the young phase kept only for a dead
/// old holder's card keeps its old referents -- and is handed back by its weak
/// reference, one collection longer than HotSpot (`UnstartedThreadItlProbe`
/// `gcCalls=2` on Generational `--compatible`, HotSpot 1).
///
/// On: the watched set is not seeded; after the trace's fixed point each
/// watched young survivor it did not reach is judged dead
/// ([`watched_left_out_by_trace`]), its identity `pointer_map` entry is
/// removed, and `GenerationalHeap::true_root_watched_left_out` answers the
/// post-collection survival questions with "did not survive". Its weak
/// references are cleared and enqueued, a JNI weak global to it is cleared,
/// and a pooled String drops its entry, exactly as HotSpot's full collection
/// does, so nothing can hand it back and its old referents may be freed (the
/// d5/r use-after-free stays closed). The moving true-root major (P-C) seeds
/// the watched set through its own root tail (`moving_true_root_extra_seeds`)
/// unless `CRATONVM_GC_MOVING_MAJOR_WATCHED_DEAD` (gci i3/og; default ON since the i7 flip) gives it
/// this verdict too; that switch reads this one as well. Read once per
/// true-root major.
pub fn full_gc_true_roots_watched_dead_enabled() -> bool {
    true
}





impl Drop for TrueRootMajorScope {
    fn drop(&mut self) {
        let _ = TRUE_ROOT_MAJOR_ARMED.try_with(|c| c.set(0));
    }
}

/// gcf f2/c — `CRATONVM_GC_REQUESTED_MAJOR_COMPACTS` (default ON since WARNING
/// gci-user-flip-20261013, `runtime_flag_default_on`, `=0` kills; was opt-in):
/// a REQUESTED major on the non-moving path (`System.gc()`, the allocation
/// ladder's major request) compacts the old generation
/// (`OldGen::compact_around_pins`) whenever the pause has no conservative root
/// to pin -- the same gate under which a refused humongous allocation already
/// compacts by default (gen r5w1/oldgen5, `pins_nothing_conservative`). HotSpot
/// Serial's full collection is always a sliding mark-compact; without this the
/// default compacts only once an allocation has already failed
/// (`gcf-f1c-stw-full-gc-never-compacts-old-gen-by-default-20261001`). Read
/// per requested major (the declared-flag snapshot; a per-thread test override
/// reaches it).
pub fn requested_major_compacts_enabled() -> bool {
    // WARNING gci-user-flip-20261013: default flipped ON by the user's decision, pending apps triage.
    true
}

/// gcf f2/c — does this non-moving old sweep plan a compaction because it is
/// a REQUESTED major (`CRATONVM_GC_REQUESTED_MAJOR_COMPACTS`)? Not when a
/// fragmentation request already plans one (counted once), not for an
/// unrequested major, and not when the true-root seed is armed narrow
/// (`CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE=0`: a planned compaction would put
/// the major back on the legacy seed). `switch_on` is read last, and only
/// then, so an unrequested major pays no flag lookup.
pub fn requested_major_compaction_wanted(
    frag_requested: bool,
    requested_major: bool,
    true_root_armed: bool,
    true_root_wide: bool,
    switch_on: impl FnOnce() -> bool,
) -> bool {
    !frag_requested && requested_major && (!true_root_armed || true_root_wide) && switch_on()
}

/// Proposal `gcj-j3og3-proposal-fragmented-in-place-majors-compact-through-the-pinned-compactor`:
/// allow at most one PROACTIVE fragmentation-triggered pinned slide per four
/// old-gen collections, and only when conservative pins cover strictly less
/// than ten percent of marked old live bytes. A repair for a currently refused
/// allocation bypasses that cadence: it is the OOM ladder's exact blocked
/// request, revalidated against the current free list, and still passes the
/// pin-budget and compactor safety checks below.
pub fn fragmented_pinned_compaction_cadence_due(collections: u64, last_attempt: Option<u64>) -> bool {
    last_attempt.is_none_or(|last| collections.wrapping_sub(last) >= 4)
}

/// A pending bit alone is insufficient: an exact blocked request must still
/// fit the current total free space and lack a contiguous fit. Proactive
/// repairs use their separate margin over the historical request scale.
pub fn fragmented_pinned_compaction_due(
    repair_due: bool,
    reactive_due: bool,
    collections: u64,
    last_attempt: Option<u64>,
) -> bool {
    reactive_due
        || (repair_due && fragmented_pinned_compaction_cadence_due(collections, last_attempt))
}

pub fn fragmented_pinned_compaction_pin_budget_ok(pinned_bytes: usize, live_bytes: usize) -> bool {
    live_bytes > 0 && pinned_bytes.saturating_mul(10) < live_bytes
}


/// What the true-root young trace reads besides its own state. Built per call
/// (the old-gen borrow must not outlive one).
///
/// gcg g3/e: generic over the old mark's admission oracle ([`OldMarkGrid`]):
/// the walked grid (`Vec<(*mut u8, usize)>`, every walked collection, the
/// same expressions as before) or the O(live) collection's block-offset
/// oracle (`live_sweep::LiveOracle`, `CRATONVM_GC_OLD_LIVE_TRUE_ROOTS`; that
/// collection was deleted by gcg g5/og, the generic stays).
pub struct TrueRootCtx<'a, G: OldMarkGrid + ?Sized> {
    pub old_gen: &'a OldGen,
    pub walked: &'a G,
    pub mirror_pin_owners: &'a FxHashSet<usize>,
    pub metadata_pins: Option<&'a FxHashMap<usize, Vec<usize>>>,
}

/// gcd d9/a: `destination -> index into promoted` (`TrueRootYoung::promoted_by_dest`).
/// gci i4/og: a free function so a lean ask can build it after the pin
/// snapshots, only when a source-keyed row exists.
pub fn promoted_by_dest_of(promoted: &[(usize, usize, usize)]) -> FxHashMap<usize, usize> {
    promoted
        .iter()
        .enumerate()
        .map(|(i, &(_, dest, _))| (dest, i))
        .collect()
}

/// The young half of a true-root major's mark (see the section comment).
pub struct TrueRootYoung {
    /// Every young object the walk parsed, `(base, size)`, ascending.
    pub grid: Vec<(usize, usize)>,
    /// Unparseable stretches `[lo, hi)` (absolute): treated as roots.
    pub stretches: Vec<(usize, usize)>,
    /// The young range `[lo, hi)` the grid covers.
    pub lo: usize,
    pub hi: usize,
    /// `class id -> defining loader`, this heap's rows (`loader_pin`), or
    /// `None` when loader pinning is off / the registry is empty.
    /// (gci i8/pa: replaced by the ask's re-keyed rows when it carries them.)
    pub loaders: Option<FxHashMap<u32, usize>>,
    /// gci i8/pa: the ask's re-keyed `mirror_pin` rows
    /// (`TrueRootAsk::mirror_rows`), set by `old_gen_gc_inner` after `build`;
    /// `None`: the registry, as before.
    pub mirror_rows: Option<FxHashMap<usize, Vec<usize>>>,
    /// The grid objects the trace reached (gci i2/og: by grid index on a P-C
    /// major, see [`TrueRootVisited`]).
    pub visited: TrueRootVisited,
    pub pending_young: Vec<usize>,
    /// Marked old objects not yet scanned for young targets. (gch h4/pc: under
    /// `CRATONVM_GC_TRUE_ROOT_ROUNDS_IN_BFS` only the ones the BFS's scan
    /// rejected; the BFS seeds every other one's targets itself.)
    pub pending_old: Vec<usize>,
    /// gcd d9/a: this pause's promotions `(source, destination, size)`,
    /// ascending by source; `size` is the destination's walked size.
    pub promoted: Vec<(usize, usize, usize)>,
    /// `destination -> index into promoted`, for the source-keyed side tables
    /// of a marked destination (`round`).
    pub promoted_by_dest: FxHashMap<usize, usize>,
    /// Per `promoted` entry: already resolved by a root or an edge.
    pub promoted_seen: Vec<bool>,
    /// Resolved entries whose destination is still to be pushed to the old
    /// mark (`drain`).
    pub pending_promoted: Vec<usize>,
    /// gcj j4/rf (`CRATONVM_GEN_FIN_ROUND_RECORDS_YOUNG`): `Some` while the
    /// finalizer resurrection round drains -- every grid object the trace
    /// reaches for the first time from then on (so the strong closure had not)
    /// is recorded, for the depth-2 set
    /// (`OldGenFinalizerPass::closure`).
    pub reached_in_round: Option<Vec<usize>>,
    /// t1 G2 (`CRATONVM_GC_OLD_PAR_MARK`): `Some` while the parallel old mark
    /// is armed and outside the finalizer round -- every OLD target this
    /// trace would hand to `mark_and_push_old_gen_in` (`follow`, `drain`'s
    /// promotion destinations) is collected here unmarked instead, and the
    /// old mark's parallel drain marks them on its workers (or the driver
    /// flushes them serially when there are few). Same screen, same marks.
    pub defer_old: Option<Vec<usize>>,
}

impl TrueRootYoung {
    /// Walk young from-space with `mark_young_to_old_refs`' grid rules.
    ///
    /// gcd d9/a: `promoted` is this pause's promotions (see the field).
    /// gci i2/og: `visited_by_index` -- the reached set is a bitset over the
    /// grid's indices instead of a hash set of bases (`TrueRootVisited`); the
    /// P-C ask (`TrueRootAsk::pre_copy`) only, for now.
    /// gci i4/og: `by_dest` -- build `promoted_by_dest` here (the default
    /// asks). `false` leaves it empty: the caller builds it with
    /// `promoted_by_dest_of` once it knows a source-keyed row exists
    /// (`gci-i3og-true-root-major-residual-costs-FIXED-20261012` item 1).
    pub fn build(
        young_from: &Arena,
        young_skips: &[(usize, usize)],
        old_gen: &OldGen,
        promoted: Vec<(usize, usize, usize)>,
        visited_by_index: bool,
        by_dest: bool,
    ) -> Self {
        let free_blocks: Vec<(usize, usize)> = if young_skips.is_empty() {
            young_from.free_blocks_sorted()
        } else {
            let mut v = young_from.free_blocks_sorted();
            v.extend_from_slice(young_skips);
            v.sort_by_key(|&(off, _)| off);
            v
        };
        let mut free_iter = free_blocks.iter().peekable();
        let mut cursor: usize = 0;
        let base = young_from.base_ptr() as usize;
        let used = young_from.used();
        let rules = GridRules {
            bad_gap_filler_is_anomaly: false,
            ..GridRules::unmarked(base, used)
        };
        let mut no_phantom_probe = 0usize;
        let mut grid: Vec<(usize, usize)> = Vec::new();
        let mut stretches: Vec<(usize, usize)> = Vec::new();
        while cursor < used {
            if skip_free_blocks(&mut cursor, &mut free_iter).0 {
                continue;
            }
            let next_free = free_iter.peek().map(|&&(off, _)| off);
            // `cursor < used` inside mapped from-space (`probe_grid_at`'s contract).
            match probe_grid_at(cursor, next_free, &rules, &mut no_phantom_probe) {
                GridProbe::Gap { len } => cursor += len,
                GridProbe::EmptyRun { resume } => cursor = resume,
                GridProbe::ZeroSpan { .. }
                | GridProbe::BadSize { .. }
                | GridProbe::BadGapFiller => {
                    let stretch_lo = cursor;
                    let resynced = resync_to_next_free_block(&mut cursor, &mut free_iter);
                    let stretch_hi = if resynced { cursor } else { used };
                    if stretch_hi > stretch_lo {
                        stretches.push((base + stretch_lo, base + stretch_hi));
                    }
                    if !resynced {
                        break;
                    }
                }
                GridProbe::Object { size } => {
                    grid.push((base + cursor, size));
                    cursor += size;
                }
                GridProbe::CrossesFreeBlock { size, .. } | GridProbe::Phantom { size, .. } => {
                    grid.push((base + cursor, size));
                    cursor += size;
                }
            }
        }
        let (lo, hi) = (base, base + used);
        let loaders = if cratonvm_types::loader_pin::loader_pinning_enabled() {
            cratonvm_types::loader_pin::snapshot_where(|a| {
                old_gen.contains(a as *const u8) || (a >= lo && a < hi)
            })
        } else {
            None
        };
        debug_assert!(
            promoted.windows(2).all(|w| w[0].0 < w[1].0),
            "TrueRootYoung::build: promotions must be ascending by source",
        );
        let promoted_by_dest: FxHashMap<usize, usize> = if by_dest {
            promoted_by_dest_of(&promoted)
        } else {
            FxHashMap::default()
        };
        let promoted_seen = vec![false; promoted.len()];
        let visited = if visited_by_index {
            TrueRootVisited::by_index(grid.len())
        } else {
            TrueRootVisited::default()
        };
        Self {
            grid,
            stretches,
            lo,
            hi,
            loaders,
            mirror_rows: None,
            visited,
            pending_young: Vec::new(),
            pending_old: Vec::new(),
            promoted,
            promoted_by_dest,
            promoted_seen,
            pending_promoted: Vec::new(),
            reached_in_round: None,
            defer_old: None,
        }
    }

    /// The index in `grid` of the parsed young object `addr` lies in (base
    /// inclusive, end exclusive). (gci i2/og: was `base_of`, which answered
    /// the base; every caller now wants the index, `grid[idx].0` is the base.)
    pub fn index_of(&self, addr: usize) -> Option<usize> {
        if addr < self.lo || addr >= self.hi {
            return None;
        }
        let idx = self
            .grid
            .partition_point(|&(b, _)| b <= addr)
            .checked_sub(1)?;
        let (b, s) = self.grid[idx];
        (addr < b.saturating_add(s)).then_some(idx)
    }

    /// Has the trace reached the grid object at `idx`?
    pub fn reached(&self, idx: usize) -> bool {
        self.visited.contains(idx, self.grid[idx].0)
    }

    /// gcd d9/a: the promotion whose SOURCE extent `addr` lies in (the index
    /// into `promoted`). The source bytes are zeroed on the young free list
    /// by now, so the grid never holds them.
    pub fn promoted_index(&self, addr: usize) -> Option<usize> {
        if self.promoted.is_empty() || addr < self.lo || addr >= self.hi {
            return None;
        }
        let idx = self
            .promoted
            .partition_point(|&(src, _, _)| src <= addr)
            .checked_sub(1)?;
        let (src, _, size) = self.promoted[idx];
        (addr < src.saturating_add(size)).then_some(idx)
    }

    /// Seed the grid object at `idx` (once).
    pub fn seed_index(&mut self, idx: usize) {
        let base = self.grid[idx].0;
        if self.visited.insert(idx, base) {
            self.pending_young.push(base);
            if let Some(r) = self.reached_in_round.as_mut() {
                r.push(base);
            }
        }
    }

    /// A root word (precise or conservative): the young object it lies in, or
    /// (gcd d9/a) the destination of the promoted object whose source it
    /// names, which `drain` pushes to the old mark. Asked FIRST: a promoted
    /// source is never a grid object, and a forwarded header must not be
    /// traced as one.
    pub fn seed_word(&mut self, w: usize) {
        if let Some(i) = self.promoted_index(w) {
            if !self.promoted_seen[i] {
                self.promoted_seen[i] = true;
                self.pending_promoted.push(i);
            }
            return;
        }
        if let Some(idx) = self.index_of(w) {
            self.seed_index(idx);
        }
    }

    /// gcd d9/a: the values of the side-table rows keyed by a promoted
    /// owner's SOURCE address (`mirror_pin`, `metadata_pin`: rebuilt by the VM
    /// from pre-collection addresses, and remapped only after the collector
    /// returns). `external_roots` rows need nothing here: they were remapped
    /// at the young phase's commit (`run_non_moving_young_cycle`).
    pub fn source_keyed_targets<G: OldMarkGrid + ?Sized>(
        src: usize,
        ctx: &TrueRootCtx<'_, G>,
        out: &mut Vec<usize>,
    ) {
        if ctx.mirror_pin_owners.contains(&src) {
            if let Some(mirrors) = cratonvm_types::mirror_pin::mirrors_for_loader(src) {
                out.extend(mirrors);
            }
        }
        if let Some(values) = ctx.metadata_pins.and_then(|m| m.get(&src)) {
            out.extend(values.iter().copied());
        }
    }

    /// Resolved promotions this trace reached.
    pub fn promoted_resolved(&self) -> usize {
        self.promoted_seen.iter().filter(|&&s| s).count()
    }

    /// An edge's target: a young one seeds its object, anything else goes to
    /// the old mark (which screens it as every push site does).
    pub fn follow<G: OldMarkGrid + ?Sized>(
        &mut self,
        target: usize,
        ctx: &TrueRootCtx<'_, G>,
        worklist: &mut Vec<*mut u8>,
        site: &'static str,
    ) {
        if target == 0 {
            return;
        }
        if target >= self.lo && target < self.hi {
            self.seed_word(target);
        } else if let Some(d) = self.defer_old.as_mut() {
            // t1 G2: marked by the parallel old mark (see the field).
            d.push(target);
        } else {
            // gcg g3/e: `mark_and_push_old_gen` is exactly this over the
            // walked grid.
            mark_and_push_old_gen_in(target as *mut u8, ctx.old_gen, ctx.walked, worklist, site);
        }
    }

    /// The side-table edges of `owner` (a young object reached, or a marked
    /// old one), appended to `out`: its overlays, its class's defining loader,
    /// and -- when it is a pinned owner -- its mirrors and metadata values.
    pub fn side_table_targets<G: OldMarkGrid + ?Sized>(
        &self,
        owner: usize,
        ctx: &TrueRootCtx<'_, G>,
        out: &mut Vec<usize>,
    ) {
        // SAFETY: `owner` is a parsed young object base or a marked old-gen
        // object base, read inside the pause.
        let class_id = unsafe { &*(owner as *const ObjectHeader) }
            .class_id
            .as_u32();
        cratonvm_gc_core::external_roots::with_external_roots_for_owner(owner, Some(class_id), |refs| {
            out.extend(refs.iter().map(|r| r.as_ptr() as usize));
        });
        if let Some(&loader) = self.loaders.as_ref().and_then(|m| m.get(&class_id)) {
            out.push(loader);
        }
        if ctx.mirror_pin_owners.contains(&owner) {
            // gci i8/pa: the re-keyed rows when the ask carried them.
            if let Some(mirrors) = pinned_mirrors_of(owner, self.mirror_rows.as_ref()) {
                out.extend(mirrors);
            }
        }
        if let Some(values) = ctx.metadata_pins.and_then(|m| m.get(&owner)) {
            out.extend(values.iter().copied());
        }
    }

    /// Everything an unparseable stretch could hold, as roots: every word
    /// (8-byte and narrow halves), every published header's loader, and every
    /// side-table edge of an owner inside it.
    ///
    /// gcd d9/a: with a pinned compaction planned (`pins`), every old-gen
    /// object a stretch WORD names (an exact base, or a word strictly inside
    /// one) is also pinned and marked, exactly as `mark_young_to_old_refs`'
    /// conservative fallback does for the legacy seed: `fixup_young_old_refs`
    /// walks the same stretch the same way and can rewrite only a word equal
    /// to a moved base. The caller tags the pushed entries
    /// `OLD_PIN_SRC_YOUNG_STRETCH`.
    pub fn seed_stretches<G: OldMarkGrid + ?Sized>(
        &mut self,
        ctx: &TrueRootCtx<'_, G>,
        worklist: &mut Vec<*mut u8>,
        mut pins: Option<&mut Vec<usize>>,
    ) {
        if self.stretches.is_empty() {
            return;
        }
        let stretches = self.stretches.clone();
        let mut words: Vec<usize> = Vec::new();
        for &(lo, hi) in &stretches {
            let scanned_from = words.len();
            // SAFETY: `[lo, hi)` lies inside mapped young from-space (the
            // walk's own stretch), read inside the pause.
            unsafe {
                for_each_conservative_ref_slot(
                    self.lo,
                    lo - self.lo,
                    hi - self.lo,
                    active_narrow_geometry(),
                    |_slot, _width, value| words.push(value),
                );
            }
            if let Some(pins) = pins.as_deref_mut() {
                for &w in &words[scanned_from..] {
                    // gcg g3/e: the grid's `has_base` / `interior_base` are
                    // exactly `walked_grid_has_base` / `old_gen_interior_root_base`.
                    if ctx.walked.has_base(w) {
                        pins.push(w);
                    } else if let Some((base, _)) = ctx.walked.interior_base(w) {
                        pins.push(base);
                        // The pin keeps it either way; marking it also traces
                        // its side-table edges (gcd d1/d's reason, in
                        // `mark_young_to_old_refs`).
                        mark_and_push_old_gen_in(
                            base as *mut u8,
                            ctx.old_gen,
                            ctx.walked,
                            worklist,
                            "true-root young stretch (pinned interior)",
                        );
                    }
                }
            }
            if let Some(loaders) = self.loaders.as_ref() {
                let word = cratonvm_types::HEADER_WORD_SIZE;
                let mut at = (lo + (word - 1)) & !(word - 1);
                while at + word <= hi {
                    // SAFETY: inside the stretch, mapped; only the header word
                    // (`class_id` u32 then the mark word u32) is read.
                    let (class_id, mark) = unsafe {
                        let p = at as *const u32;
                        (p.read(), p.add(1).read())
                    };
                    if ObjectHeader::gc_flags_of(mark) & GC_FLAG_HEADER != 0 {
                        if let Some(&loader) = loaders.get(&class_id) {
                            words.push(loader);
                        }
                    }
                    at += word;
                }
            }
        }
        let in_stretch = |a: usize| stretches.iter().any(|&(lo, hi)| a >= lo && a < hi);
        for overlay in
            cratonvm_gc_core::external_roots::external_roots_for_matching_owners(&|owner| in_stretch(owner))
        {
            words.push(overlay.as_ptr() as usize);
        }
        // gci i8/pa: the re-keyed rows' owners when the ask carried them.
        match self.mirror_rows.as_ref() {
            Some(rows) => {
                for (&owner, mirrors) in rows {
                    if in_stretch(owner) {
                        words.extend(mirrors.iter().copied());
                    }
                }
            }
            None => {
                for owner in cratonvm_types::mirror_pin::pinned_owner_addrs() {
                    if in_stretch(owner) {
                        if let Some(mirrors) = cratonvm_types::mirror_pin::mirrors_for_loader(owner)
                        {
                            words.extend(mirrors);
                        }
                    }
                }
            }
        }
        if let Some(pins) = ctx.metadata_pins {
            for (&owner, values) in pins {
                if in_stretch(owner) {
                    words.extend(values.iter().copied());
                }
            }
        }
        for w in words {
            self.follow(w, ctx, worklist, "true-root young stretch");
        }
    }

    /// Trace every pending young object: its fields and side-table edges.
    /// gcd d9/a: and push every newly resolved promotion's destination to the
    /// old mark (its own edges are the BFS's and `round`'s business, as for
    /// any marked old object).
    pub fn drain<G: OldMarkGrid + ?Sized>(
        &mut self,
        ctx: &TrueRootCtx<'_, G>,
        worklist: &mut Vec<*mut u8>,
    ) {
        let mut targets: Vec<usize> = Vec::new();
        loop {
            while let Some(i) = self.pending_promoted.pop() {
                let dest = self.promoted[i].1;
                if let Some(d) = self.defer_old.as_mut() {
                    // t1 G2: marked by the parallel old mark (see the field).
                    d.push(dest);
                    continue;
                }
                mark_and_push_old_gen_in(
                    dest as *mut u8,
                    ctx.old_gen,
                    ctx.walked,
                    worklist,
                    "true-root promotion destination",
                );
            }
            let Some(obj) = self.pending_young.pop() else {
                break;
            };
            targets.clear();
            // SAFETY: `obj` is a young object base the grid walk parsed; the
            // world is stopped.
            let header = unsafe { &*(obj as *const ObjectHeader) };
            // SAFETY: as above; the walk sized the object.
            unsafe {
                for_each_ref_slot(obj as *mut u8, header, |r, _| targets.push(r as usize));
            }
            self.side_table_targets(obj, ctx, &mut targets);
            for &t in &targets {
                self.follow(t, ctx, worklist, "true-root young edge");
            }
        }
    }

    /// gch h4/pc (`CRATONVM_GC_TRUE_ROOT_ROUNDS_IN_BFS`) — the per-object half
    /// of [`Self::round`], done by the old mark's BFS for the object `obj` it
    /// just popped and scanned. `young_targets` are the young-range values the
    /// BFS met for it: its slots (the scan), its overlays, its class's young
    /// loader, its mirror and metadata pins -- exactly the values `round`
    /// would have kept from its own slot scan and `side_table_targets` (whose
    /// old values are the BFS's business on both arms). Each is seeded now,
    /// not at the next `round`; a promotion destination's source-keyed rows
    /// are followed now too. Nothing is pushed to `pending_old`, so `round`
    /// only drains what was seeded. The set of young objects reached by the
    /// time a round's drain finishes is the same: seeding is idempotent
    /// (`visited`), and both arms seed the same values before the same drain
    /// (the one at the worklist's next empty point), so the phases of the
    /// mark (strong closure, finalizer round) see the same marks.
    pub fn seed_from_bfs<G: OldMarkGrid + ?Sized>(
        &mut self,
        obj: usize,
        young_targets: &[usize],
        ctx: &TrueRootCtx<'_, G>,
        worklist: &mut Vec<*mut u8>,
    ) {
        for &t in young_targets {
            self.seed_word(t);
        }
        if self.promoted_by_dest.is_empty() {
            return;
        }
        if let Some(&i) = self.promoted_by_dest.get(&obj) {
            let mut source_keyed: Vec<usize> = Vec::new();
            Self::source_keyed_targets(self.promoted[i].0, ctx, &mut source_keyed);
            for t in source_keyed {
                self.follow(
                    t,
                    ctx,
                    worklist,
                    "true-root promoted owner's source-keyed pin",
                );
            }
        }
    }

    /// The fixed-point step, run whenever the old mark's worklist is empty:
    /// the young objects the marked old objects name (their fields and
    /// side-table edges) are seeded and traced, which may push more old ones.
    pub fn round<G: OldMarkGrid + ?Sized>(
        &mut self,
        ctx: &TrueRootCtx<'_, G>,
        worklist: &mut Vec<*mut u8>,
    ) {
        let pending = std::mem::take(&mut self.pending_old);
        let mut targets: Vec<usize> = Vec::new();
        // gcd d9/a: the rows keyed by a marked promotion destination's SOURCE
        // (old values included: the BFS looks them up by the destination and
        // finds nothing).
        let mut source_keyed: Vec<usize> = Vec::new();
        for obj in pending {
            if let Some(&i) = self.promoted_by_dest.get(&obj) {
                Self::source_keyed_targets(self.promoted[i].0, ctx, &mut source_keyed);
            }
            let (lo, hi) = (self.lo, self.hi);
            // gci i1/og / wave 9 lane G (`gch-h4pc-old-mark-and-true-root-review-findings`
            // item 2): the BFS's screen (`scan_object_for_old_refs_split_in`) before
            // the slot scan -- a bad kind / element tag or an extent that leaves old
            // gen is a corrupt header, and scanning it reads garbage or faults.
            // Both arms reach here with such an object (the BFS rejected it
            // and counted it already, so no counter moves twice). Its side-table
            // edges below do not read the header and are still followed.
            if Self::scannable_old_object(obj, ctx.old_gen) {
                // SAFETY: `obj` is a marked old-gen object base the mark
                // popped, its tag bytes and extent were just validated.
                let header = unsafe { &*(obj as *const ObjectHeader) };
                // SAFETY: as above.
                unsafe {
                    for_each_ref_slot(obj as *mut u8, header, |r, _| {
                        let a = r as usize;
                        if a >= lo && a < hi {
                            targets.push(a);
                        }
                    });
                }
            }
            let from = targets.len();
            self.side_table_targets(obj, ctx, &mut targets);
            // Old targets of the side tables are the BFS's own business.
            let mut i = from;
            while i < targets.len() {
                if targets[i] >= lo && targets[i] < hi {
                    i += 1;
                } else {
                    targets.swap_remove(i);
                }
            }
        }
        for t in targets {
            self.seed_word(t);
        }
        for t in source_keyed {
            self.follow(
                t,
                ctx,
                worklist,
                "true-root promoted owner's source-keyed pin",
            );
        }
        self.drain(ctx, worklist);
    }

    /// Wave 9 lane G (gch-h4pc item 2): the three tests the old mark's BFS
    /// screens an object with before scanning its slots
    /// (`scan_object_for_old_refs_split_in`): valid kind and element tags,
    /// and a whole extent inside old gen. `false` = a corrupt header.
    pub fn scannable_old_object(obj: usize, old_gen: &OldGen) -> bool {
        let obj_ptr = obj as *mut u8;
        if !old_gen.contains_range(obj_ptr, HEADER_SIZE) {
            return false;
        }
        // SAFETY: the header bytes lie in old gen (just checked); raw `u8`
        // reads have no validity requirement.
        let kind_tag = unsafe { cratonvm_types::kind_tag_at(obj_ptr) };
        let elem_tag = unsafe { cratonvm_types::element_type_tag_at(obj_ptr) };
        if object_kind_from_tag(kind_tag).is_none()
            || !cratonvm_types::element_tag_ok(kind_tag, elem_tag)
        {
            return false;
        }
        // SAFETY: in old gen, tag bytes validated above.
        let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
        let total = gen_object_total_size(header);
        total >= HEADER_SIZE && old_gen.contains_range(obj_ptr, total)
    }

    /// Young survivors the trace did not reach (kept only for dead old data).
    pub fn excluded(&self) -> usize {
        self.grid.len().saturating_sub(self.visited.len())
    }
}

/// gci i2/og (the next P-C cost cut, `vi1-og.list` C: P-C's steady major was
/// 235.4 vs a 233.4 ms bound) — the grid objects a [`TrueRootYoung`] trace
/// reached.
///
/// `ByAddr` is the set it always was: a hash set of bases, grown from empty,
/// one insert per young object reached and one lookup per young edge. On a
/// moving cycle's major (P-C) the grid is every young SURVIVOR of the pause
/// (~460k objects per steady major of `GenR4W4SteadyPromotionProbe`), and
/// almost all of them are reached, so that set is rebuilt to ~460k entries
/// (several rehashes of a table far larger than L2) on every major, while the
/// legacy seed it replaces walks the same objects linearly with no set at all.
/// `ByIndex` keeps one bit per grid index (`seed_word` already finds the
/// index by its binary search): the same answers -- an object is reached iff
/// its bit is set, and each grid index names one base -- at one L2-resident
/// bit per object. Chosen by `TrueRootYoung::build`'s `visited_by_index`:
/// the P-C ask, and (gci i3/og) every non-moving true-root major under
/// `CRATONVM_GC_TRUE_ROOT_BATCHED_VERDICT` (opt-in); otherwise the non-moving
/// majors keep `ByAddr` until that switch's row decides
/// (`gci-i2og-...-review-findings` item 3).
pub enum TrueRootVisited {
    ByAddr(FxHashSet<usize>),
    ByIndex { bits: Vec<u64>, count: usize },
}

impl Default for TrueRootVisited {
    fn default() -> Self {
        Self::ByAddr(FxHashSet::default())
    }
}

impl TrueRootVisited {
    /// An empty bitset for a grid of `grid_len` objects.
    pub fn by_index(grid_len: usize) -> Self {
        Self::ByIndex {
            bits: vec![0u64; grid_len.div_ceil(64)],
            count: 0,
        }
    }

    /// Record the grid object at `idx` (base `base`); `true` the first time.
    pub fn insert(&mut self, idx: usize, base: usize) -> bool {
        match self {
            Self::ByAddr(set) => set.insert(base),
            Self::ByIndex { bits, count } => {
                let (word, bit) = (idx / 64, 1u64 << (idx % 64));
                if word >= bits.len() {
                    bits.resize(word + 1, 0);
                }
                if bits[word] & bit != 0 {
                    false
                } else {
                    bits[word] |= bit;
                    *count += 1;
                    true
                }
            }
        }
    }

    /// Was the grid object at `idx` (base `base`) recorded?
    pub fn contains(&self, idx: usize, base: usize) -> bool {
        match self {
            Self::ByAddr(set) => set.contains(&base),
            Self::ByIndex { bits, .. } => bits
                .get(idx / 64)
                .is_some_and(|&w| w & (1u64 << (idx % 64)) != 0),
        }
    }

    /// How many objects were recorded.
    pub fn len(&self) -> usize {
        match self {
            Self::ByAddr(set) => set.len(),
            Self::ByIndex { count, .. } => *count,
        }
    }
}

/// gci i1/og (`gch-h4pc-old-mark-and-true-root-review-findings-RETIRED-UNFIXED-20261018` item
/// 2) — may the slots of the old-gen object at `obj` be walked? The two
/// screens of [`GenerationalHeap::scan_object_for_old_refs_split_in`] (a valid
/// kind / element tag, and a whole extent inside old gen), without its
/// counters: every object asked here was popped, and so screened and counted,
/// by the old mark's BFS first.
///
/// merge-dev-20261015: jr20 wave 9 lane G fixed the same item as
/// [`TrueRootYoung::scannable_old_object`], which also range-checks the header
/// before reading its tags; the production site calls that one and this name
/// delegates to it for gci's tests.
#[cfg_attr(not(test), allow(dead_code))]
pub fn old_object_slots_scannable(obj: *mut u8, old_gen: &OldGen) -> bool {
    TrueRootYoung::scannable_old_object(obj as usize, old_gen)
}

/// gch h4/wr — the WATCHED young survivors a finished true-root trace `y` did
/// not reach, as the watched set's own keys (pre-collection addresses),
/// ascending (`CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD`).
///
/// An address counts when it lies in a parsed young object (`index_of`) whose
/// base the trace never visited. Everything else is left to its usual verdict:
/// a promotion SOURCE (resolved through `TrueRootSweepOut::dead_sources` when
/// its destination is not reached), an old-generation address (the old mark
/// and sweep decide it), and an address in an unparseable stretch (every word
/// there is a root, so what lives there is kept and is not judged).
///
/// Must be asked after the trace's fixed point: `visited` only grows.
pub fn watched_left_out_by_trace(
    y: &TrueRootYoung,
    watched: &cratonvm_gc_core::gc_quiescence::WatchedSnapshot,
) -> Vec<usize> {
    let mut out: Vec<usize> = watched
        .iter()
        .copied()
        .filter(|&addr| y.promoted_index(addr).is_none())
        .filter(|&addr| y.index_of(addr).is_some_and(|idx| !y.reached(idx)))
        .collect();
    out.sort_unstable();
    out
}

/// gci i3/og (`CRATONVM_GC_MOVING_MAJOR_WATCHED_DEAD`) — the P-C twin of
/// [`watched_left_out_by_trace`]: of `post_copy` (the watched set's young
/// copies in the post-swap grid, ascending), the addresses that lie in a
/// parsed young object the finished trace `y` never reached, ascending. An
/// address outside the grid (an unparseable stretch, whose words are all
/// roots, or not young at all) is not judged. Must be asked after the trace's
/// fixed point.
pub fn moving_watched_left_out_by_trace(y: &TrueRootYoung, post_copy: &[usize]) -> Vec<usize> {
    let mut out: Vec<usize> = post_copy
        .iter()
        .copied()
        .filter(|&addr| y.index_of(addr).is_some_and(|idx| !y.reached(idx)))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// gci i3/og (`CRATONVM_GC_MOVING_MAJOR_WATCHED_DEAD`) — the moving cycle's
/// half of P-C's watched-dead verdict, before the major: every watched
/// PRE-copy address whose `pointer_map` value is YOUNG (`is_young`: a to-space
/// copy, in the post-swap grid), as `(post-copy, pre-copy)` sorted by post-copy
/// address. A watched promotion (an old value) is left out: its destination is
/// a skipped-tail root only, so the dead-promotion verdict judges it. An
/// unmapped address did not move (old) or did not survive.
pub fn moving_watched_young_copies(
    pointer_map: &cratonvm_types::PointerMap,
    watched: &cratonvm_gc_core::gc_quiescence::WatchedSnapshot,
    is_young: impl Fn(usize) -> bool,
) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = watched
        .iter()
        .filter_map(|&pre| pointer_map.get(&pre).map(|&post| (post, pre)))
        .filter(|&(post, _)| post != 0 && post % 8 == 0 && is_young(post))
        .collect();
    out.sort_unstable();
    out
}

/// gci i3/og (`CRATONVM_GC_MOVING_MAJOR_WATCHED_DEAD`) — map the trace's
/// left-out post-copy addresses (`left_out_post`, ascending) back to their
/// watched PRE-copy sources through `copies` (from
/// [`moving_watched_young_copies`]): every source whose copy was left out
/// (two watched keys can name one copy), in `copies` order.
pub fn moving_watched_left_out_sources(copies: &[(usize, usize)], left_out_post: &[usize]) -> Vec<usize> {
    copies
        .iter()
        .filter(|&&(post, _)| left_out_post.binary_search(&post).is_ok())
        .map(|&(_, pre)| pre)
        .collect()
}

/// gen r5w2/alloc6 (2026-09-26) — what a pinned compaction's pin set was made
/// of, for its `tracing` line. `GenR4W5OldPinnedCompactProbe` fails its
/// JIT-warm case with the compaction ENABLED, and the question the orchestrator
/// could not answer from the counters was: did the compaction run, and if it
/// did, which pins kept the free space apart, and where did each come from?
/// `RUST_LOG=cratonvm::gc=info` now prints it on the compaction's own line.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OldPinReport {
    /// Distinct pinned objects per source (an object named by two sources
    /// counts under both), indexed by `OLD_PIN_SRC_*.trailing_zeros()`.
    pub by_source: [usize; 6],
    /// Distinct objects pinned, and their bytes.
    pub pinned: usize,
    pub pinned_bytes: usize,
    /// Marked object bytes before pin-only resurrection.
    pub live_bytes: usize,
    /// Pinned objects the mark had NOT reached (the pin itself keeps them).
    pub resurrected: usize,
    /// Objects named only by live-only sources that the mark had not reached:
    /// NOT pinned (see [`settle_old_pins`]), and their bytes.
    pub dropped_unmarked: usize,
    pub dropped_unmarked_bytes: usize,
    /// The largest pinned objects: `(offset in the generation, size, source
    /// bits)`, largest first, at most [`OLD_PIN_REPORT_LARGEST`].
    pub largest: Vec<(usize, usize, u8)>,
    /// gen r5w6/old10 — per `largest` entry: does the root slice name the
    /// object's base MORE often than the published words do? `true` means
    /// some root that is not a published word (a static, a handle, a
    /// promotion seed, a peer-range root) holds it too, i.e. the object is
    /// live on its own and the word only PINS it; `false` means the word(s)
    /// may be all that RETAINS it. The two readings of a JIT-warm
    /// `made_room=false` (`GenR4W5OldPinnedCompactProbe`) need different
    /// fixes — see `gengc-r4w6-review6-old-pinned-compaction-residuals`.
    /// Filled by `old_gen_gc_inner` after [`settle_old_pins`]; a heuristic
    /// over counts, not a proof (an interior-valued word is not counted).
    pub largest_also_rooted: Vec<bool>,
}

/// gen r5w2/alloc6 — the mark's raw pin list (`pins[i]` named by source
/// `srcs[i]`, one `OLD_PIN_SRC_*` bit) turned into the pin set
/// `OldGen::compact_around_pins` receives, with its [`OldPinReport`].
///
/// Duplicates merge (their source bits OR). An entry that is not a walked
/// object base is dropped, as the compaction drops it. And an object named
/// ONLY by live-only sources ([`OLD_PIN_SRC_LIVE_ONLY`]) that the mark did not
/// reach is dropped too, instead of being resurrected by Phase 0a:
///
/// * a root-range root or a published word that is the base of (or inside) an
///   object is a ROOT of this pause — the root slice holds every published
///   word, and the root loop marks every walked base and every interior word
///   in it. An unmarked object named that way is named by a word that is NOT
///   in this pause's roots: a stale publication. The in-place sweep would free
///   it; pinning it resurrected it and could split the very free run the
///   compaction exists to make.
/// * a one-past-the-end word (`word - 1` inside the object) pins its object so
///   a live array does not slide under a compiled loop's end bound. The
///   interior rule does not mark through such a word, so an unmarked object
///   here is one no root reaches: the sweep frees it, and so may the
///   compaction. Over-pinning a LIVE object stays exactly as it was.
///
/// Must run after the mark's closure (the BFS) and before the compaction,
/// with the old-gen lock held; `walked` is the pause's object grid.
pub fn settle_old_pins(
    pins: Vec<usize>,
    srcs: Vec<u8>,
    walked: &[(*mut u8, usize)],
    old_base: usize,
    count_live_bytes: bool,
) -> (Vec<usize>, OldPinReport) {
    debug_assert_eq!(pins.len(), srcs.len(), "one source per pin entry");
    let mut report = OldPinReport::default();
    let mut tagged: Vec<(usize, u8)> = pins.into_iter().zip(srcs).collect();
    tagged.sort_unstable_by_key(|&(base, _)| base);
    let mut merged: Vec<(usize, u8)> = Vec::with_capacity(tagged.len());
    for (base, src) in tagged {
        match merged.last_mut() {
            Some(last) if last.0 == base => last.1 |= src,
            _ => merged.push((base, src)),
        }
    }
    let mut kept: Vec<usize> = Vec::with_capacity(merged.len());
    // The fragmentation budget is pinned bytes / all marked live bytes. Count
    // the complete mark before visiting the pin subset; using only marked
    // pinned objects as the denominator made every fully pinned pin set look
    // like 100% of live data, even when the rest of the generation was freely
    // movable.
    if count_live_bytes {
        for &(base, size) in walked {
            // SAFETY: each base and size came from the pause's validated old-gen
            // walk, while the generation lock prevents movement or reclamation.
            let marked = unsafe { &*(base as *const ObjectHeader) }.gc_flags() & GC_FLAG_MARKED != 0;
            if marked {
                report.live_bytes = report.live_bytes.saturating_add(size);
            }
        }
    }
    for (base, src) in merged {
        let Ok(i) = walked.binary_search_by_key(&base, |&(p, _)| p as usize) else {
            continue;
        };
        let size = walked[i].1;
        // SAFETY: `base` is an object base the walk yielded (found just above),
        // read under the old-gen lock the pause holds; nothing has moved yet.
        let marked = unsafe { &*(base as *const ObjectHeader) }.gc_flags() & GC_FLAG_MARKED != 0;
        if !marked && src & !OLD_PIN_SRC_LIVE_ONLY == 0 {
            report.dropped_unmarked += 1;
            report.dropped_unmarked_bytes += size;
            continue;
        }
        for (bit, count) in report.by_source.iter_mut().enumerate() {
            if src & (1u8 << bit) != 0 {
                *count += 1;
            }
        }
        report.pinned += 1;
        report.pinned_bytes += size;
        if !marked {
            report.resurrected += 1;
        }
        report
            .largest
            .push((base.wrapping_sub(old_base), size, src));
        kept.push(base);
    }
    report
        .largest
        .sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    report.largest.truncate(OLD_PIN_REPORT_LARGEST);
    (kept, report)
}

/// gen r5w6/old10 — [`OldPinReport::largest_also_rooted`]: for each
/// `(offset, _, _)` of `largest` (offsets from `old_base`), does `roots` name
/// the object's base more often than `words` does? Words are counted only
/// where they EQUAL the base (an interior-valued word is not in the root
/// slice as that base either). O(`largest` × (roots + words)).
pub fn old_pins_also_rooted(
    largest: &[(usize, usize, u8)],
    old_base: usize,
    roots: &[ObjectRef],
    words: &[usize],
) -> Vec<bool> {
    largest
        .iter()
        .map(|&(off, _, _)| {
            let b = old_base.wrapping_add(off);
            let in_roots = roots.iter().filter(|r| r.as_ptr() as usize == b).count();
            let in_words = words.iter().filter(|&&w| w == b).count();
            in_roots > in_words
        })
        .collect()
}

impl OldPinnedCompact {
    /// A plan with explicit words and a pinned root PREFIX (the gates are the
    /// caller's). `new(words, n)` pins roots `0..n`. Tests only since
    /// gen r4w6/oldpin6 (production builds plans through
    /// [`Self::plan_from_sources`]).
    #[cfg(test)]
    pub fn new(words: Vec<usize>, pin_root_prefix: usize) -> Self {
        Self::with_range(words, (0, pin_root_prefix))
    }

    /// gen r5w1/oldgen5 — does this plan pin nothing a conservative source
    /// named: no published word and an empty root range? Then every root of
    /// the pause is precise (interior roots and unparseable young stretches
    /// still pin through the mark, as on every plan), which is the condition
    /// under which the non-moving path compacts by default for a refused
    /// humongous allocation.
    pub fn pins_nothing_conservative(&self) -> bool {
        self.words.is_empty() && self.pin_root_range.0 >= self.pin_root_range.1
    }

    /// A plan with explicit words and a pinned root RANGE `lo..hi`
    /// (gen r4w6/oldpin6).
    pub fn with_range(words: Vec<usize>, pin_root_range: (usize, usize)) -> Self {
        Self {
            words,
            pin_root_range,
            young_ref_holders: Vec::new(),
            element_card_base: None,
            compacted: None,
            refused: None,
            report: OldPinReport::default(),
            movable_words_unpinned: 0,
            fragmentation_triggered: false,
            reactive_fragmentation_repair: false,
            fragmentation_slide_goal: None,
        }
    }

    /// The plan for THIS pause, from the published conservative sources, or
    /// why none may run. `caller_roots` is the length of the caller's root
    /// slice (before any promotion seeds are appended).
    pub fn for_this_pause(caller_roots: usize) -> Result<Self, &'static str> {
        // Taken FIRST, before any refusal, so a layout the door published can
        // never outlive the pause it describes (the door also clears it after
        // the collection; see `clear_old_pin_root_layout`).
        let layout = take_old_pin_root_layout();
        if cratonvm_gc_core::gc_quiescence::may_move_under_takeover(true)
            == cratonvm_gc_core::gc_quiescence::MoveLicence::NonMoving
        {
            return Err("the take-over licence is NonMoving");
        }
        // gci i3/tk: also any peer-state reason recorded through the pinned
        // door (`CRATONVM_GEN_TAKEOVER_PROMOTES` /
        // `CRATONVM_GEN_HELPER_WINDOW_REFUSAL_PROMOTES`, opt-in), which
        // licenses selective promotion only, not this plan. On the default
        // path every such reason also raised the bit, so this is unchanged.
        if cratonvm_gc_core::gc_quiescence::unrewritable_peer_state()
            || cratonvm_gc_core::gc_quiescence::peer_state_reason_recorded()
        {
            return Err("un-rewritable peer state (a frozen in-JIT peer)");
        }
        // The sources that put conservative words in the root slice without
        // publishing them through `gc_quiescence` (see the type's doc).
        let interpreter_probe = cratonvm_gc_core::gc_quiescence::compiled_frames_live()
            || cratonvm_gc_core::gc_quiescence::conservative_peers_live();
        // gcd d1/d: the typed, latched field (it was a `runtime_var_os` read
        // per pause).
        let jit_withheld = gc_flags().g1_only_jit_pins;
        // gcf f4/c: `CRATONVM_GC_OLD_PIN_UNREWRITABLE_ONLY` (opt-in) -- the
        // compiled-frame words only (the interpreter probe's words, which
        // `plan_from_sources` adds from the layout, are never filtered: no
        // channel rewrites them).
        //
        // gcg g3/e (hunk H2, applied by gci i1/pn): only the CALLER's own
        // words may be unpinned. The movable / unrewritable sets are the
        // caller's thread-locals and say nothing about a word another thread
        // published (`gcg-g3e-unrewritable-only-plan-unpins-words-other-threads-published`).
        let (words, unpinned) = if old_pin_unrewritable_only_enabled() {
            // gcg g3/e (hunk H2, GC wave 9 lane gcD): only the caller's own
            // words may be unpinned; every word another thread (or a
            // helper window) published pins.
            let (own, others) = cratonvm_gc_core::gc_quiescence::pinned_jit_roots_snapshot_split();
            movable_words_unpinned_by_thread(
                own,
                others,
                cratonvm_gc_core::gc_quiescence::is_movable_jit_root,
                cratonvm_gc_core::gc_quiescence::is_unrewritable_jit_root,
            )
        } else {
            (cratonvm_gc_core::gc_quiescence::pinned_jit_roots_snapshot(), 0)
        };
        let mut plan =
            Self::plan_from_sources(words, caller_roots, interpreter_probe, jit_withheld, layout);
        plan.movable_words_unpinned = unpinned;
        Ok(plan)
    }

    /// gen r4w6/oldpin6 — the pure decision behind [`Self::for_this_pause`],
    /// separated so every arm is unit-testable without a VM.
    ///
    /// * No unpublished source ran (`!interpreter_probe && !jit_withheld`):
    ///   the published words only, no root range — exactly wave 5.
    /// * `jit_withheld` (`CRATONVM_GC_G1_ONLY_JIT_PINS`): the compiled-frame
    ///   words are in the slice but not in `words`; nothing tells them apart,
    ///   so the whole slice pins, whatever the door published.
    /// * The interpreter probe ran, and the door published a layout for a
    ///   slice of exactly `caller_roots` entries: its words join the pins and
    ///   only its `pin_all` range (the peers' part) pins wholesale.
    /// * The interpreter probe ran and no usable layout exists (a door that
    ///   does not publish, or one whose slice length disagrees, i.e. the
    ///   layout describes some other slice): the whole slice pins — wave 5's
    ///   fallback, never less.
    pub fn plan_from_sources(
        mut words: Vec<usize>,
        caller_roots: usize,
        interpreter_probe: bool,
        jit_withheld: bool,
        layout: Option<OldPinRootLayout>,
    ) -> Self {
        if !interpreter_probe && !jit_withheld {
            return Self::with_range(words, (0, 0));
        }
        if jit_withheld {
            return Self::with_range(words, (0, caller_roots));
        }
        match layout {
            Some(l) if l.describes(caller_roots) => {
                OLD_PIN_LAYOUT_USED.fetch_add(1, Ordering::Relaxed);
                words.extend_from_slice(&l.interpreter_words);
                Self::with_range(words, l.pin_all)
            }
            _ => {
                OLD_PIN_LAYOUT_FALLBACKS.fetch_add(1, Ordering::Relaxed);
                Self::with_range(words, (0, caller_roots))
            }
        }
    }
}

/// gen r4w6/oldpin6 (2026-09-24) — the collection door's description of which
/// part of the root slice it hands the collector may hold UNPUBLISHED
/// conservative words, and the interpreter conservative probe's words for its
/// own thread. oldcompact5's cross-lane request (the review page
/// `gengc-round4-w5-oldcompact5-20260924.md`, "Root gatherer"): with it, a
/// pinned old-gen compaction under a live compiled frame pins only the objects
/// conservative words name, instead of every root.
///
/// Built by `vm/src/runtime/interpreter/gc_and_alloc.rs::
/// publish_old_pin_root_layout_for_pause` for the slice it is about to pass
/// to `collect_garbage_with_finalizers`:
///
/// * `interpreter_words` — every word the conservative frame scanners
///   (`Frame::scan_locals_conservative`, `ValueStack::scan_object_refs_conservative`)
///   recover from the INITIATOR's interpreter frames. These are the words
///   step 1 (`conservative_locals`) and step 14a5 put in the slice; the door
///   re-derives them from the same frames, in the same pause, with the same
///   heap, so it finds every one of them.
/// * `pin_all` — `[lo, hi)` of the slice that still pins wholesale: the
///   peers' root snapshots and the take-over's words. A peer's snapshot may
///   hold probe words from ITS deposit, which the door cannot re-derive.
/// * `root_len` — the length of the slice described. A layout whose length
///   disagrees with the collector's slice is ignored (fail closed: the whole
///   slice pins).
///
/// Everything else in the initiator's part of the slice is precise, and the
/// VM rewrites it through the returned map: statics (`STATIC_REF_SLOTS`),
/// side tables, JNI handles, `Object`-tagged frame slots, the initiator's
/// compiled-frame words (published through `gc_quiescence`, and so already in
/// the plan's `words`).
///
/// Per THREAD, not per process: the door and the collector run on the same
/// (initiator) thread within one pause — the same reasoning as
/// `gc_quiescence::unregistered_jit_frame_on_stack`. No process global.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OldPinRootLayout {
    /// The initiator's interpreter conservative probe words (see above).
    pub interpreter_words: Vec<usize>,
    /// `[lo, hi)` of the root slice to pin wholesale.
    pub pin_all: (usize, usize),
    /// Length of the root slice this layout describes.
    pub root_len: usize,
}

impl OldPinRootLayout {
    /// Does this layout describe a slice of `caller_roots` entries, with a
    /// well-formed range inside it?
    pub fn describes(&self, caller_roots: usize) -> bool {
        self.root_len == caller_roots
            && self.pin_all.0 <= self.pin_all.1
            && self.pin_all.1 <= caller_roots
    }
}

thread_local! {
    pub static OLD_PIN_ROOT_LAYOUT: std::cell::RefCell<Option<OldPinRootLayout>> =
        const { std::cell::RefCell::new(None) };
}

/// Pinned compactions whose pin set used a door's [`OldPinRootLayout`], and
/// those that fell back to pinning the whole slice (diagnostics; process-wide
/// counters, not state).
pub static OLD_PIN_LAYOUT_USED: AtomicUsize = AtomicUsize::new(0);
pub static OLD_PIN_LAYOUT_FALLBACKS: AtomicUsize = AtomicUsize::new(0);

/// `(used, fallbacks)` — see `OLD_PIN_LAYOUT_USED`.
pub fn old_pin_root_layout_counters() -> (usize, usize) {
    (
        OLD_PIN_LAYOUT_USED.load(Ordering::Relaxed),
        OLD_PIN_LAYOUT_FALLBACKS.load(Ordering::Relaxed),
    )
}

/// Publish the calling thread's [`OldPinRootLayout`] for the collection it is
/// about to run. Replaces any earlier one.
pub fn publish_old_pin_root_layout(layout: OldPinRootLayout) {
    let _ = OLD_PIN_ROOT_LAYOUT.try_with(|c| *c.borrow_mut() = Some(layout));
}

/// Drop the calling thread's layout (the door calls this after the
/// collection, whether or not a pinned compaction consumed it).
pub fn clear_old_pin_root_layout() {
    let _ = OLD_PIN_ROOT_LAYOUT.try_with(|c| c.borrow_mut().take());
}

/// Take the calling thread's layout, leaving none.
pub fn take_old_pin_root_layout() -> Option<OldPinRootLayout> {
    OLD_PIN_ROOT_LAYOUT
        .try_with(|c| c.borrow_mut().take())
        .ok()
        .flatten()
}

/// gen r5w1/oldgen5 (2026-09-26) — what the young pause's dirty-card scan
/// found in old-gen WALK GAPS: bytes behind a header no walk can size, inside
/// the part of a gap a dirty card reaches (`OldGen::walk_card_ranges_with_gaps`).
///
/// The young half of `gengc-r4w4-oldgen4-a-walk-gap-hides-old-to-young-roots-from-the-card-scan`:
/// the objects in a gap cannot be parsed, so their reference slots cannot be
/// told from their primitive fields. Every aligned word there that lands in
/// young from-space is reported instead. A word may be a real reference, an
/// interior value or a primitive that happens to look like one, so a consumer
/// must treat each as a CONSERVATIVE root: keep the young object it names
/// alive and leave it where it is (the non-moving mark's interior-aware
/// `mark_young`, or the pinned in-place copy's pin words on a moving cycle).
/// Never rewrite the word: it may not be a reference.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CardScanGapRoots {
    /// Distinct young from-space values, ascending: every non-zero aligned
    /// 8-byte word in a gap window that `Arena::contains`, and, under
    /// compressed references, every decoded non-zero 4-byte half that does.
    pub young_words: Vec<usize>,
    /// The old-gen address of every gap word that produced a young value. A
    /// consumer that keeps the young objects where they are must keep these
    /// cards dirty (`CardTable::mark_dirty_bulk`), as the non-moving mark does
    /// for its precise holders, or the next young pause will not look again.
    pub card_addrs: Vec<usize>,
    /// Gap windows scanned, and their bytes.
    pub windows: usize,
    pub window_bytes: usize,
}

/// gen r5w1/oldgen5 — scan the card walk's gap windows (offsets from
/// `old_base`) for young from-space values; see [`CardScanGapRoots`].
///
/// Cost: one read per aligned word of the windows, which lie inside dirty
/// cards' unparseable stretches only — zero on a sane heap.
pub fn card_walk_gap_young_words(
    old_base: usize,
    windows: &[(usize, usize)],
    young_from: &Arena,
) -> CardScanGapRoots {
    let narrow = cratonvm_types::narrow_oop::narrow_oops_enabled();
    let mut out = CardScanGapRoots::default();
    for &(start, end) in windows {
        out.windows += 1;
        out.window_bytes += end.saturating_sub(start);
        let mut a = old_base + ((start + 7) & !7);
        let hi = old_base + end;
        while a + 8 <= hi {
            // SAFETY: `[a, a + 8)` lies inside `[old_base + start, old_base +
            // end)`, which the walk derived from an ALLOCATED region of the old
            // generation (between two free blocks), so it is committed and
            // readable (allocated bytes lie below `readable_end`, and a
            // give-back hole is always a free block); `a` is 8-aligned because
            // the base is (`OldGen::new` debug-asserts it) and the offset is
            // rounded up. The young pause has stopped the world, so nothing
            // writes it concurrently. Same argument as `seed_old_gen_walk_gaps`.
            let w = unsafe { std::ptr::read(a as *const u64) };
            if w != 0 {
                let mut hit = false;
                if young_from.contains(w as usize as *const u8) {
                    out.young_words.push(w as usize);
                    hit = true;
                }
                if narrow {
                    for half in [w as u32, (w >> 32) as u32] {
                        if half != 0 {
                            let d = cratonvm_types::narrow_oop::decode(half) as usize;
                            if young_from.contains(d as *const u8) {
                                out.young_words.push(d);
                                hit = true;
                            }
                        }
                    }
                }
                if hit {
                    out.card_addrs.push(a);
                }
            }
            a += 8;
        }
    }
    out.young_words.sort_unstable();
    out.young_words.dedup();
    out
}

/// How many times [`note_card_walk_gap_young_words`] has warned, process-wide
/// (a log rate limit only; the per-generation count is
/// `BlockOffsetStats::gap_windows`).
pub static CARD_WALK_GAP_WARNS: AtomicU64 = AtomicU64::new(0);

/// Cold reporter: the first few young pauses whose card scan found young
/// values in an old-gen walk gap. Such a pause is only sound if its caller
/// pins them (see [`CardScanGapRoots`]).
#[cold]
#[inline(never)]
pub fn note_card_walk_gap_young_words(roots: &CardScanGapRoots) {
    if CARD_WALK_GAP_WARNS.fetch_add(1, Ordering::Relaxed) < 8 {
        tracing::warn!(
            target: "cratonvm::gc",
            young_words = roots.young_words.len(),
            windows = roots.windows,
            window_bytes = roots.window_bytes,
            "young pause: the dirty-card scan found young references in old-gen bytes \
             behind a header no walk can size; they are conservative roots (gen r5w1/oldgen5, \
             gengc-r4w4-oldgen4-a-walk-gap-hides-old-to-young-roots-from-the-card-scan)",
        );
    }
}

/// gen r4w4/oldgen4 (2026-09-24) — treat the old-gen bytes an object walk
/// could NOT cover as a conservative root range, so an in-place sweep over an
/// incomplete walk can still free what it did walk.
///
/// `gaps` are `OldGen::walk_objects_with_gaps` offsets: each is the tail of an
/// allocated region behind an implausible header. Objects in there are
/// neither walked nor marked nor traced, so anything reachable ONLY through
/// one of them would be freed while live — the reason the sweep used to free
/// nothing at all on such a cycle, which never ends because the bad header
/// never goes away
/// (`gengc-r4w3-oldgen3-a-walk-desync-stops-all-old-gen-reclamation`). This
/// marks, and pushes for tracing, everything such an object could name:
///
/// * every 8-byte word in a gap that is a WALKED object base or points
///   strictly inside a walked object (heap references, including derived
///   ones);
/// * under compressed references, both 4-byte halves of every word, decoded
///   (`narrow_oop::decode`) and resolved the same way;
/// * every side-table edge whose OWNER lies in a gap: external overlay roots,
///   mirror pins, metadata pins — keyed by owner address, so resolvable
///   without parsing the owner;
/// * every pinned class loader, when loader pinning is on: a loader pin is
///   keyed by the CLASS of an object, which cannot be read from a header the
///   walk rejected, so all of them are kept (over-retention, bounded by the
///   number of pinned loaders).
///
/// A word pointing INTO a gap marks nothing: the gap is retained whole (the
/// sweep frees only walked objects), and its own words are scanned here
/// regardless of whether the object holding them is live. So the closure is
/// sound for any content of the gap — it can only over-retain — and it needs
/// no header in the gap to be parseable. Stale `GC_FLAG_MARKED` bits inside a
/// gap (a conservative root screen may set one on a gap header) are
/// harmless for the same reason: nothing decides the gap's fate from a mark.
///
/// Returns how many walked objects the gap words themselves marked.
pub fn seed_old_gen_walk_gaps(
    old_gen: &OldGen,
    walked: &[(*mut u8, usize)],
    gaps: &[(usize, usize)],
    mirror_pin_owners: &FxHashSet<usize>,
    metadata_pins: Option<&FxHashMap<usize, Vec<usize>>>,
    loader_pin_on: bool,
    rekeyed_mirrors: Option<&FxHashMap<usize, Vec<usize>>>,
    rekeyed_loaders: Option<&FxHashMap<u32, usize>>,
    worklist: &mut Vec<*mut u8>,
) -> usize {
    /// Mark the walked object `cand` names (as a base or an interior word).
    fn mark_walked(cand: usize, walked: &[(*mut u8, usize)], worklist: &mut Vec<*mut u8>) -> bool {
        let base = if walked_grid_has_base(walked, cand) {
            cand        } else if let Some((b, _)) = old_gen_interior_root_base(cand as *mut u8, walked) {
            b        } else {
            return false;
        };
        // SAFETY: `base` is an object base `walk_objects_with_gaps` yielded
        // for this pause, under the old-gen lock the caller holds, so its
        // header is valid and writable.
        let header = unsafe { &mut *(base as *mut ObjectHeader) };
        if header.gc_flags() & GC_FLAG_MARKED != 0 {
            return false;
        }
        header.add_gc_flags(GC_FLAG_MARKED);
        worklist.push(base as *mut u8);
        true
    }

    let base = old_gen.base_ptr() as usize;
    let narrow = cratonvm_types::narrow_oop::narrow_oops_enabled();
    let mut marked = 0usize;
    for &(start, end) in gaps {
        let mut a = base + ((start + 7) & !7);
        let hi = base + end;
        while a + 8 <= hi {
            // SAFETY: `[a, a + 8)` lies inside `[base + start, base + end)`, an
            // allocated region of the old generation (the walk derives gaps
            // from the gaps BETWEEN free blocks), so it is committed and
            // readable; `a` is 8-aligned because `base` is (`OldGen::new`
            // debug-asserts it) and the offset is rounded up to 8. The world is
            // stopped, so nothing writes it concurrently.
            let w = unsafe { std::ptr::read(a as *const u64) };
            if w != 0 {
                if mark_walked(w as usize, walked, worklist) {
                    marked += 1;
                }
                if narrow {
                    for half in [w as u32, (w >> 32) as u32] {
                        if half != 0 {
                            let d = cratonvm_types::narrow_oop::decode(half) as usize;
                            if mark_walked(d, walked, worklist) {
                                marked += 1;
                            }
                        }
                    }
                }
            }
            a += 8;
        }
    }

    // Side-table edges owned by an object inside a gap.
    let in_gap = |addr: usize| -> bool {
        let Some(off) = addr.checked_sub(base) else {
            return false;
        };
        let i = gaps.partition_point(|&(s, _)| s <= off);
        i > 0 && off < gaps[i - 1].1
    };
    for overlay_ref in
        cratonvm_gc_core::external_roots::external_roots_for_matching_owners(&|owner| in_gap(owner))
    {
        mark_and_push_old_gen(
            overlay_ref.as_ptr(),
            old_gen,
            walked,
            worklist,
            "external-overlay(walk-gap owner)",
        );
    }
    for &owner in mirror_pin_owners.iter().filter(|&&o| in_gap(o)) {
        // gci i8/pa: the caller's re-keyed rows when it has them.
        if let Some(mirror_addrs) = pinned_mirrors_of(owner, rekeyed_mirrors) {
            for mirror_addr in mirror_addrs {
                mark_and_push_old_gen(
                    mirror_addr as *mut u8,
                    old_gen,
                    walked,
                    worklist,
                    "mirror_pin(walk-gap owner)",
                );
            }
        }
    }
    if let Some(pins) = metadata_pins {
        for (&owner, addrs) in pins.iter() {
            if !in_gap(owner) {
                continue;
            }
            for &metadata_addr in addrs {
                mark_and_push_old_gen(
                    metadata_addr as *mut u8,
                    old_gen,
                    walked,
                    worklist,
                    "metadata_pin(walk-gap owner)",
                );
            }
        }
    }
    if loader_pin_on {
        // gc-common w36-d: per class id, the row inside THIS old generation
        // when VMs share the id (`common-w4b-loader-pin-collision-...`).
        // gci i8/pa: the caller's re-keyed rows (post-copy loaders) when it
        // has them; the screen below keeps the old ones.
        let loaders = match rekeyed_loaders {
            Some(rows) => Some(rows.clone()),
            None => cratonvm_types::loader_pin::snapshot_where(|a| old_gen.contains(a as *const u8)),
        };
        if let Some(loaders) = loaders {
            for (_class_id, loader_addr) in loaders {
                mark_and_push_old_gen(
                    loader_addr as *mut u8,
                    old_gen,
                    walked,
                    worklist,
                    "loader_pin(walk gap)",
                );
            }
        }
    }
    marked
}

/// Cold reporter for the first few interior conservative roots into old gen.
///
/// The counter alone only reaches a log through the shutdown summary, and the
/// runs that matter here are killed or crash before it. This says, from any
/// run, whether the workload actually produces interior old-gen roots — the
/// precondition for the defect the pin closes.
#[cold]
#[inline(never)]
pub fn note_interior_old_root(root: *mut u8, base: usize, size: usize) {
    // SAFETY: `base` is an object base `walk_objects` yielded, so its header is
    // mapped and readable.
    let header = unsafe { &*(base as *const ObjectHeader) };
    tracing::warn!(
        "old-gen mark: conservative root {root:p} is an INTERIOR word of the live object at \
         {base:#x}+{size:#x} (interior_off={}, class_id={}, kind={}) — pinning the containing \
         object. Without this the in-place sweep frees it under a root it cannot rewrite; see \
         old-sweep-liveness.md section 7",
        root as usize - base,
        header.class_id.as_u32(),
        ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)),
    );
}

/// GCAUD-8 — the fail-closed second opinion on a rejected mark candidate.
///
/// `walked` is the ascending `(base, size)` grid
/// [`OldGen::walk_objects`] yielded for THIS collection pause. Membership in it
/// is a derivation, not a heuristic: the walk computes allocated extents as the
/// gaps between free blocks and then strides header-by-header through each one,
/// so every address in the list is an object base whose extent fits inside its
/// allocated region. That subsumes every clause of
/// [`old_gen_mark_candidate_plausible`] that is about *being an object*
/// (alignment, containment, a sane kind byte, an in-bounds extent) and is
/// unaffected by the clauses that are about a header *looking* untouched
/// (`_padding` / `_gc_reserved` / undefined `gc_flags` bits) — which is exactly
/// where a real object can be rejected by accident.
///
/// Returns `true` if the candidate may be marked anyway. `false` keeps the
/// existing reject.
///
/// The list MUST be from the current pause and the current old gen; a stale one
/// would authorise a mark-bit write into a recycled address, which is the
/// corruption family the screen was introduced to stop. `old_gen_gc` builds it
/// under the same old-gen lock it marks and sweeps under.
#[inline]
pub fn rescue_mark_candidate_by_walk(
    ptr: *mut u8,
    old_gen: &OldGen,
    walked: &[(*mut u8, usize)],
) -> bool {
    rescue_mark_candidate_in(ptr, old_gen, walked)
}

/// [`rescue_mark_candidate_by_walk`] over any [`OldMarkGrid`] (gen r5w5/old9).
#[inline]
pub fn rescue_mark_candidate_in<G: OldMarkGrid + ?Sized>(
    ptr: *mut u8,
    old_gen: &OldGen,
    walked: &G,
) -> bool {
    // Cheap gates first: a base is 8-aligned and inside old gen by
    // construction, so an address failing either can never be in the list and
    // the binary search is pure cost.
    if (ptr as usize) & 0x7 != 0 || !old_gen.contains(ptr) {
        return false;
    }
    walked.has_base(ptr as usize)
}

/// Cold reporter for a mark candidate the walk rescued from the plausibility
/// screen. Separate from [`note_rejected_old_mark_candidate`] because the two
/// mean opposite things: a reject says a side table holds a non-base address, a
/// rescue says the screen was about to drop a real object.
#[cold]
#[inline(never)]
pub fn note_rescued_old_mark_candidate(ptr: *mut u8, site: &'static str) {
    let n = OLDMARK_RESCUED_BY_WALK.fetch_add(1, Ordering::Relaxed);
    if n < 8 {
        // SAFETY: `walk_objects` yielded `ptr` as an object base, so its header
        // is mapped and readable.
        let header = unsafe { &*(ptr as *const ObjectHeader) };
        tracing::warn!(
            "old-gen mark [{site}]: candidate {ptr:p} FAILED the plausibility screen but IS an \
             object base the walk yielded (class_id={}, kind={}, shape={}, gc_flags={:#04x}) — \
             marking it. Without this the in-place sweep would free a live object; see \
             old-sweep-liveness.md.",
            header.class_id.as_u32(),
            ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)),
            header.num_slots(),
            header.gc_flags(),
        );
    }
}

/// Mark `ptr` and push it onto the old-gen mark worklist — but only if it is a
/// plausible old-gen object BASE. See [`old_gen_mark_candidate_plausible`].
///
/// An address outside old gen is silently ignored (it is simply not this
/// generation's business). An address INSIDE old gen that fails the screen is
/// counted and reported: it means a reference slot or a side table is holding
/// something that is not an object base.
///
/// # GCAUD-8 — the walked object grid is the fail-closed override
///
/// Every caller of this function is a **precise** site: a reference slot of a
/// live object, a registered overlay owner, a loader/mirror/metadata pin. None
/// of them is a guess. [`old_gen_mark_candidate_plausible`]'s own doc states
/// the consequence — "at a precise site a false reject IS a premature free" —
/// and then relies on the screen never false-rejecting a real object, which is
/// a property maintained by convention (`_padding`/`_gc_reserved` stay zero, no
/// fourth `gc_flags` bit is ever defined, no class has 2^24 fields) and checked
/// by nothing.
///
/// `walked` turns that convention into a check. It is
/// `OldGen::walk_objects()`' output for this very pause: the object grid
/// *derived* from the free list and strided header-by-header, not inferred from
/// one header's bytes. When the heuristic and the derivation disagree about an
/// address the derivation produced, the derivation wins and the object is
/// marked. The cost of being wrong in this direction is one cycle of retained
/// garbage; the cost of being wrong in the other direction is the in-place
/// sweep freeing it under a live pointer.
pub fn mark_and_push_old_gen(
    ptr: *mut u8,
    old_gen: &OldGen,
    walked: &[(*mut u8, usize)],
    worklist: &mut Vec<*mut u8>,
    site: &'static str,
) {
    mark_and_push_old_gen_in(ptr, old_gen, walked, worklist, site);
}

/// [`mark_and_push_old_gen`] over any [`OldMarkGrid`] (gen r5w5/old9): the
/// walked grid on every default collection — the same expressions, so the
/// same answers — and the block-offset oracle on the O(live) one, which also
/// records every mark it makes and re-traces a stale one
/// ([`OldMarkGrid::retrace_stale_mark`]).
pub fn mark_and_push_old_gen_in<G: OldMarkGrid + ?Sized>(
    ptr: *mut u8,
    old_gen: &OldGen,
    walked: &G,
    worklist: &mut Vec<*mut u8>,
    site: &'static str,
) {
    if !old_gen.contains(ptr) {
        // gcj j3/og3 (Phase 0 drop): inside the extent but not readable (above
        // the committed end, or in a hole) is an escape to Phase 0, so the
        // mark cannot vouch for the closure.
        if old_gen.in_extent(ptr) {
            old_gen.note_mark_gap_fact();
        }
        return;
    }
    // A plausible-looking header is NECESSARY but not SUFFICIENT.
    //
    // The screen decodes bytes, so any interior address whose bytes happen to
    // read as a well-formed header passes it. That used to be a coincidence one
    // could not construct on purpose; since `identity_hash_code` left the
    // header (2026-08-07) it is systematic. `obj + 8` now lands the header's
    // 4-byte tail padding on `cratonvm_types::KIND_TAGS_BYTE_OFFSET`, and zero is a perfectly
    // valid `ObjectKind::Object`, while `num_slots` reads the low half of an
    // unhashed, unlocked mark word -- also zero. So EVERY object's `obj + 8`
    // decodes as a plausible empty object.
    //
    // (The old layout rejected it for an incidental reason: `shape` sat at
    // +12, so `obj + 8` put the field count on the kind byte, and a count of 2
    // exceeds `ObjectKind::Array`. A negative control that depended on a field
    // count not colliding with a kind tag was never testing the invariant it
    // claimed to.)
    //
    // When the caller has a walk, the base list is authoritative for the span
    // it covers, so an address inside that span that is not a base is interior
    // by construction -- whatever its bytes say. Marking one writes a mark bit
    // into a live object's payload, which is the corruption this screen exists
    // to stop. Outside the walked span, and when there is no walk at all, the
    // screen remains the only evidence available and still decides.
    let addr = ptr as usize;
    // gen r5w5/old9: `authoritative_base` is the match below, verbatim, for
    // the walked grid (`impl OldMarkGrid for [(*mut u8, usize)]`); the O(live)
    // oracle is authoritative everywhere in the generation.
    let interior_to_the_walk = walked.authoritative_base(addr) == Some(false);
    // What the walked form answers, and why:
    //
    //  match walked.first() {
    //   Bounded by the FIRST base only, not by the last: the walk is
    //   contiguous from there to the end of the used region, so every
    //   allocated byte at or above it belongs to some object the walk
    //   returned. Bounding by the last base instead reaches only the final
    //   object's own base address and misses its body entirely -- with a
    //   single walked object the span collapses to one point and catches
    //   nothing, which is exactly how the first attempt at this failed.
    //
    //   gen r4/mark (2026-09-23): "contiguous to the end of the used region"
    //   holds only while `OldGen::scan_region` never BREAKS. It does break,
    //   mid-region, on an implausible header (`SCAN_REGION_BREAK_HITS`,
    //   `WALK_DESYNC_HITS`), and every allocated object after the break is
    //   then absent from `walked`. A precise reference to one of those is
    //   rejected here as "interior", so the object is neither marked nor
    //   TRACED, and a walked child reachable only through it is freed by the
    //   in-place sweep. Not narrowed in-session: the tail object would then
    //   keep a stale `GC_FLAG_MARKED` (the sweep clears marks only on walked
    //   objects), which hides its children from the NEXT cycle instead. See
    //   `docs/feature-designs/generative-gc-architecture.md`.
    //    Some(&(lo_ptr, _)) => addr > lo_ptr as usize && !walked_grid_has_base(walked, addr),
    //    None => false,
    //  }
    if interior_to_the_walk {
        // gcj j3/og3: an unfollowed reference (see `OldGen::note_mark_gap_fact`).
        old_gen.note_mark_gap_fact();
        note_rejected_old_mark_candidate(ptr, old_gen, site);
        return;
    }
    if !old_gen_mark_candidate_plausible(ptr, old_gen, false) {
        if !rescue_mark_candidate_in(ptr, old_gen, walked) {
            old_gen.note_mark_gap_fact();
            note_rejected_old_mark_candidate(ptr, old_gen, site);
            return;
        }
        note_rescued_old_mark_candidate(ptr, site);
    }
    // SAFETY: the screen verified 8-alignment, old-gen containment and a
    // header whose claimed extent fits inside old gen.
    // t1 G2: a shared borrow and a CAS claim (`try_add_gc_flags`), not a
    // test-then-`fetch_or`: under `CRATONVM_GC_OLD_PAR_MARK` several mark
    // workers may reach the same object at once, and exactly one of them must
    // push it. Serially the two forms set the same bit and push the same set.
    let header = unsafe { &*(ptr as *const ObjectHeader) };
    if header.try_add_gc_flags(GC_FLAG_MARKED) {
        worklist.push(ptr);
        walked.note_marked(addr);
    } else if walked.retrace_stale_mark(addr) {
        worklist.push(ptr);
    }
}

