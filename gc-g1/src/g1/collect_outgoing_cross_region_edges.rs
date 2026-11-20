#[allow(unused_imports)]
use super::*;

impl G1Collector {
    /// gci i15/rs -- [`Self::collect_outgoing_cross_region_edges`], and with
    /// `young_of = Some(cset)` also answers whether the holder must keep its
    /// card YOUNG-dirty: a slot (read after this pause's rewrite) names
    /// another region that `young_card_target` counts as young, or a slot
    /// walk was clamped short of the declared count (not evidence of
    /// absence; a compact holder's walk is never clamped). `None`: `false`,
    /// and the edges and cards exactly as before.
    #[allow(clippy::too_many_arguments)]
    pub fn collect_outgoing_cross_region_edges_noting_young(
        &self,
        regions: &[G1Region],
        holder: usize,
        obj_ptr: *mut u8,
        header: &ObjectHeader,
        out: &mut Vec<(usize, usize)>,
        seen: &mut RegionSet,
        mut census: Option<&mut HumongousCensus>,
        young_of: Option<&RegionSet>,
    ) -> bool {
        let mut keeps_young = false;
        let mut clamped = false;
        let holder_span = humongous_span_start(regions, holder).unwrap_or(holder);
        let mut dirtied_header = false;
        let mut note = |target: usize| {
            if let Some(cset) = young_of {
                if target != holder
                    && young_card_target(regions[target].region_type(), cset.contains(&target))
                {
                    keeps_young = true;
                }
            }
            if let Some(span) = humongous_span_start(regions, target) {
                // A humongous target: NOT an rset edge (a humongous region can
                // never enter a collection set, so no pause would ever consult
                // the entry) but it is the one fact eager reclaim runs on.
                // References from a span INTO ITSELF are ignored — a
                // self-referential dead object must not be its own reason to
                // survive.
                if span != holder_span {
                    if let Some(c) = census.as_mut() {
                        c.referenced.insert(span);
                    }
                    // Ten-findings item 1: ALSO a remembered-set edge, on the
                    // span's start region. W5-B — from the holder's own header.
                    // At most once per object scan instead of once per slot.
                    if !dirtied_header {
                        dirtied_header = true;
                        self.cards
                            .dirty_addr_at_object_start(crate::g1_cards::ObjectStart::of_header(
                                header,
                            ));
                    }
                    if seen.insert(span) {
                        out.push((span, holder));
                    }
                }
            } else if target != holder && is_collectable_region_type(regions[target].region_type())
            {
                // F-05 — dirty the holder's card BEFORE the `seen` dedup,
                // once per holder object rather than once per element of a
                // humongous array.
                if !dirtied_header {
                    dirtied_header = true;
                    self.cards
                        .dirty_addr_at_object_start(crate::g1_cards::ObjectStart::of_header(
                            header,
                        ));
                }
                // G1AUD-9: emit each `(target, holder)` edge ONCE per holder
                // region — see `seen_targets` at the caller.
                if seen.insert(target) {
                    out.push((target, holder));
                }
            }
        };

        // LANE A — CLAMP BOTH WALKS TO THE HOLDER'S OWN REGION.
        //
        // `record_outgoing_rset_edges` already makes this argument in full and
        // names the three flat walks it applies to; this was the last of the
        // three still trusting the header's own count. The caller has validated
        // that `obj_ptr` is a plausible object, which bounds its ALIGNMENT and
        // its CONTAINMENT — not `HEADER_SIZE + num_slots * SLOT_SIZE`, and not
        // `HEADER_SIZE + array_length * 8`. Neither `num_slots` nor
        // `array_length` is bounded by anything but the `shape` dword they
        // share, so a torn or overwritten header sends the walk off the end of
        // the region and into whatever the arena holds next — the shape of the
        // measured H2 crash recorded at `record_outgoing_rset_edges`.
        //
        // `holder_walkable_slots` steps forward across HumongousContinuation
        // slices, so a legitimately huge array is not clamped; a clamp here
        // means the header is lying, and the slots past it are not part of this
        // object at all. Unlike the twin in `scan_source_region_for_cset_refs`
        // there is nothing to keep dirty on a refusal: this function only ADDS
        // rset edges and dirties cards, so refusing a slot leaves the card in
        // whatever state its real producer left it.
        let data_start = unsafe { obj_ptr.add(ARRAY_DATA_OFFSET) };
        if header.kind() == ObjectKind::Array {
            if header.element_type() == ArrayElementType::Reference {
                let walkable =
                    self.holder_walkable_slots(regions, obj_ptr, header.array_length() as usize, 8);
                // gci i15/rs: a clamped walk keeps the young card.
                clamped = walkable < header.array_length() as usize;
                for k in 0..walkable {
                    let raw: u64 = unsafe { std::ptr::read(data_start.add(k * 8) as *const u64) };
                    if raw == 0 {
                        continue;
                    }
                    if let Some(j) = self.lookup_region_for_addr(raw as usize) {
                        note(j);
                    }
                }
            }
        } else {
            let walkable = self.holder_walkable_slots(
                regions,
                obj_ptr,
                header.num_slots() as usize,
                SLOT_SIZE,
            );
            // gci i15/rs: as the source walk's twin -- not for a COMPACT
            // holder, whose walk ignores the cap and visits every field.
            clamped = walkable < header.num_slots() as usize
                && !cratonvm_types::is_compact_object(header);
            for_each_flat_object_reference_capped(obj_ptr, header, 0, walkable, |_, raw, _| {
                if let Some(j) = self.lookup_region_for_addr(raw) {
                    note(j);
                }
            });
        }
        young_of.is_some() && (keeps_young || clamped)
    }

    /// G1AUD-11 — post-pause remembered-set COMPLETENESS check
    /// (`CRATONVM_G1_DBG_RSET`).
    ///
    /// `verify_no_dangling_into_cset` answers "did this pause leave a stale
    /// pointer?" — a question about the pause that just ran. This answers the
    /// other half: "will the NEXT pause know where to look?" Every cross-region
    /// reference from a live region `H` into a collectable region `T` must be
    /// named in `T`'s remembered set, or the pause that collects `T` never
    /// scans `H` and frees an object `H` still points at.
    ///
    /// That is the invariant the Phase-4 rebuild maintains, and it is the one
    /// narrowing the walk could break. It is checked here rather than in a unit
    /// test because the unit suite cannot discriminate: on every constructible
    /// fixture the mutator barrier alone already records every edge, so a
    /// Phase-4 walk that visits NOTHING still passes. The shape that needs the
    /// rebuild — a pair stored while both ends shared one Eden region, so no
    /// cross-region barrier fired, which promotion then splits across two Old
    /// regions — is documented as observed on real workloads and resisted
    /// construction in a test. So the check runs against real programs instead.
    ///
    /// Whole-heap and O(live bytes): a diagnostic, never a shipping default.
    /// Reports and continues rather than aborting, so one violation does not
    /// hide the rest.
    pub fn dbg_verify_rset_completeness(&self, regions: &[G1Region], site: &'static str) {
        if !gc_flags().g1_dbg_rset {
            return;
        }
        let census = self.rset_edge_census(regions, site);
        // Printed even when clean, and with the DENOMINATOR: "no violations"
        // over zero examined edges is what a vacuous check looks like, and this
        // check has already been vacuous once in a unit fixture.
        //
        // W3-B — AND WITH THE SHAPE, for the same reason the denominator is
        // here. `edges=13583 missing=0` was the answer this census gave on
        // `G1RsetWideProbe`, a probe written to produce thousands of
        // old-to-young remembered-set SOURCES, while the pause reported
        // `rset_sources=1`. Those two numbers are only reconcilable if almost
        // none of those 13583 edges is old-to-young — which is exactly what the
        // breakdown below shows, and which no count of "edges" could have said.
        // A young pause walks a source region only for an edge whose source is
        // OUTSIDE the collection set; a young-to-young edge needs no entry
        // because young is collected whole. So `o2y_sources` is the number the
        // pause's `rset_sources=` should be compared against, and any gap
        // between them is the filtering this census exists to make visible.
        eprintln!(
            "[g1][RSET-CHECK] {site}: edges={} missing={} o2y={} o2y_sources={} \
             y2y={} o2o={} y2o={}",
            census.examined,
            census.missing,
            census.old_to_young,
            census.old_to_young_sources.len(),
            census.young_to_young,
            census.old_to_old,
            census.young_to_old,
        );
    }

    /// The walk itself, returning `(edges_examined, edges_missing)`.
    ///
    /// Split from the flag-gated, printing wrapper so a unit test can prove
    /// this code CAN report a violation. `missing=0` from a checker that is
    /// incapable of returning anything else is not evidence, and the whole
    /// reason this exists is that the unit suite could not tell a correct
    /// Phase-4 narrowing from one that walked nothing.
    pub fn rset_completeness_counts(&self, regions: &[G1Region], site: &str) -> (usize, usize) {
        let census = self.rset_edge_census(regions, site);
        (census.examined, census.missing)
    }

    /// W3-B — the same whole-heap walk, keeping the SHAPE of what it found.
    ///
    /// See [`RsetEdgeCensus`] for why the shape is the load-bearing part and a
    /// bare edge count is not.
    pub fn rset_edge_census(&self, regions: &[G1Region], site: &str) -> RsetEdgeCensus {
        let mut examined = 0usize;
        let mut missing = 0usize;
        // W3-B - the shape accumulators. Kept as locals rather than fields of a
        // borrowed struct because the per-slot `check` closure below already
        // borrows several of them at once.
        let mut old_to_young = 0usize;
        let mut young_to_young = 0usize;
        let mut old_to_old = 0usize;
        let mut young_to_old = 0usize;
        let mut old_to_young_sources = RegionSet::new();
        // INT-3 - a frozen peer's un-retired TLAB tail is reserved,
        // UNINITIALIZED memory below the region cursor and carries no walkable
        // filler, so it has to be stepped over BEFORE any byte of it is read as
        // a header (its random tail bytes can even alias `GAP_FILLER_CLASS_ID`).
        // Every other linear walker in this file takes the list; this one did
        // not, so under `CRATONVM_G1_DBG_RSET=1` a takeover pause made the
        // checker report a desync of its own making.
        //
        // Hoisted out of the region loop by dev; the body used to re-take the
        // mutex and clone the Vec once per region.
        let jit_skips = self.jit_tlab_skip_spans();
        for h in 0..regions.len() {
            if regions[h].region_type() == RegionType::Free {
                continue;
            }
            let base = regions[h].data.as_ptr() as usize;
            let cursor = regions[h].cursor();
            // LANE A — skip the frozen-peer TLAB tails, exactly as the two
            // production walks do. This walk had neither that skip nor a
            // header validation, so on a heap with a published un-retired tail
            // it interpreted uninitialized bytes as an object and either
            // reported nonsense violations or walked off the grid. A
            // DIAGNOSTIC that crashes the run it was switched on to diagnose is
            // worse than no diagnostic. (gci i10/pp: the list is the one hoisted
            // above the loop; a per-region re-take that shadowed it is gone.)
            let mut offset = 0usize;
            while offset < cursor {
                let obj_ptr = (base + offset) as *mut u8;
                if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, obj_ptr as usize) {
                    offset += skip;
                    continue;
                }
                if let Some(gap) = gap_filler_len(obj_ptr) {
                    offset += gap;
                    continue;
                }
                // LANE A — validate before decoding; see the identical guard in
                // `scan_source_region_for_cset_refs` for why a size test alone
                // does not catch a zeroed header.
                if !self.candidate_header_is_plausible(regions, obj_ptr as usize) {
                    break;
                }
                let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                if is_humongous_filler(header) {
                    break;
                }
                let size = object_total_size(header);
                if size < HEADER_SIZE || offset + size > cursor {
                    break;
                }
                let mut check = |raw: usize| {
                    let Some(t) = self.lookup_region_for_addr(raw) else {
                        return;
                    };
                    if t == h || !is_collectable_region_type(regions[t].region_type()) {
                        return;
                    }
                    examined += 1;
                    // W3-B — classify the edge before judging it.
                    //
                    // "Old" here means "not in a young generation", which is the
                    // property that decides whether a young pause has to walk
                    // the source at all: a young-to-young edge is covered by
                    // collecting young wholesale, so the remembered set neither
                    // holds it nor needs to. Only the `o2y` column can ever turn
                    // into an `rset_sources=` entry for a young pause, and only
                    // `o2y_sources` bounds how many.
                    let src_young = matches!(
                        regions[h].region_type(),
                        RegionType::Eden | RegionType::Survivor
                    );
                    let dst_young = matches!(
                        regions[t].region_type(),
                        RegionType::Eden | RegionType::Survivor
                    );
                    match (src_young, dst_young) {
                        (false, true) => {
                            old_to_young += 1;
                            old_to_young_sources.insert(h);
                        }
                        (true, true) => young_to_young += 1,
                        (false, false) => old_to_old += 1,
                        (true, false) => young_to_old += 1,
                    }
                    // LANE A — `names_source` is an O(1) probe under the rset's
                    // own mutex. This was `sources_with_generations()` followed
                    // by a linear `any`, i.e. an owned `Vec` of every entry in
                    // the target's remembered set allocated PER REFERENCE SLOT
                    // IN THE HEAP. On a heap where the rsets are large — which
                    // is the only heap whose completeness is in doubt — that is
                    // O(live slots x rset size) of allocation, and it is why
                    // `CRATONVM_G1_DBG_RSET` was unusable on anything real.
                    // `names_source` answers `true` for a coarsened rset, which
                    // subsumes the explicit check that used to precede it.
                    let named = regions[t].rset.names_source(h);
                    // LANE A — A STALE ENTRY IS A MISSING ENTRY. `live_rset_sources`
                    // DROPS an entry whose source was recycled after it was
                    // recorded, so a target whose rset names `h` only with an
                    // out-of-generation stamp will not cause `h` to be scanned.
                    // The check used to accept any stamp and so could not see
                    // the one failure mode the generation stamp introduced.
                    // (A genuine live edge stored after the recycle carries the
                    // newer generation, because `add_reference_in_generation`
                    // keeps the max — so a stale stamp here really does mean the
                    // edge is unrecorded.)
                    let stale = !regions[t].rset.is_coarsened()
                        && regions[t]
                            .rset
                            .recorded_generation(h)
                            .is_some_and(|g| Self::rset_entry_is_stale(regions, h, g));
                    if !named || stale {
                        missing += 1;
                        if missing <= 8 {
                            tracing::warn!(
                                "[g1][RSET-INCOMPLETE] {site}: region {h} ({:?}) object \
                                 0x{:x} references 0x{raw:x} in region {t} ({:?}), but \
                                 region {t}'s remembered set does not name {h} \
                                 (named={named} stale_stamp={stale}) — the next \
                                 pause that collects {t} will not scan {h}",
                                regions[h].region_type(),
                                obj_ptr as usize,
                                regions[t].region_type(),
                            );
                        }
                    }
                };
                // LANE A — the same region clamp the production walks have.
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
                            let raw: u64 = unsafe { std::ptr::read(data.add(k * 8) as *const u64) };
                            if raw != 0 {
                                check(raw as usize);
                            }
                        }
                    }
                } else {
                    let walkable = self.holder_walkable_slots(
                        regions,
                        obj_ptr,
                        header.num_slots() as usize,
                        SLOT_SIZE,
                    );
                    for_each_flat_object_reference_capped(
                        obj_ptr as *mut u8,
                        header,
                        0,
                        walkable,
                        |_, raw, _| check(raw),
                    );
                }
                offset += size;
            }
        }
        RsetEdgeCensus {
            examined,
            missing,
            old_to_young,
            young_to_young,
            old_to_old,
            young_to_old,
            old_to_young_sources,
        }
    }

    /// SECURITY FIX (V7b): defensive post-evacuation dangling-pointer
    /// verification.
    ///
    /// Remembered-set completeness is a hard UAF precondition: Phase-2
    /// only evacuates objects reachable from CSet rset sources, and
    /// Phase-5 then frees (resets) every CSet region. If a live
    /// cross-region edge into the CSet was missing from some rset, the
    /// referent is neither evacuated nor entered into `pointer_map`, so
    /// Phase-4 leaves the referring slot untouched — a dangling pointer
    /// into a region whose backing buffer was just zero-filled and will
    /// be re-typed for unrelated objects (classic UAF).
    ///
    /// This pass runs AFTER Phase-5 over every surviving (non-CSet,
    /// non-Free) region and inspects each reference slot. A slot whose
    /// target lands inside a CSet region (those addresses still map to
    /// the same region indices — `reset` only zero-fills, it never
    /// reallocates the buffer) but is NOT a key in `pointer_map` is a
    /// candidate dangling reference. Because a linear region walk also sees
    /// dead objects, the candidate is reported only after a root walk proves
    /// its holder reachable; then it is a dangling reference into a freed
    /// region. Rather than silently leave it dangling we abort (debug builds)
    /// / log (release builds).
    ///
    /// Cost: an extra walk of survivor/old regions. The WHOLE walk runs under
    /// `debug_assertions` (or `--verbose:gc` with
    /// `CRATONVM_G1_VERIFY_UNBOUNDED_ON_VERBOSE_GC=1`, gci i7/g1y); every other
    /// pause runs the budgeted sampler described in the body
    /// (`CRATONVM_G1_VERIFY_BUDGET=0` skips it).
    pub fn verify_no_dangling_into_cset(
        &self,
        regions: &[G1Region],
        cset: &RegionSet,
        pointer_map: &cratonvm_types::PointerMap,
        roots: &[ObjectRef],
    ) {
        // I-6 COVERAGE (audit §9 item 2). This is the only direct check that
        // the remembered set was complete enough for the pause that just ran,
        // and it is the check that caught G1-9. It used to run ONLY under
        // `debug_assertions` or the verify flag — i.e. never in a shipping
        // build — so the invariant it guards was a review claim everywhere it
        // actually mattered.
        //
        // It now always runs, at one of two budgets:
        //
        //   * UNBOUNDED, when `debug_assertions` or the verify flag is on:
        //     every object of every surviving region, exactly as before. Tests
        //     and verify runs keep their whole-heap guarantee.
        //   * BUDGETED otherwise: at most `verify_budget()` objects, starting
        //     from a cursor that ROTATES across pauses, so consecutive pauses
        //     sweep different regions and coverage accumulates over a run
        //     instead of re-checking the same prefix forever.
        //
        // The budgeted pass is a sampler, and the metrics say so: a pause that
        // stops on its budget is counted as `truncated`, and the objects it
        // did walk are published, because "no dangling references found" is
        // only meaningful against the number of objects that statement covers.
        // Set `CRATONVM_G1_VERIFY_BUDGET=0` to opt out entirely.
        //
        // LANE W4-B — A TRAP IN THAT FIRST BULLET, worth stating where the
        // line is drawn rather than where it is read.
        //
        // `gc_log_enabled` is `--verbose:gc`, and `--verbose:gc` is also how
        // most people make the `[GC] g1 cset-verify:` census print. So the
        // ordinary way to LOOK at the sampler is the way that switches the
        // sampler off in favour of the whole-heap sweep, and the census then
        // reports `budget_truncated=0` — which reads as "the budget was never
        // reached" and means "there was no budget". The same run, same binary,
        // same workload:
        //
        //   `--verbose:gc`        -> pauses=5 objects=6 547 331 budget_truncated=0
        //   `CRATONVM_GC_STATS=1` -> pauses=6 objects=24 576    budget_truncated=6
        //
        // A factor of 311 in what the verifier covered, selected by which flag
        // was used to ask. `gc_stats_requested()` in `vm-cli` is the other
        // door to the same census and does NOT set `gc_log_enabled`, so it is
        // the one that reports the shipped configuration. This is section 7.1
        // of `orchestrator-wave-1-measurements.md` in a new place: the switch
        // that displays the number also changes it.
        //
        // gci i7/g1y — and `--verbose:gc` no longer selects it. On G1 4-thread
        // javac at -Xmx512m (vi5g-g1f J, `--verbose:gc`) the whole-heap sweep
        // was `verify_us` ~96 ms of a ~200-255 ms pause, growing with the old
        // generation (311 ms per pause at -Xmx1g), and `update_young_target`
        // read that cost as "young is too big for the goal" and pinned the
        // target at its floor: the switch that displays the pause also made
        // it, and shaped the young generation. HotSpot's `-Xlog:gc` verifies
        // nothing. The budgeted sampler (the shipped configuration) still runs
        // under `--verbose:gc`; `CRATONVM_G1_VERIFY_UNBOUNDED_ON_VERBOSE_GC=1`
        // restores the old coupling. Debug builds stay unbounded.
        let unbounded = verify_unbounded_for(
            cfg!(debug_assertions),
            self.gc_log_enabled.load(Ordering::Relaxed),
            self.verify_unbounded_on_verbose_gc,
        );
        let budget = if unbounded {
            usize::MAX
        } else {
            verify_budget()
        };
        if budget == 0 {
            return;
        }
        // LANE W4-B — the second, heap-scaled bound. See [`verify_sweep_pauses`]
        // for the coverage measurements that say why a flat object budget
        // stops meaning anything above a few hundred megabytes.
        //
        // `walkable_bytes` is the population this pass samples FROM: the
        // occupied prefix of every region the walk below will actually enter.
        // Computed here, from the same predicate the walk uses, so the target
        // period is a period over the thing being swept and not over the heap
        // an operator asked for — a 32 GiB `-Xmx` holding 200 MiB of live data
        // must not buy a budget sized for 32 GiB.
        //
        // MEASURED, and it changed the design: the two bounds must not both
        // apply. With the flat object budget still in force,
        // `SWEEP_PAUSES=64` on the reproducer made coverage go DOWN — 2 731
        // objects per pause against the flat budget's 4 096 — because the
        // derived byte budget was the tighter of the two, while
        // `SWEEP_PAUSES=8` changed nothing at all because the object budget
        // was. So the flag could lower coverage and could not raise it, which
        // is the exact opposite of the thing it is named for. An operator who
        // states a sweep period has stated their intent; a second ceiling that
        // silently overrides it in one direction only is not a safety
        // mechanism, it is a bug with a comment.
        //
        // The byte budget is a bound BY CONSTRUCTION — it is `walkable / n`,
        // a fixed fraction of the occupied heap — so it does not need the
        // object budget underneath it to stay finite. The object budget
        // remains the whole story when `SWEEP_PAUSES` is unset, which is the
        // default and therefore today's behaviour unchanged.
        let (budget, byte_budget) = match verify_sweep_pauses() {
            0 => (budget, usize::MAX),
            _ if unbounded => (budget, usize::MAX),
            n => {
                let walkable: usize = regions
                    .iter()
                    .enumerate()
                    .filter(|(i, r)| !cset.contains(i) && r.region_type() != RegionType::Free)
                    .map(|(_, r)| r.cursor())
                    .sum();
                // A floor of one region's worth, so a tiny or nearly-empty
                // heap still makes real progress per pause instead of
                // truncating after a handful of objects — the failure mode a
                // divisor alone produces, and the one a flat budget was right
                // about.
                (
                    usize::MAX,
                    (walkable / n.max(1)).max(self.config.region_size),
                )
            }
        };
        self.verify_no_dangling_into_cset_within(
            regions,
            cset,
            pointer_map,
            roots,
            budget,
            byte_budget,
        );
    }

    /// Trace the graph reachable from the pause roots: the independent walk
    /// of the debug oracle [`Self::debug_assert_no_reference_into_spans`].
    /// Not a liveness proof a collection may act on -- it follows object
    /// fields only, not the class-loader side edges -- and only published
    /// object starts are followed (a root word that is not one is skipped).
    ///
    /// `extra_roots` names collector-owned roots which are not represented in
    /// the VM's ordinary root slice (for example SATB keep-alives and pending
    /// finalizers).  They are addresses rather than `ObjectRef`s because the
    /// collector stores them that way.
    #[cfg(debug_assertions)]
    pub fn reachable_object_addresses_from_roots(
        &self,
        regions: &[G1Region],
        roots: &[ObjectRef],
        extra_roots: &[usize],
        require_completed_mark_liveness: bool,
    ) -> std::collections::HashSet<usize> {
        // A complete mark snapshot is a stronger liveness oracle than an
        // arbitrary word in a conservative root snapshot.  In particular, a
        // stale frame word can still fall in a live region and otherwise make
        // a dropped reference array look reachable, causing Phase 2 to copy
        // the array's dead subtree.  Keep the snapshot optional because the
        // post-evacuation verifier also uses this walker after its source
        // regions may have been reset.
        let mark_snapshot =
            require_completed_mark_liveness.then(|| self.mark_start_snapshot.lock().clone());
        let live_extent = |addr: usize| -> Option<usize> {
            if addr == 0 || addr & 0x7 != 0 {
                return None;
            }
            let idx = self.lookup_region_for_addr(addr)?;
            let region = regions.get(idx)?;
            if region.region_type() == RegionType::Free {
                return None;
            }
            let base = region.data.as_ptr() as usize;
            let offset = addr.checked_sub(base)?;
            if !self.mark_root_is_object_start(regions, addr) {
                return None;
            }
            let header = unsafe { &*(addr as *const ObjectHeader) };
            let size = object_total_size(header);
            if size < HEADER_SIZE {
                return None;
            }
            if region.region_type() == RegionType::HumongousStart {
                if offset != 0 {
                    return None;
                }
            } else if offset.checked_add(size)? > region.cursor() {
                return None;
            }
            if let Some(snapshot) = &mark_snapshot {
                match snapshot.get(idx) {
                    Some(&(epoch, tams, region_type))
                        if epoch == region.reuse_epoch
                            && region_type == region.region_type()
                            && offset < tams
                            && !region.mark_bitmap.is_marked(addr) =>
                    {
                        return None;
                    }
                    _ => {}
                }
            }
            Some(size)
        };

        let mut seen = std::collections::HashSet::new();
        let mut work = Vec::new();
        for root in roots {
            let addr = root.as_ptr() as usize;
            if live_extent(addr).is_some() && seen.insert(addr) {
                work.push(addr);
            }
        }
        for &addr in extra_roots {
            if live_extent(addr).is_some() && seen.insert(addr) {
                work.push(addr);
            }
        }
        while let Some(addr) = work.pop() {
            let Some(extent) = live_extent(addr) else {
                continue;
            };
            let header = unsafe { &*(addr as *const ObjectHeader) };
            let mut visit = |raw: usize| {
                if live_extent(raw).is_some() && seen.insert(raw) {
                    work.push(raw);
                }
            };
            if header.kind() == ObjectKind::Array {
                if header.element_type() == ArrayElementType::Reference {
                    let data = unsafe { (addr as *const u8).add(ARRAY_DATA_OFFSET) };
                    let capacity = extent.saturating_sub(ARRAY_DATA_OFFSET) / 8;
                    for slot in 0..(header.array_length() as usize).min(capacity) {
                        let raw = unsafe { std::ptr::read(data.add(slot * 8) as *const u64) };
                        visit(raw as usize);
                    }
                }
            } else {
                for_each_flat_object_reference_trusting_header(
                    addr as *mut u8,
                    header,
                    0,
                    |_, raw, _| visit(raw),
                );
            }
        }
        seen
    }

    /// Is `wanted` reachable from one of the VM roots after this evacuation?
    ///
    /// The post-evacuation verifier walks every surviving region, including
    /// floating garbage. A stale slot in an unreachable holder is not a UAF:
    /// both objects died in the collection and the holder will never be read.
    /// This bounded-on-demand graph walk is therefore deliberately paid only
    /// after the cheap linear verifier finds a candidate dangling slot.
    pub fn reachable_from_roots_after_evacuation(
        &self,
        regions: &[G1Region],
        _roots: &[ObjectRef],
        wanted: usize,
    ) -> bool {
        // A holder the last trustworthy completed mark proves dead is floating
        // garbage: its stale slot is not a use-after-free, and the
        // allocation-pressure source walk skips exactly such holders. Anything
        // else is treated as reachable and reported.
        let Some(proof) = self.low_space_rset_source_liveness(regions) else {
            return true;
        };
        let Some(idx) = self.lookup_region_for_addr(wanted) else {
            return true;
        };
        regions
            .get(idx)
            .is_none_or(|r| !proof.proves_dead(r, idx, wanted))
    }

    /// The holders a completed mark proves dead, for the remembered-set source
    /// walk of an allocation-pressure pause; see [`MarkProvenDead`].
    ///
    /// `None` -- keep the region-level remembered-set contract, every holder in
    /// a source region scanned -- unless the last cycle completed, its
    /// `cleanup` trusted its closure (`last_mark_trusted`), and no new cycle is
    /// running (its bitmaps are being rebuilt).
    ///
    /// This replaced a trace of the pause's root slice (2026-09-30). That trace
    /// was an unsound liveness oracle: it followed object fields only, not the
    /// class-loader side edges the marker follows, and it took every root word
    /// as an object start. Holders it missed lost their young referents, and
    /// the root words that were not object starts drove a legacy-cell walk
    /// over zeroed or foreign memory -- the `g1::for_each_object_reference:
    /// corrupt Value cell` bursts of `UncaughtExitUnderGcProbe` (32 in 2 of 2
    /// runs). A completed mark is the evidence `cleanup` already frees whole
    /// regions on, and it needs no walk.
    pub fn low_space_rset_source_liveness(&self, regions: &[G1Region]) -> Option<MarkProvenDead> {
        if !g1_rset_dead_holder_filter() {
            return None;
        }
        self.mark_proven_dead(regions)
    }

    /// gci i14/hd (split out of [`Self::low_space_rset_source_liveness`],
    /// unchanged): the [`MarkProvenDead`] of the last completed, trusted mark
    /// with no cycle running, whatever `CRATONVM_G1_RSET_DEAD_HOLDER_FILTER`
    /// says -- also the oracle of [`Self::for_each_object_skipping_proven_dead`].
    pub fn mark_proven_dead(&self, regions: &[G1Region]) -> Option<MarkProvenDead> {
        // gci i18/nr (i17/nr N1): the proof ignores JIT pins. It is sound
        // only when the remark marked every object a compiled frame pins
        // (both switches ON, the default; the condition cleanup's bitmap
        // scrub arm uses for a JIT-pinned region). With either OFF an object
        // only a compiled frame keeps reads unmarked, and the filter would
        // skip -- and since i17/nr null -- a LIVE holder: no proof then.
        if !mark_proof_covers_jit_pins(g1_mark_root_screen(), true) {
            return None;
        }
        if self.gc_state.is_marking_active()
            || !self.marking_complete.load(Ordering::Acquire)
            || !self.last_mark_trusted.load(Ordering::Acquire)
        {
            return None;
        }
        let snapshot = self.mark_start_snapshot.lock().clone();
        if snapshot.len() != regions.len() {
            return None;
        }
        let mut proof = MarkProvenDead {
            snapshot,
            keep: FxHashSet::default(),
        };
        // A registered finalizable object the mark found dead is resurrected
        // when finalization runs, and so is everything it reaches. Keep that
        // closure out of the filter.
        let finalizer_roots = self.pending_finalizer_roots.lock().clone();
        let mut work: Vec<usize> = Vec::new();
        for addr in finalizer_roots {
            if self.proven_dead_object_start(regions, &proof, addr) && proof.keep.insert(addr) {
                work.push(addr);
            }
        }
        while let Some(addr) = work.pop() {
            // SAFETY: `proven_dead_object_start` placed a published header at
            // `addr` inside an Old region's allocated prefix.
            let header = unsafe { &*(addr as *const ObjectHeader) };
            let mut children = Vec::new();
            if header.kind() == ObjectKind::Array {
                if header.element_type() == ArrayElementType::Reference {
                    // SAFETY: the gray gate bounded the array inside the
                    // region's allocated prefix.
                    let data = unsafe { (addr as *const u8).add(ARRAY_DATA_OFFSET) };
                    for i in 0..header.array_length() as usize {
                        children.push(unsafe { std::ptr::read(data.add(i * 8) as *const u64) } as usize);
                    }
                }
            } else {
                for_each_flat_object_reference_trusting_header(
                    addr as *mut u8,
                    header,
                    0,
                    |_, raw, _| children.push(raw),
                );
            }
            for child in children {
                if self.proven_dead_object_start(regions, &proof, child) && proof.keep.insert(child)
                {
                    work.push(child);
                }
            }
        }
        Some(proof)
    }

    /// Whether `addr` is a published object start in an Old region that
    /// `proof` proves dead, i.e. one [`Self::low_space_rset_source_liveness`]
    /// may have to keep when finalization can resurrect it.
    pub fn proven_dead_object_start(
        &self,
        regions: &[G1Region],
        proof: &MarkProvenDead,
        addr: usize,
    ) -> bool {
        let Some(idx) = self.lookup_region_for_addr(addr) else {
            return false;
        };
        let Some(region) = regions.get(idx) else {
            return false;
        };
        if region.region_type() != RegionType::Old || !proof.proves_dead(region, idx, addr) {
            return false;
        }
        let flagless = AtomicU64::new(0);
        classify_mark_scan_target(region, addr, &flagless).is_ok()
    }

    /// The walk itself, with the object budget as a PARAMETER rather than a
    /// build-configuration decision.
    ///
    /// Split out so the budgeted arm is testable. The wrapper's budget is
    /// `usize::MAX` under `debug_assertions`, which is every unit-test build,
    /// so a test that goes through the wrapper can only ever exercise the
    /// unbounded sweep — it would assert on the sampler's rotation without
    /// running the sampler. Tests call this directly with a small budget.
    pub fn verify_no_dangling_into_cset_within(
        &self,
        regions: &[G1Region],
        cset: &RegionSet,
        pointer_map: &cratonvm_types::PointerMap,
        roots: &[ObjectRef],

        budget: usize,
        byte_budget: usize,
    ) {
        // Both MAX, not either: under `CRATONVM_G1_VERIFY_SWEEP_PAUSES` the
        // object budget is deliberately `usize::MAX` while the byte budget
        // bounds the pass, and that is a BUDGETED pass — it must still rotate
        // its cursor and must still be reported as budgeted.
        let unbounded = budget == usize::MAX && byte_budget == usize::MAX;
        let mut objects_walked = 0usize;
        // LANE W4-B — the second bound's accumulator. Bytes rather than
        // objects because a sweep PERIOD is a period over the occupied bytes
        // of the heap, which is what the walk advances through; see
        // [`verify_sweep_pauses`].
        let mut bytes_walked = 0usize;
        let mut dangling_found = 0u64;
        let mut truncated = false;
        // One root walk per holder at most. A reference array can carry many
        // stale slots after it becomes dead, and repeating the same reachability
        // proof for every element would turn a diagnostic into quadratic work.
        let mut holder_reachability: std::collections::HashMap<usize, bool> =
            std::collections::HashMap::new();

        // Lane B — the second verdict this walk can now return, and the reason
        // it had to grow one.
        //
        // The classifier below used to answer a single question: "does this
        // slot point into a freed CSet region with NO forwarding entry?" That
        // is the incomplete-remembered-set shape (the referent was never
        // reached, so it was never copied). It is not the only way a slot ends
        // up pointing into zero-filled memory, and it is emphatically not the
        // way THIS lane's work would break the heap.
        //
        // `phase4_regions_to_walk` narrows the fix-up to the regions that can
        // hold a slot into the collection set. If that set is ever wrong —
        // a region left out that did hold one — the referent is still copied
        // (some other holder reached it), it still gets a `pointer_map` entry,
        // and the missed slot is left holding the FROM-SPACE address. The old
        // classifier read `pointer_map.contains_key(addr)` as proof the slot
        // was fine and returned `false`: the one tripwire that is supposed to
        // guard the narrowing was structurally incapable of seeing a narrowing
        // bug. It would have reported the pause as clean.
        //
        // So a target that IS a forwarding key, whose value is a DIFFERENT
        // address, and whose region Phase 5 actually freed, is now its own
        // verdict. All three terms are load-bearing:
        //
        //   * `value != key` excludes self-forwards. An evacuation failure
        //     installs `k -> k` and `free_or_keep_cset` KEEPS that region; the
        //     object is alive exactly where the slot points.
        //   * "region is now `Free`" excludes every kept region for the same
        //     reason, and it is the test that makes the verdict mean "this
        //     address is zero-filled memory" rather than "this address is
        //     stale". A kept region's from-space copy of a moved object is a
        //     real defect too, but it is a different one with a different
        //     producer, and mixing them would make this counter unreadable.
        //   * the 8-alignment screen above still applies, for its own reasons.
        //
        // REPORT-ONLY, and deliberately not a `debug_assert`. The dangling
        // verdict aborts debug builds because it is a proven UAF on a
        // definitely-live path. This one is proven stale but not proven LIVE:
        // this walk is linear over a surviving region, so it inspects dead
        // objects too, and a dead holder's un-rewritten slot is floating
        // garbage. Turning it into an abort would convert ordinary garbage
        // into a test failure; turning it into a counted, deduplicated report
        // makes "did narrowing miss a region" a number an operator can read.
        // `CRATONVM_G1_VERIFY_STALE_FORWARD=0` switches it off.
        let want_stale_forward = verify_stale_forward_enabled();

        // Closure: classify a referent address that lands in a CSet region.
        // `None` means the slot is fine.
        let classify = |addr: usize| -> Option<CsetSlotVerdict> {
            if addr == 0 {
                return None;
            }
            // NOT 8-ALIGNED IS NOT A REFERENCE. Every object start in this heap
            // is 8-aligned — `plausible_heap_pointer` and
            // `ObjectHeader::make_forwarded` both assert it — so a misaligned
            // word cannot be a reference the collector failed to remap. It is a
            // primitive, a stale slot, or a conservatively-discovered `long`,
            // and the evacuator has just DECLINED to follow it
            // (`evacuation_candidate_is_an_object`, both arms since
            // 2026-09-05). Counting a deliberate refusal as an "incomplete
            // remembered set => UAF" makes this verifier report the fix as the
            // defect.
            //
            // The screen is alignment ONLY, deliberately. The obvious stronger
            // one — `candidate_header_is_plausible` — would make the whole
            // check vacuous here: this walk runs AFTER Phase 5, so every CSet
            // region is already `Free` and the classifier answers `RegionFree`
            // for every address in one, including the genuinely dangling ones
            // this verifier exists to catch.
            if addr & 0x7 != 0 {
                return None;
            }
            let tgt_idx = self.lookup_region_for_addr(addr)?;
            if !cset.contains(&tgt_idx) {
                return None;
            }
            // Target sits in a CSet region. If it was properly evacuated it
            // would have a forwarding entry.
            match pointer_map.get(&addr) {
                None => Some(CsetSlotVerdict::Dangling),
                Some(&moved_to) => {
                    if !want_stale_forward
                        || moved_to == addr
                        || regions.get(tgt_idx).map(|r| r.region_type()) != Some(RegionType::Free)
                    {
                        return None;
                    }
                    Some(CsetSlotVerdict::StaleForward(moved_to))
                }
            }
        };

        let jit_skips = self.jit_tlab_skip_spans();
        // Rotate the starting region so a budgeted run does not verify the same
        // low-numbered regions every pause and call the heap checked. The
        // cursor advances by whatever this pass consumed, so over enough pauses
        // the sampler sweeps the whole heap.
        //
        // Lane B — the cursor now carries a BYTE OFFSET as well as a region
        // index, packed as `(offset << 32) | region`.
        //
        // The rotation alone does not accumulate coverage the way its comment
        // claims. The budget is counted in OBJECTS and the walk breaks
        // mid-region when it runs out, but the cursor it then stored was
        // `region + 1` — so the tail of that region was not "picked up by a
        // later pause", it was skipped, and the next pass to reach that region
        // started at offset 0 again and truncated at the same object. Any
        // region holding more than `verify_budget()` objects therefore had its
        // tail verified NEVER, for the whole life of the VM, while the metrics
        // reported a rotating sampler that sweeps the heap. A 1 MB region of
        // 32-byte objects holds 32 768 of them against a default budget of
        // 4 096, so this is the ordinary case, not a corner.
        //
        // Packing the resume offset into the existing word keeps this a
        // property of the verifier rather than new collector state (one atomic,
        // one owner, and `store(0)` on a complete sweep still means exactly
        // what it meant). A region whose cursor does not fit in 32 bits — only
        // a humongous span can be that large — falls back to the old
        // next-region behaviour rather than truncating the offset, because a
        // truncated offset would resume in the middle of an object.
        let nregions = regions.len();
        let packed = if unbounded {
            0
        } else {
            self.cset_verify_cursor.load(Ordering::Relaxed)
        };
        let start = (packed & 0xffff_ffff) % nregions.max(1);
        let resume_offset = packed >> 32;
        // gci i29/cv2 — the offset is an object start only in the incarnation
        // of region `start` it was recorded in. Read once, before the loop.
        let resume_stamp = self.cset_verify_resume_stamp.load(Ordering::Relaxed);
        for step in 0..nregions {
            let i = (start + step) % nregions;
            if cset.contains(&i) || regions[i].region_type() == RegionType::Free {
                continue;
            }
            if objects_walked >= budget || bytes_walked >= byte_budget {
                truncated = true;
                break;
            }

            let cursor = regions[i].cursor();
            let base = regions[i].data.as_ptr();
            // Lane B — resume where the previous budgeted pass ran out, but
            // only on the FIRST region this pass looks at and only if the
            // offset is still a plausible object start in it. A region that has
            // been reset, refilled or retyped since is walked from the top;
            // resuming into the middle of a region whose object grid has moved
            // would read a body word as a header.
            // `step == 0` says the stored offset belongs to region `start` and
            // to no other, and if `start` was skipped above (it is Free or in
            // this CSet now) every later step starts from the top by
            // construction.
            //
            // gci i29/cv2 (gci-i26cv) — but `step == 0` was NOT the whole
            // validity condition, although the comment above promised a
            // refilled region is walked from the top. Region `start` can be
            // freed and refilled BETWEEN two verifier passes: cleanup frees
            // all-dead Old regions after remark, and the very next (Mixed)
            // pause takes them from the free list as promotion destinations.
            // At this pass's end the region is Old again, not in the CSet, its
            // cursor is past the stale offset, and the offset lands inside an
            // object of the NEW grid -- the verifier then read a compact
            // object's body as a legacy header and strode it as 16-byte
            // `Value` cells (32 `corrupt Value cell` lines, vi26-cv C1 r2:
            // `caller=` this walk, holder `grid=INTERIOR`, region r29 Old
            // reuse_epoch 5, the first Mixed pause after `freed_regions=89`).
            // The stamp names the incarnation; a mismatch walks from the top.
            let mut offset = if step == 0
                && resume_offset > 0
                && resume_offset < cursor
                && resume_stamp == Self::cset_verify_resume_stamp_of(&regions[i])
            {
                resume_offset
            } else {
                0usize
            };

            while offset < cursor {
                let obj_ptr = unsafe { base.add(offset) };
                // INT-3 — frozen-peer TLAB tail: skip before interpreting.
                if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, obj_ptr as usize) {
                    offset += skip;
                    continue;
                }
                // TLAB-retire gap sentinel: skip its exact span.
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

                let data_start = unsafe { obj_ptr.add(ARRAY_DATA_OFFSET) };
                if header.kind() == ObjectKind::Array {
                    if header.element_type() == ArrayElementType::Reference {
                        for k in 0..header.array_length() as usize {
                            let slot_ptr = unsafe { data_start.add(k * 8) };
                            let raw: u64 = unsafe { std::ptr::read(slot_ptr as *const u64) };
                            if let Some(verdict) = classify(raw as usize) {
                                let reachable = *holder_reachability
                                    .entry(obj_ptr as usize)
                                    .or_insert_with(|| {
                                        self.reachable_from_roots_after_evacuation(
                                            regions,
                                            roots,
                                            obj_ptr as usize,
                                        )
                                    });
                                if !reachable {
                                    continue;
                                }
                                if verdict == CsetSlotVerdict::Dangling {
                                    dangling_found += 1;
                                }
                                self.report_dangling_cset_ref(
                                    i,
                                    obj_ptr as usize,
                                    raw as usize,
                                    regions[i].mark_bitmap.is_marked(obj_ptr as usize),
                                    verdict,
                                );
                            }
                        }
                    }
                } else {
                    let mut found = 0u64;
                    for_each_flat_object_reference_trusting_header(
                        obj_ptr as *mut u8,
                        header,
                        0,
                        |slot, raw, _| {
                            if let Some(verdict) = classify(raw) {
                                let reachable = *holder_reachability
                                    .entry(obj_ptr as usize)
                                    .or_insert_with(|| {
                                        self.reachable_from_roots_after_evacuation(
                                            regions,
                                            roots,
                                            obj_ptr as usize,
                                        )
                                    });
                                if !reachable {
                                    return;
                                }
                                if verdict == CsetSlotVerdict::Dangling {
                                    found += 1;
                                }

                                self.report_dangling_cset_ref(
                                    i,
                                    obj_ptr as usize,
                                    raw,
                                    regions[i].mark_bitmap.is_marked(obj_ptr as usize),
                                    verdict,
                                );
                            }
                        },
                    );
                    dangling_found += found;
                }

                objects_walked += 1;
                bytes_walked += obj_size;
                offset += obj_size;
                // Budget is counted in OBJECTS, not regions: one humongous
                // reference array can hold more slots than a thousand ordinary
                // objects, so a region-granular budget would have wildly
                // different costs per unit. Break mid-region rather than
                // overrun — and record WHERE, so the next pause picks the tail
                // up instead of skipping it (see the cursor's doc above).
                //
                // LANE W4-B — and the heap-scaled BYTE bound beside it. Both
                // are ceilings and either may bind; `budget_truncated` counts
                // a stop by either, because from the census's point of view
                // they are the same event ("this pass did not finish, and the
                // cursor says where to resume"). Which one bound is a tuning
                // question, and the two numbers that answer it —
                // `objects/pause` and the sweep period — are already on the
                // line.
                if objects_walked >= budget || bytes_walked >= byte_budget {
                    truncated = true;
                    break;
                }
            }
            if truncated {
                // Lane B — resume inside this region when there is a tail left
                // and the offset is representable; otherwise fall back to the
                // old "next region" behaviour, which is what "this region is
                // finished" means anyway.
                let packed =
                    if offset < cursor && offset <= u32::MAX as usize && i <= u32::MAX as usize {
                        (offset << 32) | i
                    } else {
                        i.wrapping_add(1)
                    };
                // gci i29/cv2 — stamp the incarnation the offset belongs to
                // (harmless on the next-region arm: its offset is 0).
                self.cset_verify_resume_stamp.store(
                    Self::cset_verify_resume_stamp_of(&regions[i]),
                    Ordering::Relaxed,
                );
                self.cset_verify_cursor.store(packed, Ordering::Relaxed);
                break;
            }
        }
        if !truncated {
            // A complete sweep: restart the rotation so the next budgeted pass
            // does not begin mid-heap for no reason.
            self.cset_verify_cursor.store(0, Ordering::Relaxed);
        }
        cratonvm_gc_core::gc_metrics::record_g1_cset_verify(
            objects_walked as u64,
            bytes_walked as u64,
            dangling_found,
            truncated,
            unbounded,
        );
    }

    /// gci i29/cv2 — the incarnation stamp the budgeted CSet verifier keys
    /// its resume offset on: the region's `reuse_epoch` (bumped by every
    /// [`G1Region::reset`], i.e. every free) with its type in the low byte (a
    /// retype without a reset does not move the grid, but restarting from the
    /// top there costs one region of coverage and needs no argument).
    #[inline]
    pub fn cset_verify_resume_stamp_of(region: &G1Region) -> u64 {
        (region.reuse_epoch << 8) | u64::from(region.region_type().to_u8())
    }

    /// SECURITY FIX (V7b): report a detected dangling-into-CSet slot.
    /// In debug builds this is a hard abort (the heap is corrupt and any
    /// further mutation risks a UAF); in release builds (reached only via
    /// the `gc_log_enabled` verify flag) it logs loudly so the condition
    /// is observable without crashing a production VM.
    ///
    /// Lane B — `verdict` splits the two shapes this walk can now find. See
    /// [`CsetSlotVerdict`]: `Dangling` is the original incomplete-remembered-set
    /// UAF and keeps the abort; `StaleForward` is a slot Phase 4 should have
    /// rewritten and did not, and is counted and logged but never aborts.
    #[cold]
    #[inline(never)]
    pub fn report_dangling_cset_ref(
        &self,
        holder_region: usize,
        holder_obj: usize,
        target: usize,
        marked: bool,
        verdict: CsetSlotVerdict,
    ) {
        if let CsetSlotVerdict::StaleForward(moved_to) = verdict {
            let n = PHASE4_STALE_FORWARD_SLOTS.fetch_add(1, Ordering::Relaxed) + 1;
            if !self.v7b_reported.lock().insert((holder_obj, target)) {
                return;
            }
            eprintln!(
                concat!(
                    "[g1][FIXUP-MISS] object {:#x} in surviving region {} still points at ",
                    "{:#x}, which this pause EVACUATED to {:#x} and then freed. The ",
                    "forwarding entry exists, so the referent was copied — what did not ",
                    "happen is the Phase-4 rewrite of THIS slot, which means the fix-up ",
                    "did not walk region {}. Check `phase4_regions_to_walk`: either the ",
                    "narrow set is missing a term, or the remembered set that feeds it ",
                    "is. holder_marked={} marking_active={} distinct_stale={}"
                ),
                holder_obj,
                holder_region,
                target,
                moved_to,
                holder_region,
                marked,
                self.gc_state.is_marking_active(),
                n,
            );
            return;
        }
        // ONE LINE PER DISTINCT PAIR. Everything past the first is a repeat:
        // the rotating budget re-walks a surviving region every few pauses and
        // an unrepaired slot is still unrepaired, so the raw line count
        // measures how often the sampler visited that region, not how much of
        // the heap is damaged. Read as a rate it is badly misleading -- the
        // 48 617 lines on `TestKillProcessWhileWriting` are SIX holders -- so
        // the running total is carried on the line instead of being implied by
        // it.
        static TOTAL: AtomicUsize = AtomicUsize::new(0);
        let total = TOTAL.fetch_add(1, Ordering::Relaxed) + 1;
        if !self.v7b_reported.lock().insert((holder_obj, target)) {
            return;
        }
        // WHETHER THE HOLDER IS REACHABLE is the whole question, and this
        // verifier cannot answer it: it walks a surviving region LINEARLY, so
        // it inspects dead objects too, and a dead object pointing at a dead
        // CSet object that was correctly not evacuated is not a UAF at all --
        // it is ordinary floating garbage. The mark bit is the closest thing
        // to an answer available here (meaningful only while a mark cycle has
        // marks), so it is reported rather than assumed, and `distinct` /
        // `total` say how much of the log is one slot.
        let distinct = self.v7b_reported.lock().len();
        eprintln!(
            concat!(
                "[g1][SECURITY V7b] post-evacuation dangling reference: object {:#x} ",
                "in surviving region {} still points at {:#x}, which lies in a freed ",
                "CSet region with no forwarding entry (incomplete remembered set => ",
                "UAF). holder_marked={} marking_active={} distinct={} total={}"
            ),
            holder_obj,
            holder_region,
            target,
            marked,
            self.gc_state.is_marking_active(),
            distinct,
            total,
        );
        debug_assert!(
            false,
            "G1 post-evacuation dangling reference into freed CSet region (V7b)"
        );
    }

    /// DIAGNOSTIC TOOL (parallel-evac defect 2, env `CRATONVM_G1_DBG_HEADERS=1`).
    /// Post-collection consistency verifier that ROOT-CAUSED the rare
    /// `young_collection_parallel` corruption (`SteadyChurn @16m --nojit`, ~1/8,
    /// `java/lang/Object`). Checks, after a parallel collection:
    ///   (0) OVERLAP — two from-space objects forwarded to the same dest (a TLAB
    ///       race) — NEVER fires (ruled out);
    ///   (a) UN-REWRITTEN — a non-CSet/root ref to a `pointer_map` KEY — NEVER
    ///       fires from heap holders (the heap is clean);
    ///   (b) LOST — a root → a CSet object NOT in `pointer_map` — fires EVERY
    ///       collection: the smoking gun.
    ///
    /// ROOT CAUSE (full writeup + ruled-out fixes:
    /// `g1-parallel-evac-persistent-forwarding-root-remap.md`):
    /// the parallel evacuator dedups via the PERSISTENT `forwarding_ptr` header
    /// field (serial uses the per-cycle `pointer_map`). A fast-path hit returns a
    /// forward — possibly left over from a PRIOR cycle — WITHOUT recording it in
    /// `pointer_map`, so the VM's `update_all_roots` cannot remap a root that
    /// resolved through it. The frame local stays stuck on the from-space object,
    /// surviving only via the `forwarding_ptr` redirect until its region is
    /// reused → corruption. Invisible to V7b (heap-only). Three naive fixes all
    /// fail (see the doc); the proper fix is a per-cycle forwarding redesign.
    ///
    /// Mixed GC is kept on the serial evacuator and parallel evac stays
    /// opt-in/experimental (`CRATONVM_G1_PARALLEL_EVAC`) until this is fixed.
    /// No-op unless the env knob is set.
    pub fn dbg_verify_no_unrewritten_forward(
        &self,
        regions: &[G1Region],
        cset_set: &RegionSet,
        pointer_map: &cratonvm_types::PointerMap,
        roots: &[ObjectRef],
    ) {
        if !gc_flags().g1_dbg_headers {
            return;
        }
        // (0) OVERLAP DETECTOR: two distinct from-space objects forwarded to the
        // SAME destination address = a TLAB allocation race (one copy clobbers
        // the other's header → `java/lang/Object`). Reverse-map the forwards.
        {
            let mut by_dest: cratonvm_types::PointerMap =
                cratonvm_types::PointerMap::with_capacity_and_hasher(
                    pointer_map.len(),
                    Default::default(),
                );
            let mut overlaps = 0usize;
            for (&k, &v) in pointer_map.iter() {
                if k == v {
                    continue; // self-forward (in place) — not a copy destination
                }
                if let Some(&prev) = by_dest.get(&v) {
                    overlaps += 1;
                    if overlaps <= 8 {
                        eprintln!(
                            "[g1][DBG-HEADERS] OVERLAP: dest {:#x} is the forward target of TWO \
                             from-space objects {:#x} and {:#x}",
                            v, prev, k
                        );
                    }
                } else {
                    by_dest.insert(v, k);
                }
            }
            if overlaps > 0 {
                eprintln!(
                    "[g1][DBG-HEADERS] {overlaps} forward-destination OVERLAP(s) this collection"
                );
            }
        }

        let mut hits = 0usize;
        // A holder is REACHABLE-RELEVANT only if it is NOT in a CSet (from-space)
        // region: kept CSet regions hold dead-never-reached from-space objects
        // whose slots are legitimately un-rewritten (noise). True survivors/old
        // (non-CSet) and roots MUST have every CSet ref rewritten (by the in-place
        // scan for new survivors, or Phase 4 for pre-existing survivors/old).
        let mut lost = 0usize;
        let mut check = |holder: usize, where_: &str, target: usize| {
            if let Some(&new) = pointer_map.get(&target) {
                if new != target {
                    hits += 1;
                    if hits <= 24 {
                        let treg = self.lookup_region_for_addr(target);
                        eprintln!(
                            "[g1][DBG-HEADERS] UN-REWRITTEN forward (live holder): {where_} \
                             holder={:#x} slot->{:#x} should be {:#x}; target region={:?}",
                            holder, target, new, treg
                        );
                    }
                }
                return;
            }
            // LOST: target sits in a CSet (collected) region but has NO forwarding
            // entry — it was never evacuated. V7b catches this for HEAP holders;
            // here it also covers ROOTS (which V7b never scans). A live object
            // reachable only via a root, dropped because the parallel closure
            // terminated before scanning it, lands here — and the VM's
            // `update_all_roots` cannot remap it (not in `pointer_map`), so the
            // frame local dangles into the freed region → `java/lang/Object`.
            if let Some(tidx) = self.lookup_region_for_addr(target) {
                if cset_set.contains(&tidx) {
                    lost += 1;
                    if lost <= 24 {
                        eprintln!(
                            "[g1][DBG-HEADERS] LOST (un-evacuated CSet target): {where_} \
                             holder={:#x} slot->{:#x} region={tidx} — NOT in pointer_map",
                            holder, target
                        );
                    }
                }
            }
        };
        // Roots (always reachable).
        for r in roots {
            let p = r.as_ptr() as usize;
            if p != 0 {
                check(0, "root", p);
            }
        }
        // Only NON-CSet regions (true survivors / old / new survivors). Kept CSet
        // regions are excluded — their dead objects are the stranded-copy noise.
        let jit_skips = self.jit_tlab_skip_spans();
        for (ridx, region) in regions.iter().enumerate() {
            if region.region_type() == RegionType::Free || cset_set.contains(&ridx) {
                continue;
            }
            let base = region.data.as_ptr();
            let cursor = region.cursor();
            let mut offset = 0usize;
            while offset < cursor {
                let obj_ptr = unsafe { base.add(offset) };
                // INT-3 — frozen-peer TLAB tail: skip before interpreting.
                if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, obj_ptr as usize) {
                    offset += skip;
                    continue;
                }
                // TLAB-retire gap sentinel: skip its exact span.
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
                let data = unsafe { obj_ptr.add(ARRAY_DATA_OFFSET) };
                if header.kind() == ObjectKind::Array {
                    if header.element_type() == ArrayElementType::Reference {
                        for k in 0..header.array_length() as usize {
                            let raw =
                                unsafe { std::ptr::read(data.add(k * 8) as *const u64) } as usize;
                            if raw != 0 {
                                check(obj_ptr as usize, "array-elem", raw);
                            }
                        }
                    }
                } else {
                    for_each_flat_object_reference_trusting_header(
                        obj_ptr as *mut u8,
                        header,
                        0,
                        |_, raw, _| {
                            check(obj_ptr as usize, "field", raw);
                        },
                    );
                }
                offset += obj_size;
            }
        }
        if hits > 0 || lost > 0 {
            eprintln!(
                "[g1][DBG-HEADERS] {hits} un-rewritten + {lost} LOST(un-evacuated CSet) \
                 from NON-CSET holders/roots = the real defect"
            );
        }
    }

    /// DIAGNOSTIC (defect-2 residual race, env `CRATONVM_G1_DBG_ZERO=1`): after a
    /// parallel collection, scan EVERY non-Free region (INCLUDING kept CSet
    /// regions, which the un-rewritten verifier excludes) plus the roots, and
    /// report any reference whose TARGET has an all-zero header (the freed/reused
    /// region signature behind `java/lang/Object`). Catches the residual
    /// concurrency-race corruption at the collection that introduces it,
    /// regardless of which region the holder lives in. No-op unless the env set.
    pub fn dbg_scan_for_zeroed_refs(
        &self,
        regions: &[G1Region],
        cset_set: &RegionSet,
        roots: &[ObjectRef],
    ) {
        if !gc_flags().g1_dbg_zero {
            return;
        }
        let mut hits = 0usize;
        let is_zeroed = |addr: usize| -> bool {
            if addr == 0 || self.lookup_region_for_addr(addr).is_none() {
                return false;
            }
            let h = unsafe { &*(addr as *const ObjectHeader) };
            header_reads_zeroed(h)
        };
        let mut report = |holder: usize, hreg: Option<usize>, where_: &str, target: usize| {
            if is_zeroed(target) {
                hits += 1;
                if hits <= 16 {
                    let treg = self.lookup_region_for_addr(target);
                    let h_in_cset = hreg.map(|i| cset_set.contains(&i)).unwrap_or(false);
                    let t_in_cset = treg.map(|i| cset_set.contains(&i)).unwrap_or(false);
                    eprintln!(
                        "[g1][DBG-ZERO] ZEROED target: {where_} holder={:#x} (region={:?} \
                         in_cset={h_in_cset}) slot->{:#x} (region={:?} in_cset={t_in_cset})",
                        holder, hreg, target, treg
                    );
                }
            }
        };
        for r in roots {
            report(0, None, "root", r.as_ptr() as usize);
        }
        let jit_skips = self.jit_tlab_skip_spans();
        for (ridx, region) in regions.iter().enumerate() {
            if region.region_type() == RegionType::Free {
                continue;
            }
            let base = region.data.as_ptr();
            let cursor = region.cursor();
            let mut off = 0usize;
            while off < cursor {
                let obj_ptr = unsafe { base.add(off) };
                // INT-3 — frozen-peer TLAB tail: skip before interpreting.
                if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, obj_ptr as usize) {
                    off += skip;
                    continue;
                }
                // TLAB-retire gap sentinel: skip its exact span.
                if let Some(gap) = gap_filler_len(obj_ptr) {
                    off += gap;
                    continue;
                }
                let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                if is_humongous_filler(header) {
                    break;
                }
                let sz = object_total_size(header);
                if sz < HEADER_SIZE || off + sz > cursor {
                    break;
                }
                let data = unsafe { obj_ptr.add(ARRAY_DATA_OFFSET) };
                if header.kind() == ObjectKind::Array {
                    if header.element_type() == ArrayElementType::Reference {
                        for k in 0..header.array_length() as usize {
                            let raw =
                                unsafe { std::ptr::read(data.add(k * 8) as *const u64) } as usize;
                            report(obj_ptr as usize, Some(ridx), "array-elem", raw);
                        }
                    }
                } else {
                    for_each_flat_object_reference_trusting_header(
                        obj_ptr as *mut u8,
                        header,
                        0,
                        |_, raw, _| {
                            report(obj_ptr as usize, Some(ridx), "field", raw);
                        },
                    );
                }
                off += sz;
            }
        }
        if hits > 0 {
            eprintln!(
                "[g1][DBG-ZERO] {hits} reference(s) to a ZEROED (freed) object this collection"
            );
        }
    }

    /// DIAGNOSTIC (env `CRATONVM_G1_DBG_REACH=1`): after a collection, BFS the
    /// heap from `roots` and report any REACHABLE slot whose target has a
    /// zeroed header or lies outside every region. Unlike `DBG-ZERO` (which
    /// walks regions linearly and false-positives on dead objects' harmless
    /// stale slots), this checks exactly the invariant the mutator depends on
    /// — so the FIRST collection it fires on is the one that corrupted live
    /// state. No-op unless the env is set.
    pub fn dbg_verify_reachable_integrity(
        &self,
        regions: &[G1Region],
        roots: &[ObjectRef],
        label: &str,
    ) {
        if !gc_flags().g1_dbg_reach {
            return;
        }
        static PAUSE_NO: AtomicUsize = AtomicUsize::new(0);
        let pause = PAUSE_NO.fetch_add(1, Ordering::Relaxed);

        let is_zeroed = |addr: usize| -> bool {
            let h = unsafe { &*(addr as *const ObjectHeader) };
            // WEAKER THAN IT WAS, deliberately and visibly.
            //
            // This used to end `&& h.identity_hash_code == 0`, justified by
            // "identity_hash_code is stamped non-zero on every allocation, so
            // requiring it zero keeps a live bare `new Object()` from
            // false-positiving". That premise died on 2026-08-07: the identity
            // hash left the header for the mark word and is now installed
            // LAZILY, on first request, so a live never-hashed object has a
            // zero word exactly like reclaimed memory does.
            //
            // The mark word is kept in the conjunction because it is the
            // strictly-better version of the same test (it also excludes
            // anything locked or forwarded), but it does NOT restore the old
            // discriminator: a live, never-hashed, never-locked `new Object()`
            // with class_id 0 is now indistinguishable from zeroed memory here.
            // This predicate is diagnostic, so the cost is a misleading label
            // rather than a wrong decision -- but it is a real loss and should
            // not be read as equivalent to what it replaced.
            //
            // gcd d1/e: the mark is tested FIRST, so `num_slots()` and
            // `array_length()` read this header's own shape word and never
            // follow a forwarded-looking mark (see `header_has_empty_shape`).
            // Same conjunction, same answers.
            h.mark_word.load(Ordering::Relaxed) == 0 && header_reads_zeroed(h)
        };

        // How many bytes an object starting at `addr` may legally occupy here,
        // or `None` when `addr` cannot be the start of a live object at all.
        //
        // Without this the BFS below traversed anything whose address merely
        // landed inside some region — including a word in the MIDDLE of a live
        // `byte[]`. `TestChunkedTransferEncodingWithProxy` fills a 1 GB array
        // with `'A'`, so such an address reads back
        // `cid=0x41414141 kind=Array len=0x41414141`, and the element loop then
        // read 1094795585 references from it and walked off the end of the
        // arena. That is the SIGSEGV in
        // g1-sigsegv-chunked-transfer-httpd-proxy-20260811-FIXED.md —
        // produced by this diagnostic, in a run that only crashed because the
        // diagnostic was on.
        //
        // The linear-walk sibling (`DBG-ZERO`) has always had this bound —
        // `if sz < HEADER_SIZE || off + sz > cursor { break }`. The BFS
        // computed the same `off`/`cursor` pair and used it only to *print*
        // ` ABOVE-CURSOR` beside a traversal it did anyway. The report carried
        // the evidence it ignored.
        let live_extent = |addr: usize| -> Option<usize> {
            let ridx = self.lookup_region_for_addr(addr)?;
            let region = &regions[ridx];
            let off = addr.checked_sub(region.data.as_ptr() as usize)?;
            let header = unsafe { &*(addr as *const ObjectHeader) };
            let size = object_total_size(header);
            if size < HEADER_SIZE {
                return None;
            }
            // A humongous object legitimately runs past its start region's own
            // buffer into the adjacent continuation regions, so `cursor` is the
            // wrong bound for it; what IS true is that it starts at offset 0 of
            // a `HumongousStart` region.
            if region.region_type() == RegionType::HumongousStart {
                return (off == 0).then_some(size);
            }
            (off + size <= region.cursor()).then_some(size)
        };

        let mut stack: Vec<usize> = Vec::new();
        let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut bad = 0usize;
        let mut check_push = |addr: usize,
                              holder: usize,
                              where_: &str,
                              slot: usize,
                              stack: &mut Vec<usize>,
                              seen: &mut std::collections::HashSet<usize>,
                              bad: &mut usize| {
            if addr == 0 {
                return;
            }
            let region = self.lookup_region_for_addr(addr);
            let no_extent = region.is_some() && live_extent(addr).is_none();
            if region.is_none() || is_zeroed(addr) || no_extent {
                *bad += 1;
                if *bad <= 16 {
                    let (hcid, hkind, hslots, hlen) = if holder != 0 {
                        let hh = unsafe { &*(holder as *const ObjectHeader) };
                        (
                            hh.class_id.as_u32(),
                            ObjectHeader::kind_tag(hh.mark_word.load(Ordering::Relaxed)),
                            hh.num_slots(),
                            hh.array_length(),
                        )
                    } else {
                        (0, 0, 0, 0)
                    };
                    let hregion = self.lookup_region_for_addr(holder);
                    let (hoff, hcur) = hregion
                        .map(|ri| {
                            let base = regions[ri].data.as_ptr() as usize;
                            (holder - base, regions[ri].cursor())
                        })
                        .unwrap_or((0, 0));
                    eprintln!(
                        "[g1][DBG-REACH] pause={pause} {label}: LIVE-REACHABLE {where_}[{slot}] \
                         holder={holder:#x} (cid={hcid} kind={hkind} slots={hslots} len={hlen} \
                         region={hregion:?} off={hoff:#x} cursor={hcur:#x}{}) -> {addr:#x} is {} \
                         (region={region:?})",
                        if hoff >= hcur { " ABOVE-CURSOR" } else { "" },
                        if region.is_none() {
                            "WILD"
                        } else if no_extent {
                            "NOT-AN-OBJECT-START (its size does not fit its region below the cursor)"
                        } else {
                            "ZEROED"
                        }
                    );
                }
                return;
            }
            if seen.insert(addr) {
                stack.push(addr);
            }
        };

        for r in roots {
            check_push(
                r.as_ptr() as usize,
                0,
                "root",
                0,
                &mut stack,
                &mut seen,
                &mut bad,
            );
        }
        while let Some(addr) = stack.pop() {
            // Only addresses `check_push` accepted reach this point, so the
            // extent is known-good; recompute it rather than carry it, since a
            // `seen` hit can push the same address from two holders.
            let Some(extent) = live_extent(addr) else {
                continue;
            };
            let header = unsafe { &*(addr as *const ObjectHeader) };
            if header.kind() == ObjectKind::Array {
                if header.element_type() == ArrayElementType::Reference {
                    let data = unsafe { (addr as *const u8).add(ARRAY_DATA_OFFSET) };
                    // Belt and braces over the extent check: read no further
                    // than the object's own payload even if `array_length()`
                    // claims more. `array_length()` is a field of the very
                    // header this walk is trying to decide it can trust.
                    let capacity = extent.saturating_sub(ARRAY_DATA_OFFSET) / 8;
                    for k in 0..(header.array_length() as usize).min(capacity) {
                        let raw = unsafe { std::ptr::read(data.add(k * 8) as *const u64) } as usize;
                        check_push(raw, addr, "array-elem", k, &mut stack, &mut seen, &mut bad);
                    }
                }
            } else {
                for_each_flat_object_reference_trusting_header(
                    addr as *mut u8,
                    header,
                    0,
                    |_, raw, _| {
                        check_push(raw, addr, "field", 0, &mut stack, &mut seen, &mut bad);
                    },
                );
            }
        }
        if bad > 0 {
            eprintln!(
                "[g1][DBG-REACH] pause={pause} {label}: {bad} corrupted LIVE-REACHABLE ref(s) \
                 ({} objects reached)",
                seen.len()
            );
        }

        // Root census (env CRATONVM_G1_DBG_ROOTCENSUS=1): per-root transitive
        // reach counts, to identify a stale root anchoring a large dead
        // subgraph (e.g. a historical list node retaining everything appended
        // after it through `next` chains).
        if gc_flags().g1_dbg_rootcensus {
            for (ri, r) in roots.iter().enumerate() {
                let addr = r.as_ptr() as usize;
                if addr == 0 || self.lookup_region_for_addr(addr).is_none() {
                    continue;
                }
                let mut rseen: std::collections::HashSet<usize> = std::collections::HashSet::new();
                let mut rstack = vec![addr];
                rseen.insert(addr);
                while let Some(a) = rstack.pop() {
                    let h = unsafe { &*(a as *const ObjectHeader) };
                    let data = unsafe { (a as *const u8).add(ARRAY_DATA_OFFSET) };
                    if h.kind() == ObjectKind::Array {
                        if h.element_type() == ArrayElementType::Reference {
                            for k in 0..h.array_length() as usize {
                                let raw = unsafe { std::ptr::read(data.add(k * 8) as *const u64) }
                                    as usize;
                                if raw != 0
                                    && self.lookup_region_for_addr(raw).is_some()
                                    && rseen.insert(raw)
                                {
                                    rstack.push(raw);
                                }
                            }
                        }
                    } else {
                        // Was a hand-rolled sixteenth copy of the flat walk, and
                        // it had all three defects the helper does not: it
                        // ignored `GC_FLAG_COMPACT` and strode a compact
                        // object's PACKED body at the legacy 16-byte pitch (the
                        // same COMPACT-LAYOUT PARITY divergence G1-9 fixed in
                        // the parallel evacuator), it transmuted each cell to a
                        // `Value` unchecked, and it took its count straight from
                        // `num_slots` with no screen at all — so a census over a
                        // torn header walked as far as the header said. Routed
                        // through the one helper instead; a census is exactly the
                        // kind of caller that should not be growing its own copy.
                        for_each_flat_object_reference_trusting_header(
                            a as *const u8,
                            h,
                            0,
                            |_, raw, _| {
                                if self.lookup_region_for_addr(raw).is_some() && rseen.insert(raw) {
                                    rstack.push(raw);
                                }
                            },
                        );
                    }
                }
                if rseen.len() > 1000 {
                    let h = unsafe { &*(addr as *const ObjectHeader) };
                    // Diagnostic slot peek: for SteadyChurn-shaped objects,
                    // slot 1 is the `seq` int — identifies WHICH node/payload
                    // anchors the subgraph (ancient seq ⟹ stale root).
                    // Legacy layout only: on a compact object the second
                    // 16-byte cell is not a slot at all (fields are packed by
                    // declared width), so the peek would print bytes from two
                    // different fields, and on a two-reference-field compact
                    // object it reads past the body. Screened decode for the
                    // same reason every other legacy-cell reader has one.
                    let slot1 = if !cratonvm_types::is_compact_object(h) && h.num_slots() >= 2 {
                        let v = unsafe {
                            cratonvm_gc_core::heap::read_value_cell_checked(
                                (addr as *const u8).add(HEADER_SIZE + SLOT_SIZE) as *const Value,
                                "g1::dbg_root_census",
                            )
                        };
                        format!("{v:?}")
                    } else {
                        String::new()
                    };
                    eprintln!(
                        "[g1][ROOTCENSUS] pause={pause} root#{ri} addr={addr:#x} cid={} kind={} \
                         slots={} reach={} slot1={slot1}",
                        h.class_id.as_u32(),
                        ObjectHeader::kind_tag(h.mark_word.load(Ordering::Relaxed)),
                        h.num_slots(),
                        rseen.len()
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Concurrent Marking
    // -----------------------------------------------------------------------

    /// Marking keep-alive (SATB soundness across evacuation pauses).
    ///
    /// While a concurrent mark cycle is active, two sets of raw object
    /// addresses are snapshot-live (live at mark start, so the SATB
    /// invariant obliges the marker to trace them to completion):
    ///
    ///   1. the gray set (`mark_worklist`) — discovered but not yet scanned;
    ///   2. the SATB log — old reference values overwritten by mutators
    ///      since the last drain.
    ///
    /// An evacuation pause moves/frees CSet regions, so both sets must be
    /// carried across the pause. Dropping a CSet-resident gray whose object
    /// the evacuation closure did not reach (the pre-fix `retain_mut`
    /// behaviour) silently unmarks its entire unscanned subtree: a live Old
    /// or humongous object whose only remaining marker-visible path ran
    /// through that gray is then never marked, and cleanup frees it live
    /// (the SteadyChurn `[FREED] cleanup region=N` failure). Leaving SATB
    /// entries in the queue is just as bad once the final remark consumes
    /// them: the addresses dangle into reset regions.
    ///
    /// Called at the start of an evacuation pause (STW, `regions` lock
    /// held). Drains the SATB log (every parked mutator's local buffer plus
    /// the global shards) into the gray set, then returns the gray entries
    /// that live in the CSet and are not already marked (marked ⇒ already
    /// scanned — its successors are in the gray set on their own). The
    /// caller evacuates each returned address exactly like a root; the
    /// existing worklist remap then rewrites every retained gray through
    /// the pause's forwarding map instead of dropping it.
    ///
    /// No-op (empty Vec) while no cycle is active — the SATB queue is only
    /// active between `start_concurrent_mark` and `cleanup`.
    pub fn marking_keepalive_roots(&self, regions: &[G1Region], cset_set: &RegionSet) -> Vec<usize> {
        self.marking_keepalive_roots_split(regions, cset_set).0
    }

    /// JIT round 23 wave 13, lane L9: [`Self::marking_keepalive_roots`] that
    /// also answers how many of the leading entries came from the SATB log.
    /// The rest came from the gray set (the seed queue and every worker
    /// deque), and the pause's gray remap (`remap_gray_set_after_pause`)
    /// already rewrites each of those entries to its copy, so their copies are
    /// gray without another push. See `CRATONVM_G1_KEEPALIVE_REGRAY_ONCE`.
    pub fn marking_keepalive_roots_split(
        &self,
        regions: &[G1Region],
        cset_set: &RegionSet,
    ) -> (Vec<usize>, usize) {
        if !self.satb_queue.is_active() {
            return (Vec::new(), 0);
        }
        // Mutators are parked at the STW barrier, so their thread-local SATB
        // buffers are stable — pull them into the global shards, then drain.
        cratonvm_gc_core::satb::flush_all_thread_satb_buffers(&self.satb_queue);
        let mut worklist = self.mark_worklist.lock();
        let mut keepalive: Vec<usize> = Vec::new();
        // JIT round 23 wave 11, lane L9: read once per pause.
        let filter_above_tams = g1_satb_filter_above_tams_enabled();
        for addr in self.satb_queue.drain() {
            if addr == 0 {
                continue;
            }
            let Some(idx) = self.lookup_region_for_addr(addr) else {
                continue;
            };
            if regions[idx].region_type() == RegionType::Free
                || regions[idx].mark_bitmap.is_marked(addr)
                // g1ep7: a young object carries no obligation in a
                // root-region cycle.
                || self.root_region_skips(&regions[idx])
            {
                continue;
            }
            // JIT round 23 wave 11, lane L9 (`CRATONVM_G1_SATB_FILTER_ABOVE_TAMS`,
            // default ON, `=0` kills): HotSpot's SATB filter
            // (`G1SATBMarkQueueSet` / `requires_marking`: an entry at or above
            // its region's TAMS is dropped). An object allocated after the
            // mark snapshot is implicitly live for this cycle -- cleanup counts
            // the extent above TAMS wholesale -- and SATB never needs to trace
            // it: everything it can name was either reachable in the snapshot
            // or logged when its last snapshot path was overwritten. Keeping
            // such an entry did real damage here: in a young pause it is a
            // CSet resident, so it was evacuated as a keep-alive root and its
            // copy re-grayed, and every later young pause of the cycle found
            // that copy again (`cset_resident` below) and carried it once
            // more. On `KeepLong -Xmx1g` (every allocation overwrites a ring
            // slot, so the SATB log carries about one young object per
            // allocation) that keep-alive set grew with the cycle: young
            // `roots_us` + `other_us` reached 6.9 s of 22.5 s of young pauses
            // (n23-w10a), `marking_keepalive_roots` + `remap_gray_set_after_pause`
            // 15.7% of the pausing thread (n23-w9base sample). `tams()` answers
            // the cursor when no cycle snapshot exists, so outside a real cycle
            // nothing is filtered.
            //
            // ONLY IN EDEN (wave 11 follow-up, after the n23-w11a gate: 1/12
            // OOME in each mode with the unrestricted filter). "Above TAMS"
            // does not mean "allocated after the snapshot" in every region
            // here: a Survivor or Old region that a pause of this cycle
            // claimed as an evacuation destination has TAMS 0, and it holds
            // the COPIES of snapshot objects. This collector's marker traces
            // through an above-TAMS object when it reaches one
            // (`try_mark_and_measure` answers `Some(0)` and the scan proceeds),
            // and a pause re-grays its keep-alive copies so the marker can
            // finish their subtrees -- so an SATB entry naming such a copy
            // still carries the obligation to trace its children, and
            // dropping it can leave snapshot-live Old objects unmarked.
            // (HotSpot can drop them because it scans the young regions of the
            // concurrent start as root regions; this collector does not.)
            // Eden holds mutator allocations only, so an Eden entry above
            // TAMS is a post-snapshot allocation, which SATB never has to
            // trace: everything it names was reachable in the snapshot or was
            // logged when its last snapshot path was overwritten.
            if filter_above_tams && regions[idx].region_type() == RegionType::Eden {
                let off = addr.wrapping_sub(regions[idx].data.as_ptr() as usize);
                if off >= regions[idx].tams() {
                    continue;
                }
            }
            if cset_set.contains(&idx) {
                // CSet resident: the evacuation loop below owns its survival;
                // its (forwarded) copy is re-grayed by the caller, so the
                // worklist push is not needed and cannot be capped away.
                keepalive.push(addr);
            } else if worklist.len() < MARK_WORKLIST_CAP {
                self.note_gray(addr, "satb-keepalive", 0);
                worklist.push(addr);
            } else {
                // Seed-class entry at cap. A plain drop is NOT recoverable:
                // the overflow rescan only re-walks MARKED objects, and no
                // marked object need reference a SATB seed. Mark it black
                // without scanning — the rescan the flag forces will scan
                // its fields. (Stays in place: non-CSet checked above.)
                // r11w19 g1store (W18-3): through the gray gate first.
                if self.cap_arm_seed_passes_gray_gate(&regions[idx], addr) {
                    regions[idx].try_mark_and_account(addr);
                }
                self.mark_worklist_overflowed.store(true, Ordering::Relaxed);
            }
        }
        // JIT round 23 wave 13: the SATB-sourced prefix ends here.
        let satb_sourced = keepalive.len();
        let cset_resident = |addr: usize| -> bool {
            match self.lookup_region_for_addr(addr) {
                Some(idx) => cset_set.contains(&idx) && !regions[idx].mark_bitmap.is_marked(addr),
                None => false,
            }
        };
        // Item 9 — entries carry a chunk index; the keep-alive wants addresses.
        keepalive.extend(
            worklist
                .iter()
                .map(|&e| gray_entry_addr(e))
                .filter(|&a| cset_resident(a)),
        );
        // F-12 — the gray set is no longer only the seed queue. A CSet-resident
        // gray sitting in a WORKER'S deque is just as snapshot-live as one in
        // the seed queue, and missing it is not a missed optimisation: the
        // object would not be evacuated, so the post-pause remap would find it
        // unforwarded and in the CSet and DROP it, silently unmarking its whole
        // unscanned subtree. That is the SteadyChurn freed-live-Old-region
        // defect, arriving by a new route.
        //
        // The seed-queue guard is released first so this never nests two gray
        // locks; the caller is stop-the-world, so nothing can push in between.
        drop(worklist);
        for deque in self.mark_deques.iter() {
            let deque = deque.lock();
            keepalive.extend(
                deque
                    .iter()
                    .map(|&e| gray_entry_addr(e))
                    .filter(|&a| cset_resident(a)),
            );
        }
        // LANE-C — the loop above pushed SATB seeds onto the gray set, and
        // nothing told the markers. Every OTHER producer of gray work wakes
        // them (`remark`, `push_gray_or_mark`, the barrier's spill — item 9b),
        // and this one, which runs once per evacuation pause and can push the
        // whole accumulated SATB log in one go, did not. The markers are not
        // stopped by the pause; they are blocked on the regions write guard the
        // caller holds, and on release they go straight back to whatever they
        // were doing — which, for a marker that ran dry before the pause, is
        // sitting parked until the `WORKER_POLL_MS` fallback fires. So the
        // largest single batch of gray work a cycle produces was the one most
        // likely to wait a quarter of a second to be noticed.
        //
        // Cheap and unconditional: `wake_marker` is a no-op outside a cycle
        // with a live controller, and inside one it is a `notify_all` on a
        // condvar nobody else contends.
        self.wake_marker();
        (keepalive, satb_sourced)
    }

    /// `CRATONVM_G1_DBG_GRAY_PROV` — see [`G1Collector::gray_prov`].
    pub fn gray_prov_enabled() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            false        })    }

    /// Record that `addr` was grayed by `tag` (naming `parent`, or `0` when the
    /// push has no parent object). No-op unless the flag is on.
    pub fn note_gray(&self, addr: usize, tag: &'static str, parent: usize) {
        if !Self::gray_prov_enabled() {
            return;
        }
        self.gray_prov.lock().insert(addr, (tag, parent));
    }

    /// Throttle for the gray-refusal report: the first 8, then powers of two.
    ///
    /// `NotAllocated` refusals are routine and arrive in the thousands per run;
    /// an unthrottled `warn!` per entry is itself a several-megabyte log and
    /// slows the marker enough to change the workload.
    pub fn mark_oob_report_budget(&self) -> bool {
        let n = self.mark_oob_report_count.fetch_add(1, Ordering::Relaxed);
        n < 8 || n.is_power_of_two()
    }

    /// The recorded provenance of `addr`, for the implausible-header report.
    pub fn gray_prov_of(&self, addr: usize) -> Option<(&'static str, usize)> {
        if !Self::gray_prov_enabled() {
            return None;
        }
        self.gray_prov.lock().get(&addr).copied()
    }

    /// LANE-C — drop the provenance map at a cycle boundary.
    ///
    /// Two reasons, and the second is the one that matters. It GROWS: one entry
    /// per address ever grayed, never removed, for the life of the VM — so
    /// `CRATONVM_G1_DBG_GRAY_PROV=1` on a soak is a diagnostic that eventually
    /// becomes the failure it was turned on to investigate. And it goes STALE:
    /// heap addresses are reused across cycles, so an entry recorded in cycle N
    /// answers `gray_prov_of` in cycle N+1 with a tag and a parent from a
    /// region that has since been zero-filled and re-typed. That is worse than
    /// no provenance at all — the whole point of the report is to separate "a
    /// marked parent held a stale reference" (a real remembered-set defect)
    /// from "SATB retained a dead object", and it decides that by looking the
    /// parent up. A stale row makes it answer confidently and wrongly.
    ///
    /// No-op unless the flag is on, so this costs one `OnceLock` load on the
    /// two cycle-boundary paths.
    pub fn clear_gray_prov(&self) {
        if !Self::gray_prov_enabled() {
            return;
        }
        self.gray_prov.lock().clear();
    }

    // -----------------------------------------------------------------------
    // F-12 — the gray set: one shared seed queue plus one deque per worker
    // -----------------------------------------------------------------------

    /// Are all per-worker deques empty?
    ///
    /// Separate from [`Self::gray_set_is_empty`] because the overflow rescan
    /// already holds the seed queue's lock when it asks, and re-taking it here
    /// would deadlock.
    pub fn mark_deques_are_empty(&self) -> bool {
        self.mark_deques.iter().all(|d| d.lock().is_empty())
    }

    /// Is there no gray work anywhere — seed queue or any worker's deque?
    ///
    /// This replaces every `mark_worklist.lock().is_empty()` that meant
    /// "marking has nothing left to do". The distinction matters: the seed
    /// queue drains to empty as soon as the workers have pulled their chunks,
    /// long before the closure is complete, so asking only the seed queue would
    /// declare a cycle converged with the entire transitive closure still in
    /// flight. Sites that genuinely mean "the SEED queue is empty" (there are
    /// none left) would have to say so explicitly.
    pub fn gray_set_is_empty(&self) -> bool {
        // Two statements, so the seed-queue guard is provably released before
        // any deque lock is taken. Nothing takes a deque and then the seed
        // queue, so this direction is the only one that exists.
        if !self.mark_worklist.lock().is_empty() {
            return false;
        }
        self.mark_deques_are_empty()
    }

    /// Discard every gray entry, seed queue and deques alike.
    pub fn clear_gray_set(&self) {
        self.mark_worklist.lock().clear();
        for deque in self.mark_deques.iter() {
            deque.lock().clear();
        }
    }

    /// Is `addr` waiting to be scanned anywhere in the gray set?
    /// Diagnostics and tests only — O(gray set).
    pub fn gray_set_contains(&self, addr: usize) -> bool {
        let names = |e: &usize| gray_entry_addr(*e) == addr;
        if self.mark_worklist.lock().iter().any(names) {
            return true;
        }
        self.mark_deques.iter().any(|d| d.lock().iter().any(names))
    }

    /// gci i11/gm finding 6 -- drop every gray entry, seed queue and deques
    /// alike, whose address lies in one of `ranges` (`[lo, hi)` byte ranges of
    /// humongous spans this pause is freeing). Returns how many were dropped.
    ///
    /// Stop-the-world with the regions WRITE guard held, as for
    /// [`Self::remap_gray_set_after_pause`]: no marker is mid-batch and none
    /// holds a popped address. Seed queue first, then each deque, one lock at a
    /// time (the order `gray_set_is_empty` uses).
    pub fn drop_gray_entries_in_ranges(&self, ranges: &[(usize, usize)]) -> usize {
        if ranges.is_empty() {
            return 0;
        }
        let inside = |entry: &usize| -> bool {
            let addr = gray_entry_addr(*entry);
            ranges.iter().any(|&(lo, hi)| addr >= lo && addr < hi)
        };
        let mut dropped = 0usize;
        {
            let mut seeds = self.mark_worklist.lock();
            let before = seeds.len();
            seeds.retain(|e| !inside(e));
            dropped += before - seeds.len();
        }
        for deque in self.mark_deques.iter() {
            let mut deque = deque.lock();
            let before = deque.len();
            deque.retain(|e| !inside(e));
            dropped += before - deque.len();
        }
        dropped
    }

    /// Test/diagnostic (item 9): how many gray entries are array-chunk
    /// continuations rather than objects.
    pub fn dbg_gray_chunk_entries(&self) -> usize {
        let is_chunk = |e: &usize| gray_entry_chunk(*e) > 0;
        self.mark_worklist
            .lock()
            .iter()
            .filter(|e| is_chunk(e))
            .count()
            + self
                .mark_deques
                .iter()
                .map(|d| d.lock().iter().filter(|e| is_chunk(e)).count())
                .sum::<usize>()
    }

    /// Item 9b — wake every parked concurrent-mark worker. Installed by
    /// [`crate::g1_concurrent::ConcurrentMarkController::spawn`] for the life
    /// of a cycle; a no-op otherwise. Called wherever gray work is created
    /// (a SATB buffer spilling to the shared queue, a pause pushing a
    /// keep-alive, remark seeding roots), which is what lets the workers park
    /// without a short poll.
    pub fn install_mark_waker(
        &self,
        waker: std::sync::Arc<crate::g1_concurrent::ConcurrentMarkState>,
    ) {
        *self.mark_waker.lock() = Some(waker);
    }

    pub fn clear_mark_waker(&self) {
        *self.mark_waker.lock() = None;
    }

    pub fn wake_marker(&self) {
        if let Some(w) = self.mark_waker.lock().as_ref() {
            w.notify_work_available();
        }
    }

    /// F-12 — fill `out` with work for `worker`, from the shared seed queue if
    /// it has any and otherwise by stealing half of a peer's deque.
    ///
    /// Returns `false` when there is nothing anywhere, which is the caller's
    /// signal to stop stepping (it is NOT termination on its own — a peer may
    /// still be mid-scan; see [`ActiveMarker`]).
    ///
    /// # Lock discipline
    ///
    /// At most ONE deque lock is held at a time: the thief drains its victim
    /// into `out` and the caller extends the thief's own deque afterwards. Two
    /// workers stealing from each other therefore cannot deadlock, which the
    /// obvious implementation — lock mine, then lock yours — would.
    ///
    /// The victim scan starts at `worker + 1` rather than at 0 so that N
    /// starving workers do not all descend on worker 0's deque at once.
    ///
    /// Halving is the standard split: taking one entry makes the thief come
    /// straight back, and taking everything makes the pair swap the whole queue
    /// back and forth. The thief takes from the FRONT because the victim pops
    /// from the BACK — the two ends are the ones least likely to be the same
    /// object's neighbours, so the victim keeps the subtree it is descending.
    pub fn take_marking_work(&self, worker: usize, out: &mut Vec<usize>) -> bool {
        {
            let mut seeds = self.mark_worklist.lock();
            let take = MARK_REFILL_CHUNK.min(seeds.len());
            if take > 0 {
                let at = seeds.len() - take;
                out.extend(seeds.drain(at..));
                return true;
            }
        }
        let n = self.mark_deques.len();
        for k in 1..n {
            let victim = (worker + k) % n;
            let mut theirs = self.mark_deques[victim].lock();
            if theirs.len() >= 2 {
                let half = theirs.len() / 2;
                out.extend(theirs.drain(..half));
                self.mark_steals.fetch_add(1, Ordering::Relaxed);
                return true;
            }
        }
        // LANE-C — LAST RESORT: take a SINGLETON deque whole.
        //
        // Without this pass the halving above strands work permanently, and
        // the consequence is not a slow mark, it is a HANG inside a
        // stop-the-world pause. Halving bottoms out at one: a victim with 3
        // entries yields 1 and keeps 2, with 2 yields 1 and keeps 1, and a
        // deque of 1 fails `len() >= 2` for every thief forever. So ANY peer
        // deque that is non-empty when its owner stops leaves exactly one
        // unreachable gray entry behind.
        //
        // That entry then makes `gray_set_is_empty()` false for the rest of the
        // cycle. The production final-remark driver is
        // `while !concurrent_mark_step(usize::MAX) {}`
        // (`VmHeap::g1_final_remark_and_cleanup`), which calls worker 0 — so
        // worker 0 drains its own deque, fails to steal the singleton, breaks
        // out of its batch loop, reports "not converged" because the gray set
        // is non-empty, and is called again. Forever, with the world stopped.
        //
        // Reaching it only needs a worker to stop holding work, which
        // `ConcurrentMarkController::worker_loop` makes ordinary: its final
        // pass after observing `should_stop` is bounded by
        // `WORKER_STEP_BUDGET` (256 objects) and then the thread RETURNS, so a
        // stop issued while a marker is mid-frontier leaves the remainder in a
        // deque nobody owns any more. `Drop`'s best-effort stop does the same.
        //
        // Ping-pong — the reason the halving guard exists at all — is not
        // reintroduced: the thief takes the victim's ENTIRE deque, so the
        // victim gets nothing back to hand over again, and this pass runs only
        // when every deque already holds at most one entry, i.e. when there is
        // no bulk left to split.
        for k in 1..n {
            let victim = (worker + k) % n;
            let mut theirs = self.mark_deques[victim].lock();
            if !theirs.is_empty() {
                out.extend(theirs.drain(..));
                self.mark_steals.fetch_add(1, Ordering::Relaxed);
                return true;
            }
        }
        false
    }

    /// F-12 — rewrite every gray entry through this pause's forwarding map,
    /// dropping the ones whose object was in the collection set and did not
    /// survive.
    ///
    /// This used to be five copies of the same `retain_mut` over
    /// `mark_worklist`, one per collection path. Striping the gray set across
    /// per-worker deques turns each of those into "and every deque too", and a
    /// path that remapped the seed queue but forgot a deque would leave a
    /// marker holding an address in a region this pause just reset — a marker
    /// UAF, and exactly the defect the mixed path had before those blocks
    /// existed. One implementation, five callers.
    ///
    /// # Stop-the-world
    ///
    /// The caller holds the regions WRITE guard, which is what makes this
    /// safe: markers only touch a deque under the read guard, so none can be
    /// mid-batch here, and none can be holding a popped address either (a pop
    /// and its scan happen inside one read-guard hold).
    pub fn remap_gray_set_after_pause(
        &self,
        regions: &[G1Region],
        cset_set: &RegionSet,
        pointer_map: &cratonvm_types::PointerMap,
    ) {
        // JIT round 23 wave 7, lane L4 (`CRATONVM_G1_GRAY_REMAP_CSET_SCREEN`,
        // default ON, `=0` kills): ask the region first. Every key of
        // `pointer_map` is a collection-set resident (only those are
        // evacuated, self-forwards included -- `ForwardLookup`'s argument), so
        // an entry outside the CSet can have no forward and is kept exactly as
        // the not-forwarded arm below keeps it. The gray set during a
        // concurrent cycle is mostly Old addresses, and it was hashed whole on
        // every young pause: `PointerMap::get` was ~30 % of the driver's busy
        // samples on `KeepLong -Xmx1g` (n23-w6, matched pdb), the largest
        // single term of the G1 young pause under a marking cycle.
        let screen = true;
        let mut remap = |entry: &mut usize| -> bool {
            // Item 9 — an entry carries a chunk index above the address; remap
            // the address and keep the chunk.
            let chunk = gray_entry_chunk(*entry);
            let addr = gray_entry_addr(*entry);
            if screen
                && !matches!(
                    self.region_for_ptr(regions, addr as *mut u8),
                    Some(idx) if cset_set.contains(&idx)
                )
            {
                return true;
            }
            if let Some(&new_addr) = pointer_map.get(&addr) {
                // Object was evacuated — follow the forwarding ptr.
                *entry = gray_entry(new_addr, chunk);
                return true;
            }
            // Not forwarded. If the address lived in a CSet region it is now
            // dangling (the region was reset above) so drop it. Otherwise
            // (Old / non-CSet) leave it alone.
            match self.region_for_ptr(regions, addr as *mut u8) {
                Some(idx) if cset_set.contains(&idx) => false,
                _ => true,
            }
        };
        {
            let mut seeds = self.mark_worklist.lock();
            if !seeds.is_empty() {
                seeds.retain_mut(&mut remap);
            }
        }
        for deque in self.mark_deques.iter() {
            let mut deque = deque.lock();
            if !deque.is_empty() {
                deque.retain_mut(&mut remap);
            }
        }
        // gcd d10/r: the `metadata_pin` rows are this cycle's snapshot of the
        // loader -> metadata edges (`vm::memory::roots::
        // g1_cycle_keeps_metadata_rows`); the pause rooted every address they
        // name, and they are rewritten HERE, under the same write guard, so no
        // marker batch can capture them between the evacuation and the remap.
        // Same verdict as a gray entry. Outside a cycle the VM's next root
        // scan rebuilds them, so they are left alone.
        if self.gc_state.is_marking_active() {
            cratonvm_types::metadata_pin::remap_metadata_pins(|addr| {
                // Wave 7: the same CSet screen before the hash probe.
                if screen
                    && !matches!(
                        self.region_for_ptr(regions, addr as *mut u8),
                        Some(idx) if cset_set.contains(&idx)
                    )
                {
                    return Some(addr);
                }
                if let Some(&new_addr) = pointer_map.get(&addr) {
                    return Some(new_addr);
                }
                match self.region_for_ptr(regions, addr as *mut u8) {
                    Some(idx) if cset_set.contains(&idx) => None,
                    _ => Some(addr),
                }
            });
        }
    }

    /// Re-gray the relocated copy of a marking keep-alive object: push
    /// `new_addr` onto the mark worklist, or — at the worklist cap — mark it
    /// black-without-scan and set the overflow flag so the conservative
    /// rescan in `concurrent_mark_step` scans its fields instead. `new_addr`
    /// is a to-space (non-CSet) address, so the mark bit survives the pause.
    ///
    /// LANE-C: an address in no region, or in a `Free` one, is dropped HERE
    /// rather than one gray pop later — see the screen at the top of the body
    /// for why the late rejection was costing a worklist slot and miscounting
    /// a routine non-candidate as a fail-safe event.
    pub fn push_gray_or_mark(&self, regions: &[G1Region], new_addr: usize) {
        // LANE-C — screen BEFORE the push, not after the pop.
        //
        // This used to push `new_addr` onto the seed queue unconditionally
        // while under the cap, and only consult the region table on the
        // overflow arm. An address in no region at all — 0 is the one that
        // actually arrived, from an SATB entry the barrier's own null gate
        // does not cover on every path — therefore occupied a gray slot
        // against `MARK_WORKLIST_CAP` and was rejected one gray pop later by
        // `region_for_ptr`, after a marker had taken the regions guard for it.
        // A Free region's address goes the same way but louder: it reaches
        // `classify_mark_scan_target`, which refuses it as `NotAllocated` and
        // bumps `mark_oob_gray_skips`, so a routine non-candidate was being
        // counted as a fail-safe event.
        //
        // The screen is the same one `remark` applies to its own seeds, and it
        // cannot lose work: an address in no live region names no object this
        // cycle can mark.
        let Some(idx) = self.lookup_region_for_addr(new_addr) else {
            return;
        };
        // `.get` rather than `[idx]`: the index comes from the address cache,
        // not from this slice, and cleanup must never gain a panic path.
        let Some(region) = regions.get(idx) else {
            return;
        };
        if region.region_type() == RegionType::Free {
            return;
        }
        let mut worklist = self.mark_worklist.lock();
        if worklist.len() < MARK_WORKLIST_CAP {
            self.note_gray(new_addr, "push-gray-or-mark", 0);
            worklist.push(new_addr);
        } else {
            // r11w19 g1store (W18-3): through the gray gate first.
            if self.cap_arm_seed_passes_gray_gate(region, new_addr) {
                region.try_mark_and_account(new_addr);
            }
            self.mark_worklist_overflowed.store(true, Ordering::Relaxed);
        }
        drop(worklist);
        self.wake_marker();
    }

    /// [`Self::push_gray_or_mark`] for every address of `addrs`, in order,
    /// under ONE seed-queue lock and with ONE marker wake.
    ///
    /// The G1 evacuation page: a pause of an open cycle re-grays its whole
    /// keep-alive set, and taking the seed-queue mutex plus the waker's two
    /// mutexes and a `notify_all` once per copy made `push_gray_or_mark` 2.8%
    /// of all samples on `R23W6KeepLong -Xmx1g` with every young pause parallel
    /// (`CRATONVM_G1_PARALLEL_UNDER_LOW_FREE_LATCH=1`), beside the gray remap.
    /// Each address gets exactly the per-entry verdict of the one-address form
    /// (the same screen, the same cap arm); the markers need to know work
    /// exists, not how much (the rule `concurrent_mark_step_worker`'s SATB
    /// drain follows). `CRATONVM_G1_GRAY_PUSH_BATCH=0` pushes one at a time.
    pub fn push_gray_or_mark_all(&self, regions: &[G1Region], addrs: impl IntoIterator<Item = usize>) {
        let mut any = false;
        let mut overflowed = false;
        {
            let mut worklist = self.mark_worklist.lock();
            for new_addr in addrs {
                let Some(idx) = self.lookup_region_for_addr(new_addr) else {
                    continue;
                };
                let Some(region) = regions.get(idx) else {
                    continue;
                };
                if region.region_type() == RegionType::Free {
                    continue;
                }
                any = true;
                if worklist.len() < MARK_WORKLIST_CAP {
                    self.note_gray(new_addr, "push-gray-or-mark", 0);
                    worklist.push(new_addr);
                } else {
                    if self.cap_arm_seed_passes_gray_gate(region, new_addr) {
                        region.try_mark_and_account(new_addr);
                    }
                    overflowed = true;
                }
            }
        }
        if overflowed {
            self.mark_worklist_overflowed.store(true, Ordering::Relaxed);
        }
        if any {
            self.wake_marker();
        }
    }

    /// Test/diagnostic: has `addr` been grayed or marked by the current
    /// mark cycle? True if its region's bitmap has it marked OR it sits in
    /// the mark worklist. Used by the g1_concurrent SATB delivery test,
    /// whose raw queue-contents assertion under-approximates delivery now
    /// that the marker drains the shards mid-cycle (G1MARK-6). Takes the
    /// regions lock then (after dropping it) the worklist lock — never both.
    pub fn dbg_is_grayed_or_marked(&self, addr: usize) -> bool {
        {
            let regions = self.regions.read();
            if let Some(idx) = self.region_for_ptr(&regions, addr as *mut u8) {
                if regions[idx].mark_bitmap.is_marked(addr) {
                    return true;
                }
            }
        }
        self.gray_set_contains(addr)
    }

    /// Abort an in-flight marking cycle WITHOUT acting on the (incomplete)
    /// bitmap: discard the gray set, deactivate the SATB barrier, return the
    /// phase to `Idle`. No cleanup verdicts are computed — the next cycle
    /// re-discovers liveness from scratch. Used when the cycle's driver
    /// state is lost (an orphaned controller slot) so the completion gate
    /// in the VM does not spin forever on a cycle nobody is driving.
    ///
    /// gci i15/ca: also the full collection's abort of an open cycle
    /// (`VmHeap::g1_abort_concurrent_mark`, opt-in
    /// `CRATONVM_G1_FULL_GC_ABORTS_OPEN_CYCLE`), called inside a STW pause
    /// after the marker was joined. What may read the cycle's state after this
    /// returns, and why none of it reads the half-built bitmap:
    /// `is_live_after_mark` answers LIVE (empty `mark_start_snapshot`);
    /// `mark_proven_dead` (the rset dead-holder filter and its walkers) and
    /// `arm_full_compaction` decline (`last_mark_trusted` false);
    /// `marking_keepalive_roots` returns nothing (queue inactive);
    /// `scrub_dead_refs_below_tams` / `walk_marked_bytes_below_tams` run only
    /// from `cleanup`, which no aborted cycle reaches; the mark bitmaps
    /// themselves are cleared by the next `start_concurrent_mark*` before any
    /// marker runs. The mixed-phase state (`marking_complete`,
    /// `mixed_gc_remaining`) belongs to the last COMPLETED cycle and is left as
    /// it is, as on the orphan path.
    pub fn abort_concurrent_mark(&self) {
        // F-15: an aborted cycle measures nothing — its duration is the time to
        // the abort, not the time marking would have taken, and folding that in
        // would bias the prediction short and make every later cycle start too
        // late. Drop the window without updating the estimates.
        *self.mark_cycle_start.lock() = None;
        // F-06: an aborted cycle's bitmap is discarded, so the byte
        // accumulator derived from it must be discarded too — and `mark_start`
        // with it, or the next `cleanup` driven outside a cycle would read a
        // stale TAMS and treat post-snapshot bytes as implicitly live on the
        // strength of a cycle that never finished.
        {
            let mut regions = self.regions.write();
            for r in regions.iter_mut() {
                r.mark_start = None;
                r.marked_bytes_below_tams.store(0, Ordering::Relaxed);
            }
            self.mark_start_snapshot.lock().clear();
        }
        self.last_mark_trusted.store(false, Ordering::Release);
        self.clear_gray_set();
        self.clear_overlay_snapshot();
        // LANE-C — per-cycle diagnostic state dies with the cycle it describes.
        self.clear_gray_prov();
        self.mark_oob_gray_skips.store(0, Ordering::Relaxed);
        self.mark_worklist_overflowed
            .store(false, Ordering::Relaxed);
        self.mark_saw_implausible.store(false, Ordering::Relaxed);
        self.root_regions_cycle.store(false, Ordering::Relaxed);
        // INT-8: the skip set is per-cycle state.
        // Length stored BEFORE the clear, so a marker racing the two lines
        // COULD see zero over the still-populated set (gci i14/gq G-5: the
        // old comment said it never could). That direction only traces a
        // referent -- over-retention, never under-marking -- and no marker
        // runs here: the controller was joined (or never spawned) first.
        self.reference_skip_len.store(0, Ordering::Release);
        self.reference_skip.lock().clear();
        // G1AUD-2 — leave the marking-active phase BEFORE deactivating the
        // queue, not after. The reverse order opens a window in which
        // `is_marking_active()` is still true while the queue is already
        // INACTIVE: every reference store in that window reads its old value
        // (the phase gate admits it) and then drops it (the queue gate rejects
        // it). Harmless for an *abort*, whose bitmap is discarded anyway — but
        // it is the only place in the collector that violated
        // `is_marking_active() => satb_queue.is_active()`, so the invariant
        // could not be asserted globally while it stood. See
        // `satb_pre_barrier_required` (gc-common w5-e: which now gates on the
        // queue itself, so this order is no longer load-bearing for stores).
        self.gc_state.set_phase(ConcurrentGcPhase::Idle);
        // The drained vector is discarded: an abort is not a safepoint, so
        // this caller could not make the "this cycle's snapshot" claim even if
        // it wanted the entries. Sound because no sweep follows an aborted
        // cycle — a stranded entry is only a false root for the NEXT cycle's
        // remark, which over-retains rather than freeing something live.
        //
        // Since 2026-09-21 the type says this: `deactivate_and_drain` takes a
        // `StopTheWorldToken` that this site cannot produce, and the comment
        // above is now the compiler's business rather than a promise.
        self.satb_queue.deactivate_and_discard();
    }

    /// Start a concurrent marking cycle. Sets phase to InitialMark.
    ///
    /// Audit fix (HIGH-3): also clears the mark worklist so a previous
    /// aborted cycle doesn't leak gray pointers into the new cycle.
    ///
    /// # Stop-the-world
    ///
    /// I-17 (audit §7). This phase is STW and had no mechanical enforcement of
    /// it — every other mutating-by-`&self` entry point on this collector takes
    /// a [`StopTheWorldToken`], and the three mark-cycle entry points
    /// (`start_concurrent_mark`, `remark`, `cleanup`) did not, so the one
    /// invariant in the table with nothing but a comment behind it was the one
    /// governing the phase that reclassifies and frees regions. The token is a
    /// zero-sized witness: it costs nothing at run time and makes the
    /// requirement a signature rather than a convention.
    ///
    /// [`StopTheWorldToken`]: cratonvm_gc_core::collector::StopTheWorldToken
    pub fn start_concurrent_mark(&self, stw: &cratonvm_gc_core::collector::StopTheWorldToken) {
        self.start_concurrent_mark_with_reference_skip(stw, &[]);
    }

    /// [`Self::start_concurrent_mark`] with this cycle's referent-slot skip set
    /// (`reference_objs`, the Weak/Soft/Phantom `Reference` OBJECT addresses
    /// the VM's processor holds, snapshotted in the same initial-mark pause)
    /// published BEFORE anything can be grayed or scanned.
    ///
    /// gci i14/gq G-4 (`gci-i14gq-g1-reference-processing-and-finalization-review-findings-FIXED-20261016`):
    /// the VM used to publish the set after this function returned and after
    /// `VmHeap::g1_start_concurrent_mark` had spawned the marker, while
    /// `capture_overlay_snapshot` below grays an incomplete overlay
    /// provider's rows as roots. A marker step in that window scanned a
    /// `WeakReference` element with slot 0 traced, so its referent stayed
    /// marked for the cycle (a weak reference cleared one cycle late). The set
    /// now replaces the INT-8 clear, so it is in force before the first gray.
    /// An empty slice is exactly the old clear (the VM then publishes later via
    /// [`Self::set_reference_skip_set`], as the unit tests do).
    pub fn start_concurrent_mark_with_reference_skip(
        &self,
        stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        reference_objs: &[usize],
    ) {
        self.start_concurrent_mark_inner(stw, reference_objs, false);
    }

    /// g1ep7: [`Self::start_concurrent_mark_with_reference_skip`] for the
    /// concurrent cycle, with root regions when `CRATONVM_G1_ROOT_REGIONS`
    /// allows (`g1_root_regions.rs`). The `System.gc()` full cycle keeps the
    /// plain start: its remark judges young referents by the bitmap.
    pub fn start_concurrent_mark_with_root_regions(
        &self,
        stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        reference_objs: &[usize],
    ) {
        self.start_concurrent_mark_inner(stw, reference_objs, root_regions::g1_root_regions_enabled());
    }

    pub fn start_concurrent_mark_inner(
        &self,
        _stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        reference_objs: &[usize],
        root_regions: bool,
    ) {
        // F-15: open the measurement window adaptive IHOP is built on. Must be
        // the first thing, so the duration covers the whole cycle including the
        // bitmap clear below.
        self.note_mark_cycle_start();
        // LANE W3-C — the population sample `CRATONVM_G1_MARK_SIDE_TABLES`'
        // falsification test needs, taken on BOTH arms of that flag because it
        // is a fact about the workload and not about the lever. One snapshot
        // per mark cycle; see [`MARK_CYCLES_STARTED`].
        MARK_CYCLES_STARTED.fetch_add(1, Ordering::Relaxed);
        if !MarkSideTables::capture(
            self.rset_cache_epoch.load(Ordering::Acquire),
            self.arena_base..self.arena_end,
        )
        .is_empty()
        {
            MARK_CYCLES_WITH_LOADER_PINS.fetch_add(1, Ordering::Relaxed);
        }
        self.last_mark_trusted.store(false, Ordering::Release);
        self.gc_state.set_phase(ConcurrentGcPhase::InitialMark);
        self.satb_queue.activate();
        // gci i16/mr: the un-retired TLAB tails this pause's take-over
        // published (a frozen compiled peer keeps bumping its tail after the
        // pause), read before the regions guard: TAMS starts at a tail's start
        // (`tams_below_reserved_tails`). Empty on a cycle that froze nobody.
        let reserved_tails = self.jit_tlab_skip_spans();
        // gci i17/rv (finding 1): the objects a lowered TAMS put above TAMS
        // although they predate the snapshot (between a reserved tail and the
        // cursor). Grayed below, after the gray set is cleared.
        let mut lowered_tams_grays: Vec<usize> = Vec::new();
        let mut lowered_tams_walk_breaks = 0usize;
        // g1ep7: no young filter until this cycle's root regions are known.
        self.root_regions_cycle.store(false, Ordering::Relaxed);
        let mut root_region_objs: Option<Vec<(usize, usize)>> = None;
        // Round-2 fix (HIGH — GC #5): clear every per-region bitmap so a
        // previous cycle's mark bits don't leak into this one.
        {
            let mut regions = self.regions.write();
            for r in regions.iter_mut() {
                // gc-common w4-e: memset under the write guard (and the STW
                // token); see `G1Region::reset` for why `&mut` is a witness.
                r.mark_bitmap.clear_exclusive();
                // F-06: the per-region mirror of the row written below, and the
                // byte accumulator that is only meaningful against it.
                let tams =
                    tams_below_reserved_tails(r.data.addr(), r.cursor(), &reserved_tails);
                if tams < r.cursor() {
                    // SAFETY: stop-the-world initial mark under the regions
                    // write guard; the walk stays inside `[tams, cursor)` of
                    // this region and skips every published tail.
                    let complete = unsafe {
                        pre_snapshot_starts_above_lowered_tams(
                            r.data.addr(),
                            tams,
                            r.cursor(),
                            &reserved_tails,
                            &mut lowered_tams_grays,
                        )
                    };
                    if !complete {
                        lowered_tams_walk_breaks += 1;
                    }
                }
                r.mark_start = Some((r.reuse_epoch, tams, r.region_type()));
                r.marked_bytes_below_tams.store(0, Ordering::Relaxed);
            }
            // g1ep7: the young regions' TAMS goes to 0 here, before the
            // snapshot rows below copy it.
            if root_regions {
                root_region_objs = self.root_regions_prepare(&mut regions, &reserved_tails);
            }
            // TAMS snapshot: record every region's incarnation + fill level
            // at mark start so `cleanup` can treat later allocations as live
            // (see `mark_start_snapshot`). Same regions-then-snapshot lock
            // order as `cleanup`. gci i16/mr: the same row as the region's own
            // mirror just written (TAMS lowered to a reserved tail's start).
            let mut snap = self.mark_start_snapshot.lock();
            snap.clear();
            snap.extend(regions.iter().map(|r| {
                r.mark_start
                    .unwrap_or((r.reuse_epoch, r.cursor(), r.region_type()))
            }));
        }
        self.root_regions_cycle
            .store(root_region_objs.is_some(), Ordering::Relaxed);
        self.clear_gray_set();
        // LANE-C — a new cycle must not inherit the previous one's gray
        // provenance: the addresses are reused, so a stale row makes the
        // implausible-header report name a parent from a region that has since
        // been zero-filled. See `clear_gray_prov`.
        self.clear_gray_prov();
        // Round-9 gc HIGH-5: reset overflow indicator at cycle start so
        // a previous cycle's overflow doesn't trigger a needless rescan.
        self.mark_worklist_overflowed
            .store(false, Ordering::Relaxed);
        // G1MARK-8: same per-cycle reset for the implausible-header flag.
        self.mark_saw_implausible.store(false, Ordering::Relaxed);
        // gci i17/rv (finding 1): a lowered TAMS makes the pre-snapshot objects
        // between a reserved tail and the cursor implicitly live, and nothing
        // traced them: an unreachable one read LIVE to `is_live_after_mark` (its
        // weak references kept, its JNI weak globals and keyed rows too) while
        // the objects only it named stayed unmarked and `cleanup` freed them.
        // Gray them as roots, as HotSpot's implicitly-live objects' referents
        // are always marked: over-retention for one cycle, never an under-mark.
        if !lowered_tams_grays.is_empty() {
            let regions = self.regions.read();
            for &addr in &lowered_tams_grays {
                self.push_gray_or_mark(&regions, addr);
            }
        }
        if lowered_tams_walk_breaks > 0 {
            tracing::debug!(
                "g1 initial mark: {lowered_tams_walk_breaks} lowered-TAMS walks stopped at an \
                 extent they could not trust ({} objects grayed)",
                lowered_tams_grays.len()
            );
        }
        // INT-8: stale skip entries from a previous cycle must never hide a
        // reused address's slot 0, so the set is REPLACED here by this
        // cycle's (gci i14/gq G-4: before the overlay grays below and before
        // the marker is spawned; empty when the caller publishes later).
        // No marker runs here: the previous controller was joined by
        // `VmHeap::g1_start_concurrent_mark` and the new one is not spawned.
        self.set_reference_skip_set(reference_objs);
        // G1AUD-2 — the queue MUST already be live before the phase becomes
        // marking-active: the store paths read their old slot value on the
        // phase and retain it on the queue, so flipping the phase first would
        // silently drop every edge overwritten in between. See
        // `satb_pre_barrier_required` for the full statement of the invariant.
        debug_assert!(
            self.satb_queue.is_active(),
            "start_concurrent_mark: the SATB queue must be activated BEFORE the phase \
             becomes marking-active"
        );
        // The overlay rows' SATB snapshot, before any marker can scan an
        // owner and after the gray set was cleared (an incomplete provider's
        // rows are grayed here as roots). See `g1_overlay.rs`.
        self.capture_overlay_snapshot();
        // g1ep7: the root regions, after the skip set and the overlay rows the
        // scan honours, before any mutator can overwrite them.
        if let Some(objs) = root_region_objs {
            self.scan_root_region_objects(&objs);
        }
        self.gc_state.set_phase(ConcurrentGcPhase::ConcurrentMark);
    }

    /// INT-8: publish the referent-slot skip set for the cycle that
    /// [`Self::start_concurrent_mark`] just opened. `addrs` are the
    /// Weak/Soft/Phantom `Reference` OBJECT addresses from the VM's
    /// `ReferenceProcessor` registry, snapshotted inside the same
    /// initial-mark STW (so no mutator can move or free them between the
    /// snapshot and this publish). See the `reference_skip` field doc.
    pub fn set_reference_skip_set(&self, addrs: &[usize]) {
        let mut skip = self.reference_skip.lock();
        skip.clear();
        skip.extend(addrs.iter().copied());
        // Published AFTER the set is filled: a marker that sees the new length
        // then takes the lock and finds the entries. The reverse order would
        // let it see a non-zero length over an empty set, which is harmless,
        // and — worse — a zero over a full one, which is not.
        self.reference_skip_len.store(skip.len(), Ordering::Release);
    }

    /// Test/diagnostic: current size of the referent-slot skip set.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn dbg_reference_skip_len(&self) -> usize {
        self.reference_skip.lock().len()
    }

    /// Test/diagnostic: does the skip set contain `addr`?
    #[cfg(test)]
    pub fn dbg_reference_skip_contains(&self, addr: usize) -> bool {
        self.reference_skip.lock().contains(&addr)
    }

    /// INT-8: carry the referent-slot skip set across an evacuation pause.
    /// Survivors are re-keyed through `pointer_map`; entries that sat in a
    /// CSet region and were NOT forwarded died with their region — they are
    /// PRUNED, because their address can be recycled later in the cycle and
    /// a stale entry would then hide the slot 0 of whatever innocent object
    /// reuses the memory (under-marking → freed-live corruption). A
    /// self-forwarded kept-region object maps to itself and is retained.
    /// Pruning a live entry is impossible by construction (non-CSet objects
    /// do not move and CSet survivors are always in the map), but even if a
    /// wedged drain dropped one, the failure mode is over-retention of its
    /// referent for one cycle — never under-marking.
    ///
    /// Called with the pause's regions lock held, after evacuation composed
    /// the final `pointer_map` (same protocol point as the monitor-table
    /// remap). No-op when no cycle is active (set empty).
    pub fn remap_reference_skip_set(
        &self,
        cset_set: &RegionSet,
        pointer_map: &cratonvm_types::PointerMap,
    ) {
        let mut skip = self.reference_skip.lock();
        if skip.is_empty() {
            return;
        }
        let old: Vec<usize> = skip.drain().collect();
        // The set is transiently empty across this rebuild, but the caller is
        // stop-the-world so no marker can observe it. The length is republished
        // at the end.
        for addr in old {
            if let Some(&new_addr) = pointer_map.get(&addr) {
                skip.insert(new_addr);
            } else {
                let in_cset = self
                    .lookup_region_for_addr(addr)
                    .is_some_and(|idx| cset_set.contains(&idx));
                if !in_cset {
                    skip.insert(addr); // untouched region — object did not move
                }
                // else: died in the CSet — prune.
            }
        }
        self.reference_skip_len.store(skip.len(), Ordering::Release);
    }

    /// G1CORE-6 — carry the string-deduplication table across an evacuation
    /// pause, exactly as [`Self::remap_reference_skip_set`] carries the
    /// referent-slot skip set.
    ///
    /// The table stores RAW canonical object addresses and nothing used to
    /// remap or purge them, which made the whole feature a use-after-free
    /// waiting for its first caller: a canonical String registered while it sat
    /// in Eden is evacuated (or freed) by the very next young pause, and a
    /// later HIT then hands back a from-space address the caller installs as a
    /// live reference. The API's own doc carried a "DO NOT WIRE UP without a
    /// remap" warning naming this function's absence as the reason.
    ///
    /// The three cases are the same three the skip set has, and they resolve
    /// the same way:
    ///
    /// * forwarded — re-key to the new address;
    /// * not forwarded, outside the CSet — the object did not move, keep it;
    /// * not forwarded, inside the CSet — it died with its region. DROP the
    ///   entry. Keeping it is the actual hazard: the address is recycled by the
    ///   next allocation, so a later hit would canonicalize every equal string
    ///   onto an unrelated object.
    ///
    /// Cheap when the feature is off (the table is empty and this is one
    /// `is_empty` behind an uncontended lock), which is why it is unconditional
    /// rather than gated on `string_dedup_enabled` — a table that was populated
    /// and then disabled must still be carried or purged, never left stale.
    ///
    /// Called with the pause's regions lock held, after evacuation composed the
    /// final `pointer_map`.
    pub fn remap_string_dedup_table(
        &self,
        cset_set: &RegionSet,
        pointer_map: &cratonvm_types::PointerMap,
    ) {
        let mut table = self.string_dedup_table.lock();
        if table.is_empty() {
            return;
        }
        table.retain(|_, addr| {
            if let Some(&new_addr) = pointer_map.get(addr) {
                // Includes the self-forwarded (evacuation-failure) case, whose
                // entry maps to itself.
                *addr = new_addr;
                return true;
            }
            match self.lookup_region_for_addr(*addr) {
                Some(idx) => !cset_set.contains(&idx),
                // Not a heap address at all — a synthetic entry from a unit
                // test, or a caller that registered something that is not an
                // object. Nothing can make it stale, so nothing here can fix
                // it either; leave it alone.
                None => true,
            }
        });
    }

    /// G1CORE-6 — drop dedup entries whose canonical object did not survive the
    /// mark cycle.
    ///
    /// The pause-time remap above handles objects that moved or died in a
    /// collection set. A canonical String promoted to Old dies a different way:
    /// `cleanup` frees its whole region in place, or a later mixed pause
    /// collects it. The first of those leaves no forwarding entry to follow, so
    /// the pause-time remap cannot see it — this is the pass that can, because
    /// it runs where the mark bitmap is still authoritative.
    ///
    /// Conservative in the safe direction: an entry is dropped unless its
    /// region is live AND the address is below the cursor. Dropping a live
    /// entry only costs a missed dedup opportunity; keeping a dead one is the
    /// UAF.
    pub fn purge_dead_string_dedup_entries(&self, regions: &[G1Region]) {
        let mut table = self.string_dedup_table.lock();
        if table.is_empty() {
            return;
        }
        table.retain(|_, addr| match self.lookup_region_for_addr(*addr) {
            Some(idx) => match regions.get(idx) {
                Some(r) if r.region_type() == RegionType::Free => false,
                Some(r) => {
                    let base = r.data.as_ptr() as usize;
                    *addr >= base && *addr < base + r.cursor()
                }
                None => false,
            },
            None => true,
        });
    }

    /// INT-8: post-remark liveness verdict for reference processing. Valid
    /// ONLY between the final remark's fixed-point drain and `cleanup()`
    /// (the mark bitmap is complete and nothing has been freed yet).
    /// Mirrors cleanup's own liveness walk:
    /// - not a heap address / no cycle data → LIVE (never claim dead
    ///   without positive evidence);
    /// - region freed earlier in the cycle → DEAD;
    /// - region recycled/re-typed since the mark-start snapshot → LIVE
    ///   (its entire content postdates the snapshot);
    /// - allocated above the snapshot cursor (TAMS) → LIVE;
    /// - otherwise → the region's mark-bitmap verdict.
    pub fn is_live_after_mark(&self, addr: usize) -> bool {
        let regions = self.regions.read();
        let Some(idx) = self.region_for_ptr(&regions, addr as *mut u8) else {
            return true;
        };
        let region = &regions[idx];
        if region.region_type() == RegionType::Free {
            return false;
        }
        // Same regions→snapshot lock order as `cleanup` / `start_concurrent_mark`.
        let snap = self.mark_start_snapshot.lock();
        Self::live_against_mark_snapshot(region, idx, snap.as_slice(), addr)
    }

    /// The snapshot half of [`Self::is_live_after_mark`] (gci i22/g1rk: one
    /// body for it and [`RemarkLiveness::is_live`], so the two verdicts cannot
    /// drift). `region` is region `idx`, not Free; `snap` is
    /// `mark_start_snapshot` as of the question.
    pub fn live_against_mark_snapshot(
        region: &G1Region,
        idx: usize,
        snap: &[(u64, usize, RegionType)],
        addr: usize,
    ) -> bool {
        if snap.is_empty() {
            return true;
        }
        match snap.get(idx) {
            Some(&(epoch, snap_cursor, snap_type))
                if epoch == region.reuse_epoch && snap_type == region.region_type() =>
            {
                let base = region.data.as_ptr() as usize;
                if addr.wrapping_sub(base) >= snap_cursor {
                    return true; // TAMS: allocated after mark start
                }
                region.mark_bitmap.is_marked(addr)
            }
            _ => true,
        }
    }

    /// gci i22/g1rk (item 7 of `gch-h4g1-g1-review-findings-FIXED-20261017`): a
    /// [`Self::is_live_after_mark`] for MANY questions in one stop-the-world
    /// window. `is_live_after_mark` locks the mark-start snapshot mutex on
    /// every call, and the remark asks it per reference row, closure node,
    /// class mirror, loader, JNI weak global and pending inherited bucket. The
    /// view copies the snapshot once (one row per region) and answers from the
    /// copy; per call it takes only the regions read guard, which it does NOT
    /// hold between calls (the remark callback marks, drains and may allocate,
    /// and those take the regions lock, the write side included).
    ///
    /// Same verdict as `is_live_after_mark` for as long as the snapshot does
    /// not change, i.e. inside one pause that neither starts, aborts nor
    /// cleans up a cycle (the snapshot's only writers:
    /// `start_concurrent_mark`, `abort_concurrent_mark`, `cleanup`'s
    /// `CRATONVM_G1_CLEANUP_RESET_TAMS` arm). Pinned by
    /// `gci_i22_rk_the_view_answers_as_is_live_after_mark`.
    pub fn remark_liveness(&self) -> RemarkLiveness<'_> {
        RemarkLiveness {
            collector: self,
            snapshot: self.mark_start_snapshot.lock().clone(),
        }
    }

    /// gci i22/g1rk (`gch-h4g1-proposal-g1-remark-sweeps-every-address-keyed-table-mark-exact-20261009`):
    /// the [`RemarkLiveness`] of the last completed mark, AFTER its `cleanup`,
    /// or `None` when that mark is no evidence of death. `Some` only when
    /// [`MarkProvenDead`]'s own conditions hold (`mark_proven_dead`): the
    /// mark covers every JIT-pinned object (`mark_proof_covers_jit_pins`), no
    /// cycle is marking, the cycle completed and `cleanup` trusted its closure
    /// (`last_mark_trusted`: an untrusted closure freed nothing and proves
    /// nothing), and the snapshot is still there (the
    /// `CRATONVM_G1_CLEANUP_RESET_TAMS` arm clears it).
    ///
    /// Valid after `cleanup` until the next allocation can reuse a freed
    /// region: `cleanup` only frees (a freed region reads Free, so DEAD, which
    /// is right for a key inside it) and leaves the surviving regions' bitmaps
    /// and incarnations as the remark left them. The remark doors ask it
    /// inside the same pause, before the world resumes.
    pub fn trusted_mark_liveness(&self) -> Option<RemarkLiveness<'_>> {
        if !mark_proof_covers_jit_pins(g1_mark_root_screen(), true) {
            return None;
        }
        if self.gc_state.is_marking_active()
            || !self.marking_complete.load(Ordering::Acquire)
            || !self.last_mark_trusted.load(Ordering::Acquire)
        {
            return None;
        }
        let view = self.remark_liveness();
        if view.snapshot.is_empty() {
            return None;
        }
        Some(view)
    }

    /// INT-8: resurrect objects the remark-time reference processor decided
    /// to hand out (dead finalizables about to be finalize()d, pending
    /// cleaner chains, policy-retained soft referents): mark each gray and
    /// drain the closure to a fixed point so this cycle's `cleanup` cannot
    /// free them or anything they reach. Must run between the remark drain
    /// and `cleanup()`. Addresses outside a live region are skipped — the
    /// caller's own staleness guards already dropped those.
    pub fn resurrect_after_remark(&self, addrs: &[usize]) {
        if addrs.is_empty() {
            return;
        }
        {
            let regions = self.regions.read();
            for &addr in addrs {
                if let Some(idx) = self.region_for_ptr(&regions, addr as *mut u8) {
                    if regions[idx].region_type() != RegionType::Free {
                        self.push_gray_or_mark(&regions, addr);
                    }
                }
            }
        }
        self.drain_marking_to_fixed_point();
    }

    /// g1ep7: drain the gray set to a fixed point inside a stop-the-world
    /// pause, on the evacuation pool: one job per marking deque, each a
    /// [`Self::concurrent_mark_step_worker`] loop (the steps steal from each
    /// other's deques and answer `true` only at the shared fixed point, as the
    /// concurrent markers' do). The final remark used to drain on the pausing
    /// thread alone: a cycle closed before its markers finished (a starved
    /// heap's allocation-failure door) marked the rest of the heap serially,
    /// most of the remark's time on `G1epKeepRounds 10 -Xmx1g`.
    /// `CRATONVM_G1_PARALLEL_REMARK_DRAIN` (default ON, `=0` drains serially).
    pub fn drain_marking_to_fixed_point(&self) {
        let pool = self.evac_pool();
        let jobs = if g1_parallel_remark_drain_enabled() {
            (pool.helpers() + 1).min(self.mark_deques.len())
        } else {
            1
        };
        if jobs <= 1 {
            while !self.concurrent_mark_step(usize::MAX) {}
            return;
        }
        let body = |w: usize| {
            let mut idle = 0u32;
            while !self.concurrent_mark_step_worker(w, usize::MAX) {
                idle = idle.wrapping_add(1);
                if idle % 64 == 0 {
                    std::thread::yield_now();
                } else {
                    std::hint::spin_loop();
                }
            }
        };
        pool.scope(jobs - 1, &body, || body(jobs - 1));
    }

    /// INT-8: store a field WITHOUT firing the SATB pre-barrier. Reserved
    /// for the weak-reference PROTOCOL writes — the pre-collection referent
    /// null pass (whose value is restored before mutators resume, so the
    /// snapshot graph is unchanged) and the remark-time referent clears
    /// (whose edge removal is the reference processor's decided verdict,
    /// which SATB must not resurrect). Every semantic mutator store must
    /// keep using `set_field`. Bounds/humongous handling and the RSet
    /// post-barrier are identical to `set_field` — only the pre-barrier is
    /// suppressed, via a same-thread RAII scope around the normal path.
    pub fn set_field_no_satb(&self, obj: ObjectRef, index: usize, value: Value) {
        let _guard = SatbPreSuppressGuard::new();
        <Self as cratonvm_gc_core::collector::GarbageCollector>::set_field(self, obj, index, value);
    }

    /// Perform an incremental step of concurrent marking.
    ///
    /// Audit fix (HIGH-3): this used to walk every region and call
    /// `try_mark` on every header — no roots, no transitive closure, so
    /// G1 reported every allocated object as live. The new implementation
    /// does real tri-color marking by draining the persistent
    /// `mark_worklist`:
    ///
    /// 1. Pop an object address from the worklist (the gray set).
    /// 2. Mark it in the bitmap.
    /// 3. Scan its reference slots — for each unmarked old/young heap
    ///    object it points at, mark it gray (push onto the worklist).
    ///
    /// The worklist is seeded by `remark` (which the interpreter calls
    /// at initial-mark STW and at the final remark STW). This way the
    /// worker thread that calls `concurrent_mark_step` in a loop simply
    /// drains the gray set produced by the roots.
    ///
    /// `work_amount` is the maximum number of objects to scan in this
    /// step. Returns `true` when the whole gray set is empty (marking is done).
    ///
    /// This is worker 0's entry point. It is what `remark`'s
    /// `while !concurrent_mark_step(usize::MAX) {}` drains through, and what
    /// every existing caller and test uses; with one worker configured it is
    /// the entire marker. See [`Self::concurrent_mark_step_worker`].
    pub fn concurrent_mark_step(&self, work_amount: usize) -> bool {
        self.concurrent_mark_step_worker(0, work_amount)
    }

    /// gci i12/wb (`gci-i12wb-jit-barrier-review-findings-FIXED-20261015` item 1):
    /// scan, INSIDE the concurrent-start pause, every object a thread the xt
    /// takeover froze holds, so the values its fields hold at the snapshot are
    /// gray before that thread resumes. G1's twin of the generational
    /// `ConcurrentMarker::initial_mark_with_frozen`
    /// (`gengc-r4w2-concmark-jit-gate-takeover-window-FIXED-20260928`).
    ///
    /// # The window it closes
    ///
    /// Compiled reference stores decide the SATB pre-barrier BEFORE the store:
    /// the g1put gated `putfield` arms of both tiers read the pre-gate byte and
    /// skip the old value when it is clear (`CRATONVM_JIT_IR_G1_GATED_REF_STORE`,
    /// default ON), and the single-pass `aastore` calls the SATB helper (which
    /// answers "inactive") a few instructions before its inline store. The
    /// takeover freezes a compiled peer at ANY instruction, so a peer can sit
    /// between that decision and its store across the pause that activates the
    /// queue. Resumed, it overwrites `O.f == A` without logging `A`; if another
    /// thread copied `A` into an object the marker already scanned, `A` is
    /// reachable, unmarked, and freed at cleanup. `remark` only pushes `O`
    /// gray, and the marker scans it later, after the overwrite.
    ///
    /// The frozen peer holds `O` (the receiver or array register is live from
    /// the decision to the store in every emitter), so `O` is among the
    /// takeover's conservative roots; scanning it here grays `A`. A root that
    /// names no object, or one the grid cannot place, is skipped: `remark`
    /// already took the cleanup fail-safe for it. A reference array is scanned
    /// whole (every chunk); the marker scans the continuation entries again,
    /// which only re-marks what is marked. Returns the objects scanned.
    pub fn scan_frozen_held_objects(
        &self,
        _stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        frozen: &[ObjectRef],
    ) -> usize {
        if frozen.is_empty() {
            return 0;
        }
        let regions = self.regions.read();
        let mut worklist = self.mark_worklist.lock();
        let mut grids = MarkRootGrids::default();
        let mut scanned = 0usize;
        for root in frozen {
            let p = root.as_ptr() as usize;
            if p == 0 {
                continue;
            }
            let start = match self.mark_root_seed(&regions, p, &mut grids) {
                MarkRootSeed::Object(start) => start,
                MarkRootSeed::Nothing | MarkRootSeed::Unresolved => continue,
            };
            let Some(idx) = self.lookup_region_for_addr(start) else {
                continue;
            };
            let Some(region) = regions.get(idx) else {
                continue;
            };
            // The marker's own gray gate, so nothing is decoded here that a
            // gray pop would refuse.
            if classify_mark_scan_target(region, start, &self.mark_gray_without_header_flag)
                .is_err()
            {
                continue;
            }
            // SAFETY: the gray gate just accepted `start` as an allocated
            // object start inside `region`, and the world is stopped.
            let header = unsafe { &*(start as *const ObjectHeader) };
            let chunks = if header.kind() == ObjectKind::Array {
                if header.element_type() != ArrayElementType::Reference {
                    continue; // no references to gray
                }
                let len = header.array_length() as usize;
                len.div_ceil(MARK_ARRAY_CHUNK).clamp(1, GRAY_CHUNK_MAX + 1)
            } else {
                1
            };
            for chunk in 0..chunks {
                self.scan_object_refs(
                    start as *mut u8,
                    header,
                    &regions,
                    MARK_WORKLIST_CAP,
                    &mut worklist,
                    chunk,
                    None,
                );
            }
            scanned += 1;
        }
        drop(worklist);
        drop(regions);
        if scanned > 0 {
            self.wake_marker();
        }
        scanned
    }

    /// F-12 — one marking worker's step.
    ///
    /// `worker` indexes [`Self::mark_deques`]; each background thread owns one
    /// index for its life, and the coordinator's own drain uses 0. The step
    /// takes work in this order:
    ///
    /// 1. its own deque (pop from the back — the entries it pushed most
    ///    recently, which are the ones still in cache);
    /// 2. the shared seed queue, `MARK_REFILL_CHUNK` at a time;
    /// 3. a peer's deque, half of it, taken from the FRONT so a thief and its
    ///    victim work opposite ends and do not fight over the same entries.
    ///
    /// Returns `true` only at a GLOBAL fixed point: every deque and the seed
    /// queue are empty, no other worker is mid-scan ([`ActiveMarker`]), and no
    /// overflow rescan is pending. Any weaker test would let one worker declare
    /// convergence while another still holds a live subtree.
    pub fn concurrent_mark_step_worker(&self, worker: usize, work_amount: usize) -> bool {
        let worker = worker % self.mark_deques.len();
        if work_amount == 0 {
            return self.gray_set_is_empty();
        }

        // G1MARK-6: pull mutator SATB overwrites into the gray set NOW
        // rather than letting them pile up in the shard queue until a young
        // pause or the final remark drains them. A mutation-heavy but
        // allocation-free phase (in-place updates of a preallocated working
        // set) triggers no young pauses, so the shards — which have no size
        // bound — grew without limit and the eventual remark pause was
        // O(all entries). Draining from the marker keeps the queue bounded
        // by the marker's cadence and shrinks the final remark. Entries
        // logged after this drain are picked up on the next step; the
        // gray-set/worklist protocol is the same one remark uses.
        // (Must run BEFORE the worklist lock below.)
        //
        // LANE-C — this used to be `for addr in drain() { push_gray_or_mark }`,
        // which is three shared locks PER ENTRY: the regions read guard is
        // hoisted, but every entry took the seed-queue mutex, the
        // `mark_waker` mutex and the waker's `parked` mutex, and issued a
        // `notify_all` on the mark condvar. A mutation-heavy phase delivers
        // these in thousands — that is the whole reason G1MARK-6 drains here —
        // so the marker's own SATB handoff was N condvar storms and N
        // acquisitions of the one lock every peer worker refills from. It now
        // takes the seed queue ONCE for the whole batch and wakes ONCE at the
        // end.
        //
        // The `has_pending()` gate in front of it matters at least as much: the
        // drain itself takes all sixteen SATB shard mutexes, and this runs once
        // per 256 scanned objects per marking worker. On a phase that overwrites
        // no references — which is most of them — the marker was paying 16
        // uncontended-but-real acquisitions per step to discover nothing, on the
        // exact locks the mutator write barrier flushes into.
        //
        // Entries are also SCREENED here rather than pushed blind. The old path
        // queued any address, including 0 and addresses in no region at all,
        // and let the scan loop's `region_for_ptr`/`classify_mark_scan_target`
        // reject them one gray pop later — which costs a gray slot against
        // `MARK_WORKLIST_CAP` and an `mark_oob_gray_skips` count for something
        // that was never a candidate. Screening here is the same test `remark`
        // already applies to its own SATB drain.
        if self.satb_queue.has_pending() {
            let entries = self.satb_queue.drain();
            if !entries.is_empty() {
                let regions = self.regions.read();
                let mut worklist = self.mark_worklist.lock();
                let mut overflowed = false;
                for addr in entries {
                    let Some(idx) = self.lookup_region_for_addr(addr) else {
                        continue;
                    };
                    let Some(region) = regions.get(idx) else {
                        continue;
                    };
                    if region.region_type() == RegionType::Free
                        || region.mark_bitmap.is_marked(addr)
                        || self.root_region_skips(region)
                    {
                        continue;
                    }
                    if worklist.len() < MARK_WORKLIST_CAP {
                        self.note_gray(addr, "satb-step-drain", 0);
                        worklist.push(addr);
                    } else {
                        // Seed-class entry at the cap: marking it black without
                        // scanning is the recovery contract (`remark`'s
                        // G1MARK-7 note) — a plain drop is unrecoverable
                        // because the overflow rescan only re-walks MARKED
                        // objects and no marked object need name a SATB seed.
                        // r11w19 g1store (W18-3): through the gray gate first.
                        if self.cap_arm_seed_passes_gray_gate(region, addr) {
                            region.try_mark_and_account(addr);
                        }
                        overflowed = true;
                    }
                }
                drop(worklist);
                drop(regions);
                if overflowed {
                    self.mark_worklist_overflowed.store(true, Ordering::Relaxed);
                }
                // One wake for the batch: the peers need to know work exists,
                // not how much of it there is.
                self.wake_marker();
            }
        }

        // F-10 — the drain runs in BATCHES, and the regions lock is released
        // between them.
        //
        // This function used to take the regions lock once and hold it until
        // the whole `work_amount` budget was spent — 256 objects for the
        // background worker (`WORKER_STEP_BUDGET`), unbounded for the
        // drain-to-fixed-point callers. Everything else that touches the region
        // table queued behind that: every allocation, every write-barrier slow
        // path, and the entire stop-the-world pause. A "concurrent" marker that
        // holds the allocator's lock is not racing the mutators, it is taking
        // turns with them — and it does so precisely when the heap is filling
        // fastest, because the only reason the marker is running at all is that
        // occupancy crossed IHOP.
        //
        // What the marker actually needs the lock for is one object at a time:
        // `region_for_ptr` + `classify_mark_scan_target` + `try_mark_and_account`
        // + `scan_object_refs` all read a `&G1Region` whose backing buffer, base
        // address and bitmap are stable for the collector's lifetime. The lock
        // is what keeps a region from being RETYPED or RESET underneath the
        // scan, and that only happens under the write guard, so a batch boundary
        // is a perfectly good place to let one in.
        //
        // `parking_lot`'s `RwLock` is task-fair: a waiting writer blocks new
        // readers, so dropping the guard here really does hand the lock to a
        // pause or a region claim rather than being immediately re-barged by
        // this thread. That is the whole mechanism — see the note on the
        // `regions` field.
        //
        // Batch size is a latency/overhead trade, not a correctness knob. 32
        // object scans is on the order of a microsecond, against a lock
        // acquire/release of a few tens of nanoseconds; a step of 256 pays 8
        // acquisitions instead of 1 and in exchange bounds a pause's wait on
        // the marker to ~1/8 of a step. `CRATONVM_G1_MARK_LOCK_YIELD=0` puts
        // the batch back to `usize::MAX`, i.e. exactly the old one-acquisition
        // step, which is the bisection lever for this change.
        //
        // **What would falsify the reasoning**: any invariant that requires the
        // marker's view of the region TABLE to be atomic across more than one
        // object. There is one candidate and it is already handled — the
        // post-pause `mark_worklist` remap in `young_collection`/
        // `mixed_collection` rewrites gray entries through the pause's
        // forwarding map; it runs under the regions WRITE guard, so it cannot
        // interleave with a batch, and a marker that resumes afterwards pops
        // from the already-remapped list.
        let batch = {
            MARK_LOCK_BATCH
        };
        let deque_cap = self.mark_deque_cap();
        // LANE W2-C — the class-loader side tables, read once per batch instead
        // of three times per object. `None` when the flag is off, which restores
        // the per-object lookups exactly. See `MarkSideTables` for the cost this
        // removes, the freshness argument, and why the snapshot is per-worker.
        let mut side_tables = gc_flags().g1_mark_side_tables.then(|| {
            MarkSideTables::capture(
                self.rset_cache_epoch.load(Ordering::Acquire),
                self.arena_base..self.arena_end,
            )
        });
        let mut remaining = work_amount;
        let mut scanned_total = 0usize;
        // F-12 — bounded spin guard. A thief can empty this worker's deque
        // between the refill and the drain, which costs an iteration that scans
        // nothing. That is fine and self-correcting (the thief now has the
        // work), but it must not be able to loop forever: after this many
        // consecutive fruitless rounds the step returns and the caller decides
        // whether to come back. `remaining` is only spent on real scans, so
        // without this the loop's own budget would never bound it.
        const IDLE_ROUNDS: usize = 16;
        let mut idle_rounds = 0usize;
        // JIT round 23 wave 8, lane L9 (`CRATONVM_G1_MARK_SIZE_MEMO`, OPT-IN
        // `=1` since the n23-w8g A/B): this step's size memo, held on this marker thread's
        // stack for the step (no thread-local, no global). Same
        // `layout_generation` invalidation as the cleanup walks'. On
        // `KeepLong -Xmx1g` the per-object layout lookup was 13.5% of a mark
        // worker's samples (n23-w8c).
        let mut sizes = WalkSizeMemo::with_enabled(g1_mark_size_memo_enabled());

        {
            // Held for the whole drain: see `ActiveMarker`. Dropped — including
            // on unwind — before the fixed-point test below, which is the one
            // place this worker must NOT count itself as active.
            let _active = ActiveMarker::new(&self.mark_active);

            while remaining > 0 && idle_rounds < IDLE_ROUNDS {
                let regions = self.regions.read();
                // LANE W2-C — re-capture the side tables if a pause has been
                // through since the last batch. Every collection path that can
                // reclassify or move a region bumps `rset_cache_epoch` as its
                // first act under the regions WRITE guard, and this batch holds
                // the READ guard for its whole duration — so a snapshot taken
                // under an unchanged epoch names the same addresses the scan
                // below will read. One relaxed-cost load per batch in the
                // steady state; a rebuild only when a pause actually happened.
                //
                // gcd d10/r: ...or if the `metadata_pin` rows changed outside a
                // pause. The initial mark files this cycle's rows AFTER the
                // worker started (`g1_start_concurrent_mark` precedes the root
                // scan), and the remark adds to them; neither moves the epoch.
                if let Some(tables) = side_tables.as_mut() {
                    let epoch = self.rset_cache_epoch.load(Ordering::Acquire);
                    if !tables.is_current(epoch) {
                        *tables = MarkSideTables::capture(epoch, self.arena_base..self.arena_end);
                    }
                }

                // Refill under the regions guard, so the deque-mutation discipline
                // (see the `mark_deques` field) covers stealing as well as
                // scanning. The emptiness probe is a separate statement on purpose:
                // taking the same deque's lock twice in one expression is a
                // deadlock waiting for a temporary-lifetime rule to change.
                let need_work = self.mark_deques[worker].lock().is_empty();
                if need_work {
                    let mut fresh = Vec::new();
                    if !self.take_marking_work(worker, &mut fresh) {
                        // Nothing in the seed queue and nothing to steal.
                        break;
                    }
                    self.mark_deques[worker].lock().extend(fresh);
                }

                let mut worklist = self.mark_deques[worker].lock();
                self.mark_lock_batches.fetch_add(1, Ordering::Relaxed);
                // Declared AFTER `regions` so it is dropped — and therefore
                // flushed — before the guard it borrows. See `MarkLiveBytes`.
                let mut live_bytes = MarkLiveBytes::new(&regions);
                let before = remaining;
                let mut in_batch = batch.min(remaining);

                while in_batch > 0 {
                    let obj_addr = match worklist.pop() {
                        Some(a) => a,
                        None => {
                            // This deque ran dry mid-batch. The outer loop refills
                            // it (from the seed queue, or by stealing) or gives up;
                            // `idle_rounds` bounds a thief/victim ping-pong.
                            break;
                        }
                    };
                    remaining -= 1;
                    in_batch -= 1;
                    // Item 9 — an entry is an address plus a chunk index; chunk 0 is
                    // the object itself, a higher chunk a continuation of a long
                    // reference array that is already marked.
                    let chunk = gray_entry_chunk(obj_addr);
                    let obj_addr = gray_entry_addr(obj_addr);

                    // Defensive: confirm this address really lives in some region.
                    // (Stale roots from before a heap rearrangement would otherwise
                    // dereference garbage.)
                    let obj_ptr = obj_addr as *mut u8;
                    let region_idx = match self.region_for_ptr(&regions, obj_ptr) {
                        Some(idx) => idx,
                        None => continue,
                    };
                    // g1ep7: never a young object in a root-region cycle (a
                    // push site that did not screen it).
                    if self.root_region_skips(&regions[region_idx]) {
                        continue;
                    }

                    // G1MARK-8: header-plausibility gate. Worklist entries are raw
                    // field bytes read by `scan_object_refs` — a corrupt or stale
                    // slot can name any in-region address. Region containment alone
                    // (above) still lets a wild pointer's garbage "header" drive the
                    // scan: its num_slots/array_length extent is walked and more
                    // garbage is pushed as children (corruption amplifier — the same
                    // class ZGC-4 closed with its registry check; G1 has no
                    // per-object registry, so the gate is geometric + header
                    // consistency instead). Skipping the scan can under-mark if the
                    // header was genuinely torn, so the flag makes `cleanup` retain
                    // everything this cycle.
                    if let Err(refusal) = classify_mark_scan_target(
                        &regions[region_idx],
                        obj_addr,
                        &self.mark_gray_without_header_flag,
                    ) {
                        // ONLY a torn header impugns the closure. `NotAllocated` says
                        // the address was never a live object in this region's current
                        // incarnation, so declining to scan it loses nothing -- and it
                        // is the COMMON case, because freed regions are not scrubbed
                        // and SATB deliberately retains objects that died mid-cycle.
                        // Driving `cleanup`'s retain-everything fail-safe from it made
                        // every reclamation decision in the cycle unavailable, which on
                        // `TestKillProcessWhileWriting` is the whole OOM: eager
                        // humongous reclaim declines every pause, collection sets go
                        // empty, and a 1 MiB `ByteBuffer.allocate` cannot be served on
                        // a 1 GiB heap that is mostly garbage.
                        let impugns = refusal == GrayRefusal::TornHeader || mark_oob_failsafe();
                        if impugns {
                            self.mark_saw_implausible.store(true, Ordering::Relaxed);
                        } else {
                            // Counted, not silent: a fail-safe that stops firing must
                            // not become a fail-safe nobody can see.
                            self.mark_oob_gray_skips.fetch_add(1, Ordering::Relaxed);
                        }
                        // The refusal alone cannot be acted on: it says WHICH of the
                        // two, not why the address exists. The region's own state
                        // separates a stale pointer into a recycled region (`cursor`
                        // far below `off`, `reuse_epoch` bumped) from a genuinely
                        // damaged one, and `CRATONVM_G1_DBG_GRAY_PROV=1` names the
                        // pusher and the parent object whose slot held it.
                        let r = &regions[region_idx];
                        let base = r.data.as_ptr() as usize;
                        let off = obj_addr.wrapping_sub(base);
                        let mut words = String::new();
                        for k in 0..4usize {
                            let a = obj_addr.wrapping_add(k * 8);
                            if a.wrapping_sub(base).wrapping_add(8) <= r.data.len() {
                                // SAFETY: the span was just bounded inside this region's
                                // own data buffer, which the regions lock holds alive;
                                // the read is 8-aligned because an unaligned `obj_addr`
                                // was refused above.
                                let v = unsafe { (a as *const usize).read() };
                                words.push_str(&format!(" [{}]=0x{v:x}", k * 8));
                            }
                        }
                        // WHETHER THE PARENT IS REACHABLE decides what this is. A
                        // marked parent holding a stale reference is an incomplete
                        // remembered set (a real UAF). An UNMARKED parent is an object
                        // SATB retained after it died, whose referent died with it --
                        // routine, and not a defect at all.
                        let prov = self.gray_prov_of(obj_addr).map(|(tag, parent)| {
                            let pstate = self
                                .lookup_region_for_addr(parent)
                                .map(|pi| {
                                    format!(
                                        "r{pi}/{:?}/marked={}",
                                        regions[pi].region_type(),
                                        regions[pi].mark_bitmap.is_marked(parent)
                                    )
                                })
                                .unwrap_or_else(|| "r?".to_string());
                            format!("{tag}<-0x{parent:x}[{pstate}]")
                        });
                        if self.mark_oob_report_budget() {
                            tracing::warn!(
                                concat!(
                                    "g1 concurrent mark: skipping gray entry {:#x} (region {}) ",
                                    "refusal={:?} impugns_cycle={} [type={:?} cursor={} off={} ",
                                    "len={} reuse_epoch={} recycled_in={} prov={:?} words:{}]"
                                ),
                                obj_addr,
                                region_idx,
                                refusal,
                                impugns,
                                r.region_type(),
                                r.cursor(),
                                off,
                                r.data.len(),
                                r.reuse_epoch,
                                r.recycled_in_generation(),
                                prov,
                                words,
                            );
                        }
                        continue;
                    }

                    // Round-2 fix (HIGH — GC #5): bitmaps now live per-region, so
                    // route the mark through the owning region's bitmap (which is
                    // keyed off that region's actual data pointer).
                    // Already black? Skip — nothing new to discover from it.
                    //
                    // F-12: `_measure` rather than `_account`, with the credit
                    // parked in `live_bytes` until the end of the batch. Same
                    // funnel, same numbers, one atomic per region per batch instead
                    // of one per object. `MarkLiveBytes` flushes on Drop, so the
                    // `continue` below cannot lose a credit.
                    if chunk == 0 {
                        // JIT round 23 wave 8, lane L9: the size through this
                        // step's memo (`CRATONVM_G1_MARK_SIZE_MEMO`).
                        match regions[region_idx].try_mark_and_measure_sized(obj_addr, &mut sizes) {
                            None => continue,
                            Some(bytes) => live_bytes.add(region_idx, bytes),
                        }
                    } else if !regions[region_idx].mark_bitmap.is_marked(obj_addr) {
                        // Item 9 — a continuation is only ever pushed by the scan
                        // of its own chunk 0, which marked the array first; an
                        // unmarked one is a stale entry a pause dropped the object
                        // of, and there is nothing to scan.
                        continue;
                    }

                    // Scan the object's reference fields and push gray successors.
                    // SAFETY: region_for_ptr above confirmed the address is inside
                    // a live region's data buffer; the header is therefore
                    // readable for the duration of the GC cycle (regions are
                    // pinned by the lock guard).
                    let header = unsafe { &*(obj_addr as *const ObjectHeader) };
                    self.scan_object_refs(
                        obj_ptr,
                        header,
                        &regions,
                        deque_cap,
                        &mut worklist,
                        chunk,
                        side_tables.as_ref(),
                    );
                }
                drop(worklist);
                let scanned = before - remaining;
                scanned_total += scanned;
                idle_rounds = if scanned == 0 { idle_rounds + 1 } else { 0 };
                // Both guards drop here. This is the window a waiting writer — an
                // STW pause, or an allocator claiming a fresh region — is admitted
                // through. See the batching note above.
            }
        }
        self.mark_worker_scans[worker].fetch_add(scanned_total, Ordering::Relaxed);

        // Re-take the guards to answer the "is marking done?" question.
        // Re-reading the worklist rather than carrying a stale emptiness across
        // the batch loop is deliberate: a mutator's SATB flush or an STW
        // pause's re-gray may have refilled it while the guards were down, and
        // reporting a fixed point that another thread has already invalidated
        // is exactly how a live subtree goes unscanned.
        {
            let _regions = self.regions.read();
            if !self.gray_set_is_empty() {
                // Ran out of budget but still have work — call again.
                return false;
            }
            // F-12 — a peer may hold a popped object whose children are not
            // pushed yet. Empty queues plus a live scanner is not a fixed
            // point; see `ActiveMarker`.
            if self.mark_active.load(Ordering::Acquire) != 0 {
                return false;
            }
            if !self.mark_worklist_overflowed.load(Ordering::Relaxed) {
                // Gray set empty, nobody scanning, no overflow: complete.
                return true;
            }
        }

        // Round-9 gc HIGH-5 — graceful overflow handling. If any push
        // was dropped earlier in this cycle, the transitive closure is
        // incomplete: dropped objects were never scanned, so their
        // children may be unmarked despite being live. Run a
        // conservative re-walk: for every marked object in every live
        // region, re-scan its outgoing references and push any unmarked
        // targets. The loop bounds the number of recovery passes; once
        // a full pass causes no new pushes the worklist drains and we
        // declare marking complete. The bitmap monotonically grows, so
        // this terminates after a bounded number of passes.
        //
        // F-11 — this rescan takes the WRITE guard, not the read guard the
        // drain above uses, and that is load-bearing rather than incidental.
        // It is the only walk over `[0, cursor)` in the collector that is not
        // stop-the-world, and once allocation bumps a cursor under a SHARED
        // guard (F-11) a concurrent walker can observe a cursor that already
        // covers a range whose zeroing has not landed yet. Under the exclusive
        // guard that window cannot be observed at all, which restores exactly
        // the exclusion this walk had when `regions` was a `Mutex`. The cost is
        // nil in practice: the rescan only runs after the gray set hit
        // `MARK_WORKLIST_CAP` (a million entries) and is already O(heap).
        let regions = self.regions.write();
        let mut worklist = self.mark_worklist.lock();
        if !worklist.is_empty() || !self.mark_deques_are_empty() {
            // Refilled while the guards were swapped — the caller steps again,
            // and the overflow flag is still set for a later rescan.
            return false;
        }
        // Reset the flag so we can detect re-overflow during recovery.
        // `swap` rather than load-then-store: between dropping the read guard
        // and taking the write guard another thread may have set it, and a
        // plain store would then clear an overflow nobody rescanned.
        if !self.mark_worklist_overflowed.swap(false, Ordering::Relaxed) {
            return true;
        }
        {
            let jit_skips = self.jit_tlab_skip_spans();
            // LANE W2-C — one capture for the whole rescan; see the call below.
            let rescan_side_tables = gc_flags().g1_mark_side_tables.then(|| {
                MarkSideTables::capture(
                    self.rset_cache_epoch.load(Ordering::Acquire),
                    self.arena_base..self.arena_end,
                )
            });
            let eden_by_bitmap = g1_overflow_rescan_eden_by_bitmap();
            for region in regions.iter() {
                if region.region_type() == RegionType::Free {
                    continue;
                }
                let base = region.data.as_ptr() as usize;
                // r11w18 g1store — an EDEN region is rescanned from its MARK
                // BITMAP, not by walking its object grid.
                //
                // This rescan runs on a marking worker, concurrently with the
                // mutators. The WRITE guard held here stops TLAB carves and
                // out-of-line allocation, but not the bumps inside a TLAB that
                // is already carved: `Tlab::alloc_initialized` and the compiled
                // inline `new` / `newarray` take no lock at all, and TLABs are
                // carved from Eden only (`refill_tlab`). So below an Eden
                // region's cursor there are zeroed TLAB tails (read as 16-byte
                // empty objects, which can overshoot an 8-byte-aligned tail end
                // into the next object) and headers being written. The grid
                // walk below breaks at the first such bytes and never rescans
                // the marked objects after them -- exactly the children the
                // rescan exists to recover
                // (`r11w17-g1store-overflow-rescan-walks-tlabs-mutators-are-filling`).
                //
                // Every mark bit names an address that already passed the gray
                // gate (`classify_mark_scan_target`): a popped gray, or, since
                // r11w19 (W18-3), a seed a cap arm marked
                // (`cap_arm_seed_passes_gray_gate`). Each is re-screened by that
                // same gate before its scan, as a gray pop is. The cursor is
                // read once: nothing that moves it can run under the write guard. Survivor, Old and humongous
                // regions receive objects only inside pauses or under the
                // regions lock, so their grids are quiescent here and keep the
                // walk below. `CRATONVM_G1_OVERFLOW_RESCAN_EDEN_BITMAP=0`
                // restores the grid walk for Eden too.
                if eden_by_bitmap && region.region_type() == RegionType::Eden {
                    if !region.mark_bitmap.any_marked() {
                        continue;
                    }
                    let cursor = region.cursor();
                    let mut offset = 0usize;
                    while offset < cursor {
                        let obj_addr = base + offset;
                        offset += 8;
                        if !region.mark_bitmap.is_marked(obj_addr) {
                            continue;
                        }
                        if let Err(refusal) = classify_mark_scan_target(
                            region,
                            obj_addr,
                            &self.mark_gray_without_header_flag,
                        ) {
                            if refusal == GrayRefusal::TornHeader || mark_oob_failsafe() {
                                self.mark_saw_implausible.store(true, Ordering::Relaxed);
                            } else {
                                self.mark_oob_gray_skips.fetch_add(1, Ordering::Relaxed);
                            }
                            continue;
                        }
                        // SAFETY: the gate above placed the header inside this
                        // region's allocated prefix and decoded it; the write
                        // guard keeps the region typed and mapped.
                        let header = unsafe { &*(obj_addr as *const ObjectHeader) };
                        self.scan_object_refs(
                            obj_addr as *mut u8,
                            header,
                            &regions,
                            MARK_WORKLIST_CAP,
                            &mut worklist,
                            0,
                            rescan_side_tables.as_ref(),
                        );
                    }
                    continue;
                }
                let mut offset = 0usize;
                while offset < region.cursor() {
                    let obj_addr = base + offset;
                    // INT-3 — frozen-peer TLAB tail: skip before interpreting.
                    if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, obj_addr) {
                        offset += skip;
                        continue;
                    }
                    // TLAB-retire gap sentinel: skip its exact span.
                    if let Some(gap) = gap_filler_len(obj_addr as *const u8) {
                        offset += gap;
                        continue;
                    }
                    // SAFETY: offset < cursor; header is within the region.
                    let header = unsafe { &*(obj_addr as *const ObjectHeader) };
                    if is_humongous_filler(header) {
                        break;
                    }
                    let obj_size = object_total_size(header);
                    if obj_size < HEADER_SIZE || offset + obj_size > region.cursor() {
                        break;
                    }
                    if region.mark_bitmap.is_marked(obj_addr) {
                        let obj_ptr = obj_addr as *mut u8;
                        // The rescan seeds the SHARED queue at the FULL cap: it
                        // IS the overflow recovery, so capping it per-deque
                        // would let the recovery overflow.
                        // LANE W2-C — the overflow rescan holds the regions
                        // WRITE guard for its whole duration, so no pause can
                        // interleave and one capture covers the entire walk.
                        self.scan_object_refs(
                            obj_ptr,
                            header,
                            &regions,
                            MARK_WORKLIST_CAP,
                            &mut worklist,
                            0,
                            rescan_side_tables.as_ref(),
                        );
                    }
                    offset += obj_size;
                }
            }
        }
        // Tell the caller to keep stepping — the rescan likely refilled the
        // worklist with newly-discovered references. Returning `false` causes
        // the marking loop to call us again, which drains what it produced.
        let converged =
            worklist.is_empty() && !self.mark_worklist_overflowed.load(Ordering::Relaxed);
        // LANE-C — and tell the PEERS, which this path never did. The rescan
        // has just seeded the shared queue with up to `MARK_WORKLIST_CAP`
        // entries, and every peer worker reached this point by finding the gray
        // set empty and parking. Item 9b's rule is that whoever creates gray
        // work wakes the markers; the overflow recovery is the single largest
        // producer in the collector and was the one place that left them on the
        // `WORKER_POLL_MS` fallback — so the recovery from an already-pathological
        // graph ran a quarter of a second per batch on one thread.
        //
        // Both guards are released first: `wake_marker` takes the waker mutex
        // and the waker's park mutex, and holding the regions WRITE guard
        // across them would widen a pause's wait on the marker for no reason.
        drop(worklist);
        drop(regions);
        if !converged {
            self.wake_marker();
        }
        converged
    }

    /// Scan an object's reference slots; for each in-heap, not-yet-marked
    /// target push the target onto `worklist`. This mirrors
    /// `ConcurrentMarker::scan_object` in `concurrent_mark.rs` but uses
    /// G1's per-region addressing (objects live inside `G1Region::data`).
    ///
    /// Round-2 fix (HIGH — GC #5): bitmaps are per-region. Instead of
    /// receiving a single global bitmap, this helper looks up the owning
    /// region for each reference and consults that region's bitmap to
    /// avoid pushing already-marked targets back onto the worklist. The
    /// `try_mark` in `concurrent_mark_step` is still the authoritative
    /// marker; the `is_marked` check here is only an optimization to
    /// reduce worklist churn.
    ///
    /// LANE W2-C — `side_tables` is the per-batch snapshot of the three
    /// class-loader registries this scan used to interrogate per object; see
    /// [`MarkSideTables`] for what that cost and why a snapshot is sound.
    /// `None` restores the per-object lookups exactly
    /// (`CRATONVM_G1_MARK_SIDE_TABLES` off, today's default).
    #[allow(clippy::too_many_arguments)]
    pub fn scan_object_refs(
        &self,
        obj_ptr: *mut u8,
        header: &ObjectHeader,
        regions: &[G1Region],
        cap: usize,
        worklist: &mut Vec<usize>,
        chunk: usize,
        side_tables: Option<&MarkSideTables>,
    ) {
        // CRIT-perf fix: use the O(log R) cached `lookup_region_for_addr`
        // helper instead of an O(R) linear walk. The wrapper preserves
        // the "skip Free regions" guard the linear version had by
        // checking the looked-up region's type.
        // g1ep7: a young child is not pushed in a root-region cycle.
        let skip_young = self.root_regions_cycle.load(Ordering::Relaxed);
        let region_for = |p: *mut u8| -> Option<usize> {
            let idx = self.lookup_region_for_addr(p as usize)?;
            let ty = regions[idx].region_type();
            if ty == RegionType::Free
                || (skip_young && matches!(ty, RegionType::Eden | RegionType::Survivor))
            {
                None
            } else {
                Some(idx)
            }
        };

        // C2 (round-12 gc): humongous objects are region-fragmented — reading a
        // ref slot at a flat offset from `obj_ptr` would read OOB past the
        // start region for any slot beyond the first region's payload. Detect
        // the humongous case once and read each ref slot through the same
        // region-aware translation used by the field/array accessors.
        let humongous_start: Option<(usize, usize)> = {
            match self.lookup_region_for_addr(obj_ptr as usize) {
                Some(idx) if regions[idx].region_type() == RegionType::HumongousStart => {
                    let total_size = object_total_size(header);
                    Some((idx, total_size.saturating_sub(HEADER_SIZE)))
                }
                _ => None,
            }
        };
        // UNRESOLVED FORK, blocking HEADER_SIZE = 16.
        //
        // `read_ref`/`read_value` below are shared between the array walk (which
        // passes element offsets) and the object walk (which passes compact
        // field offsets), so at 16 bytes this base has to become
        // ARRAY_DATA_OFFSET for one caller and HEADER_SIZE for the other. Both
        // shapes of that change -- `header.payload_offset()` in the closure, and
        // a `payload_base` hoisted out of it -- were tried on 2026-08-07 and
        // both left `cargo test -p cratonvm-gc --lib` timing out at 500s.
        //
        // That is NOT established as cause: the same suite also hung and also
        // failed `parallel_matches_serial_no_loss_or_dup` on runs with this code
        // untouched (6 runs: hang / 979 pass / hang / 978+1 / hang / hang), so
        // the g1 parallel set is unstable here independently and cannot serve as
        // the control. Left at HEADER_SIZE -- correct today, since the two
        // constants are equal -- until that instability is separated out.
        // Read an 8-byte ref word at logical payload offset `payload_off`.
        let read_ref = |payload_off: usize| -> u64 {
            if let Some((start, total_payload)) = humongous_start {
                let mut buf = [0u8; 8];
                if self.humongous_copy(
                    regions,
                    start,
                    total_payload,
                    payload_off,
                    buf.as_mut_ptr(),
                    8,
                    false,
                ) {
                    u64::from_ne_bytes(buf)
                } else {
                    0
                }
            } else {
                // SAFETY: caller guarantees the slot is within the object.
                let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + payload_off) };
                unsafe { std::ptr::read(slot_ptr as *const u64) }
            }
        };

        if header.kind() == ObjectKind::Array {
            if header.element_type() == ArrayElementType::Reference {
                // Reference array: 8-byte compact slot per element.
                //
                // Item 9 — scan this chunk only, and push the next chunk as a
                // continuation entry BEFORE the children so a LIFO pop drains
                // the children first (locality) and comes back for the rest.
                let len = header.array_length() as usize;
                let start = chunk.saturating_mul(MARK_ARRAY_CHUNK).min(len);
                let end = if chunk >= GRAY_CHUNK_MAX {
                    len
                } else {
                    start.saturating_add(MARK_ARRAY_CHUNK).min(len)
                };
                if end < len {
                    if worklist.len() >= cap {
                        // The overflow rescan re-scans every marked object from
                        // chunk 0, which re-derives this continuation.
                        self.mark_worklist_overflowed.store(true, Ordering::Relaxed);
                    } else {
                        worklist.push(gray_entry(obj_ptr as usize, chunk + 1));
                    }
                }
                for i in start..end {
                    let raw: u64 = read_ref(i * 8);
                    if raw == 0 {
                        continue;
                    }
                    let ref_ptr = raw as usize as *mut u8;
                    if let Some(idx) = region_for(ref_ptr) {
                        if !regions[idx].mark_bitmap.is_marked(ref_ptr as usize) {
                            // Round-9 gc HIGH-5: graceful overflow — drop
                            // the push and record the event so remark
                            // can run a conservative full re-walk.
                            if worklist.len() >= cap {
                                self.mark_worklist_overflowed.store(true, Ordering::Relaxed);
                            } else {
                                self.note_gray(
                                    ref_ptr as usize,
                                    "scan-child-refarray",
                                    obj_ptr as usize,
                                );
                                worklist.push(ref_ptr as usize);
                            }
                        }
                    }
                }
            }
            // Primitive arrays carry no references.
        } else {
            // INT-8: referent-slot hiding. A Weak/Soft/Phantom Reference
            // object (registered in `reference_skip` at mark start, re-keyed
            // across every evacuation pause) has its slot 0 — the referent —
            // EXCLUDED from the trace: weak reachability must not keep the
            // referent alive, or remark-time reference processing can never
            // see a dead verdict for it (the INT-8 bitmap taint). All other
            // slots (queue, next, discovered, subclass fields) trace
            // normally. Lock order: regions (held by every caller) →
            // reference_skip — same as `remap_reference_skip_set`'s callers.
            // F-12 — the length is read WITHOUT the lock. This runs once per
            // scanned object, and with N markers a global mutex here is a
            // convoy on the marker's hottest line rather than the few
            // nanoseconds it costs a single marker. The set is empty except
            // while a cycle has `Reference` objects registered, so the load
            // answers almost every scan on its own.
            let first_slot = if self.reference_skip_len.load(Ordering::Acquire) == 0 {
                0
            } else {
                let skip = self.reference_skip.lock();
                if skip.contains(&(obj_ptr as usize)) {
                    1
                } else {
                    0
                }
            };
            if cratonvm_types::is_compact_object(header) {
                // Borrowing accessor: the mark walk only reads `field_offsets` /
                // `is_ref`, so it need not pay an `Arc` clone/drop per marked
                // object.
                let _ = cratonvm_types::with_class_layout(
                    header.class_id.as_u32(),
                    header.num_slots(),
                    |layout| {
                        for (slot_idx, (&disp, &is_ref)) in layout
                            .field_disps
                            .iter()
                            .zip(layout.is_ref.iter())
                            .enumerate()
                        {
                            if !is_ref || slot_idx < first_slot {
                                continue;
                            }
                            // `disp` is ABSOLUTE (a compact instance's fields
                            // start at 8, inside what `read_ref` calls the
                            // header). A field in the first 16 bytes is in the
                            // object's first region, so it is read directly;
                            // the rest go through the payload-relative reader,
                            // which handles a humongous span.
                            let disp = disp as usize;
                            let raw = if disp < HEADER_SIZE {
                                // SAFETY: inside the object, in its first region.
                                unsafe { std::ptr::read(obj_ptr.add(disp) as *const u64) }
                            } else {
                                read_ref(disp - HEADER_SIZE)
                            };
                            if raw == 0 {
                                continue;
                            }
                            let ref_ptr = raw as usize as *mut u8;
                            if let Some(idx) = region_for(ref_ptr) {
                                if !regions[idx].mark_bitmap.is_marked(ref_ptr as usize) {
                                    if worklist.len() >= cap {
                                        self.mark_worklist_overflowed
                                            .store(true, Ordering::Relaxed);
                                    } else {
                                        self.note_gray(
                                            ref_ptr as usize,
                                            "scan-child-compact",
                                            obj_ptr as usize,
                                        );
                                        worklist.push(ref_ptr as usize);
                                    }
                                }
                            }
                        }
                    },
                );
            } else {
                // Legacy object: 16-byte Value slot per field.
                for slot_idx in first_slot..header.num_slots() as usize {
                    let payload_off = slot_idx * SLOT_SIZE;
                    let value = if let Some((start, total_payload)) = humongous_start {
                        let mut buf = [0u8; SLOT_SIZE];
                        if !self.humongous_copy(
                            regions,
                            start,
                            total_payload,
                            payload_off,
                            buf.as_mut_ptr(),
                            SLOT_SIZE,
                            false,
                        ) {
                            continue;
                        }
                        value_from_bytes(&buf)
                    } else {
                        let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + payload_off) };
                        // Discriminant-screened: see `heap::read_value_cell_checked`.
                        // Corrupt cells decode to `Value::Object(None)` and are
                        // skipped, instead of transmuting to a UB `Value` whose
                        // garbage pointer would be pushed onto the mark worklist.
                        unsafe {
                            cratonvm_gc_core::heap::read_value_cell_checked(
                                slot_ptr as *const Value,
                                "g1::concurrent_mark_step",
                            )
                        }
                    };
                    if let Value::Object(Some(ref_obj)) = value {
                        let ref_ptr = ref_obj.as_ptr();
                        if let Some(idx) = region_for(ref_ptr) {
                            if !regions[idx].mark_bitmap.is_marked(ref_ptr as usize) {
                                // Round-9 gc HIGH-5: graceful overflow — drop
                                // the push and record the event so remark
                                // can run a conservative full re-walk.
                                if worklist.len() >= cap {
                                    self.mark_worklist_overflowed.store(true, Ordering::Relaxed);
                                } else {
                                    self.note_gray(
                                        ref_ptr as usize,
                                        "scan-child-legacy",
                                        obj_ptr as usize,
                                    );
                                    worklist.push(ref_ptr as usize);
                                }
                            }
                        }
                    }
                }
            }
        }

        // Class-loader-data side edges. CratonVM stores these relationships in
        // VM side tables rather than traceable Java fields:
        //
        //   live instance -> defining loader
        //   live defining loader -> class mirrors and metadata oops
        //
        // Root gathering publishes the mirror/metadata tables only for G1's
        // initial/final full-mark snapshots, never for an evacuating young
        // pause. The concurrent marker can therefore follow them exactly like
        // ordinary object references without making the loader itself a root.
        let mut enqueue = |addr: usize| {
            let ptr = addr as *mut u8;
            if let Some(idx) = region_for(ptr) {
                if !regions[idx].mark_bitmap.is_marked(addr) {
                    if worklist.len() >= cap {
                        self.mark_worklist_overflowed.store(true, Ordering::Relaxed);
                    } else {
                        self.note_gray(addr, "scan-child-sidetable", obj_ptr as usize);
                        worklist.push(addr);
                    }
                }
            }
        };
        // The collection-overlay edge: this owner's rows as they stood at the
        // mark snapshot, followed like fields. Ahead of the loader tails below,
        // which return early. See `g1_overlay.rs`.
        if header.kind() != ObjectKind::Array {
            self.overlay_snapshot_edges(obj_ptr as usize, header.class_id.as_u32(), &mut enqueue);
        }
        //
        // LANE W2-C — two shapes, chosen by `CRATONVM_G1_MARK_SIDE_TABLES`.
        // The `Some` arm reads the batch's snapshot and takes no lock at all;
        // the `None` arm is the previous code, unchanged, which asks each
        // process-global registry about this one object. See
        // [`MarkSideTables`].
        match side_tables {
            Some(tables) => {
                // One branch for the overwhelmingly common "no user loader has
                // ever defined a class" case, instead of three map probes.
                if tables.is_empty() {
                    return;
                }
                if let Some(&loader) = tables
                    .loaders
                    .as_ref()
                    .and_then(|m| m.get(&header.class_id.as_u32()))
                {
                    // LANE W3-C — see [`MARK_LOADER_TAIL_HITS`]. Inside the
                    // `if let`, so an object with no loader edge pays nothing
                    // and the census cannot confound the A/B it serves.
                    MARK_LOADER_TAIL_HITS.fetch_add(1, Ordering::Relaxed);
                    enqueue(loader);
                }
                if let Some(mirrors) = tables
                    .mirrors
                    .as_ref()
                    .and_then(|m| m.get(&(obj_ptr as usize)))
                {
                    MARK_LOADER_TAIL_HITS.fetch_add(mirrors.len() as u64, Ordering::Relaxed);
                    for &mirror in mirrors {
                        enqueue(mirror);
                    }
                }
                if let Some(metadata) = tables
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get(&(obj_ptr as usize)))
                {
                    MARK_LOADER_TAIL_HITS.fetch_add(metadata.len() as u64, Ordering::Relaxed);
                    for &object in metadata {
                        enqueue(object);
                    }
                }
            }
            None => {
                // The row inside this heap when several live VMs share the
                // class id (`common-w4b` loader-pin collision): the arena
                // bounds are immutable and lock-free, as `snapshot_where`'s
                // predicate must be.
                let (arena_base, arena_end) = (self.arena_base, self.arena_end);
                if let Some(loader) = cratonvm_types::loader_pin::loader_pin_addr_where(
                    header.class_id.as_u32(),
                    |a| (arena_base..arena_end).contains(&a),
                ) {
                    MARK_LOADER_TAIL_HITS.fetch_add(1, Ordering::Relaxed);
                    enqueue(loader);
                }
                if let Some(mirrors) =
                    cratonvm_types::mirror_pin::mirrors_for_loader(obj_ptr as usize)
                {
                    MARK_LOADER_TAIL_HITS.fetch_add(mirrors.len() as u64, Ordering::Relaxed);
                    for mirror in mirrors {
                        enqueue(mirror);
                    }
                }
                if let Some(metadata) =
                    cratonvm_types::metadata_pin::roots_for_loader(obj_ptr as usize)
                {
                    MARK_LOADER_TAIL_HITS.fetch_add(metadata.len() as u64, Ordering::Relaxed);
                    for object in metadata {
                        enqueue(object);
                    }
                }
            }
        }
    }

    /// r11w19 g1store (proposal W18-3) — may a gray-set cap arm mark this SEED
    /// black without a scan?
    ///
    /// A seed under the cap is pushed, and the gray gate
    /// ([`classify_mark_scan_target`]) screens it one pop later: a refusal
    /// there skips it (routine `NotAllocated`) or skips it and impugns the
    /// cycle (`TornHeader`). A seed AT the cap used to skip that gate: the four
    /// cap arms (`remark`'s `push_with_cap`, the SATB keep-alive, the marker's
    /// SATB step drain and `push_gray_or_mark`) set its mark bit unscreened,
    /// so a seed that was not an object start became a mark bit -- counted
    /// into `live_bytes` from whatever its bytes decode to, and a start the
    /// overflow rescan then trusted. This asks the same gate the pop would
    /// have asked and records the same verdict, so every mark bit, cap arm or
    /// not, names an address that passed it.
    ///
    /// SATB values are read from reference slots, so a real value always
    /// passes; a refusal here means a stale or damaged value, and the cycle
    /// takes exactly the treatment that value would have got as a gray pop.
    /// `CRATONVM_G1_MARK_CAP_SEED_SCREEN=0` restores the unscreened mark.
    pub fn cap_arm_seed_passes_gray_gate(&self, region: &G1Region, addr: usize) -> bool {
        if !g1_mark_cap_seed_screen() {
            return true;
        }
        match classify_mark_scan_target(region, addr, &self.mark_gray_without_header_flag) {
            Ok(()) => true,
            Err(refusal) => {
                if refusal == GrayRefusal::TornHeader || mark_oob_failsafe() {
                    self.mark_saw_implausible.store(true, Ordering::Relaxed);
                } else {
                    self.mark_oob_gray_skips.fetch_add(1, Ordering::Relaxed);
                }
                false
            }
        }
    }

    /// r11w18 g1store — may a mark ROOT be pushed as it stands, i.e. is it a
    /// published object START?
    ///
    /// The marker trusts every gray address to be an object start: it decodes
    /// the header there and walks the slots that header declares
    /// (`classify_mark_scan_target` only checks the header is decodable and
    /// fits below the cursor). A root is not such an address by construction:
    /// `collect_roots` range-filters operand-stack words, and the frozen-peer
    /// and blocked-thread snapshots are conservative band scans of compiled
    /// frames, which hold derived and stale words. The measured shape (see
    /// `pinned_region_set_including_non_object_roots`, which met the same
    /// words on the evacuation side) is `obj + 8` of a compact instance: its
    /// eight bytes are a reference field, whose low half reads as a class id
    /// and whose high half as a flag-less, NEUTRAL legacy mark word; the word
    /// at `+8` then reads as `num_slots`. Pushed raw, the marker walked that
    /// many 16-byte "cells" across the rest of the object and its neighbours
    /// -- the `g1::concurrent_mark_step: corrupt Value cell` bursts, whose
    /// `raw0` is a neighbour's header -- and never marked the object the word
    /// actually lives in.
    ///
    /// So a root passes only when all of these hold:
    /// * the tag screen (`classify_candidate_header`: aligned, in a live
    ///   region below its cursor, tag bytes decode, sane shape);
    /// * the header carries `GC_FLAG_HEADER`, which every allocator stamps and
    ///   the words above never carry (the evacuator's root pin applies the
    ///   same requirement);
    /// * it is not a TLAB filler (dead space, see [`is_tlab_filler_header`]);
    /// * the evacuator's own second look (`addr_is_followable_object`: the
    ///   class-id band, the empty-header grid proof, the arena-pointer word).
    ///
    /// A root that fails is NOT dropped: [`Self::mark_root_seed`] places it in
    /// its region's object grid, so a real object that fails a screen is still
    /// seeded (as a grid `Start`), and a word that cannot be placed takes the
    /// cleanup fail-safe.
    pub fn mark_root_is_object_start(&self, regions: &[G1Region], addr: usize) -> bool {
        if self.classify_candidate_header(regions, addr).0 != HeaderVerdict::Object {
            return false;
        }
        // SAFETY: the screen above placed `addr` in a live region's allocated
        // prefix, 8-aligned, and validated its tag bytes.
        let header = unsafe { &*(addr as *const ObjectHeader) };
        if header.gc_flags() & cratonvm_types::GC_FLAG_HEADER == 0 || is_tlab_filler_header(header)
        {
            return false;
        }
        if !self.addr_is_followable_object(regions, addr, "mark-root") {
            return false;
        }
        // The G1 near-full proxy row (`CRATONVM_G1_MARK_ROOT_SCREEN_DECODES`):
        // the gray gate's own verdict too. A compiled frame's word into the
        // class-file bytes of a `byte[]` passed every screen above (its bytes
        // read as tags, the flag and a class id) and failed the gate's size
        // decode, so the pushed seed was refused as a torn header and every
        // cycle that met it retained every region and unloaded no class. A
        // word that fails here goes to the grid, which seeds the array that
        // contains it.
        let Some(idx) = self.lookup_region_for_addr(addr) else {
            return false;
        };
        classify_mark_scan_target(&regions[idx], addr, &AtomicU64::new(0)).is_ok()    }

    /// r11w18 g1store — what a mark ROOT keeps alive: the address
    /// [`Self::remark`] may push onto the gray set for it.
    ///
    /// * a root that passes [`Self::mark_root_is_object_start`] is its own
    ///   seed -- the old behaviour, byte for byte, for every real object;
    /// * a word in a humongous span seeds the span's object, which begins at
    ///   its start region's base (`claim_humongous_span_locked`); the
    ///   marker used to refuse a continuation word (`NotAllocated`) and decode
    ///   a start-region payload word as a header;
    /// * a word in no live region, unaligned, or at or above its region's
    ///   cursor names no allocated memory and seeds nothing (the marker refused
    ///   it as `NotAllocated` one pop later anyway);
    /// * any other word is placed in its region's object grid
    ///   ([`MarkRootGrid`], walked once per region per `remark` call): an
    ///   interior word seeds the object that CONTAINS it (a derived pointer
    ///   keeps its base alive), a word in a TLAB filler, gap sentinel or
    ///   published TLAB skip span seeds nothing, and a word the walk could not
    ///   reach is `Unresolved`.
    ///
    /// `Unresolved` (also returned past [`MARK_ROOT_GRID_REGION_BUDGET`]) seeds
    /// nothing and the caller sets `mark_saw_implausible`, so `cleanup` retains
    /// every region this cycle: the verdict an impugning gray refusal already
    /// gets, and never a walk of bytes that are not an object.
    ///
    /// # Why the grid is walkable here, and why the wave-17 form was reverted
    ///
    /// `remark` runs inside a stop-the-world pause whose takeover publishes
    /// every live thread's un-retired TLAB tail as a skip span
    /// (`stw_take_over_and_wait` -> `collect_reserved_tlab_tails`, all
    /// threads, the initiator included); a thread parked at a poll retired its
    /// TLAB into a filler on the way in. So below every cursor there are only
    /// objects, fillers, gap sentinels and skip spans, the same grid `cleanup`
    /// walks in the same pause.
    ///
    /// The wave-17 form of this function was reverted on a hang whose log
    /// showed `Phase-4 fixup REFUSED a HOLDER ... class_id=4044482304
    /// num_slots=442`. That holder is `0xF111_E700`, the TLAB filler class id,
    /// an ordinary `int[442]` filler ending its region; Phase 4 walks EVERY
    /// object whether marked or not, and refused it only because the
    /// arena-pointer screen paired its class id with its length (see
    /// [`header_first_word`], fixed in the same wave). The line is no evidence
    /// that anything marked it. The orchestrator's 20-run table on the same
    /// page records the hang on binaries WITHOUT the wave-17 change (`fix3`
    /// 1/20, `w17b` 2/20). What the wave-17 walk did get wrong is kept out
    /// here: it treated a TLAB filler as an object (a root inside one seeded
    /// the filler), and it trusted any header the tag screen accepted,
    /// flag-less `obj + 8` words included, as a start.
    pub fn mark_root_seed(
        &self,
        regions: &[G1Region],
        addr: usize,
        grids: &mut MarkRootGrids,
    ) -> MarkRootSeed {
        let Some(idx) = self.lookup_region_for_addr(addr) else {
            return MarkRootSeed::Nothing;
        };
        let Some(region) = regions.get(idx) else {
            return MarkRootSeed::Nothing;
        };
        let base = region.data.as_ptr() as usize;
        match region.region_type() {
            RegionType::Free => return MarkRootSeed::Nothing,
            // A humongous object begins at offset 0 of its start region and
            // nothing else is allocated in its span.
            RegionType::HumongousStart => return MarkRootSeed::Object(base),
            RegionType::HumongousContinuation => {
                // A continuation holds payload only; its object starts at the
                // base of the nearest `HumongousStart` below it.
                let mut i = idx;
                while i > 0 {
                    i -= 1;
                    match regions.get(i).map(|r| r.region_type()) {
                        Some(RegionType::HumongousContinuation) => continue,
                        Some(RegionType::HumongousStart) => {
                            return MarkRootSeed::Object(regions[i].data.as_ptr() as usize);
                        }
                        _ => break,
                    }
                }
                return MarkRootSeed::Unresolved;
            }
            RegionType::Eden | RegionType::Survivor | RegionType::Old => {}
        }
        if addr & 0x7 != 0 || addr < base || addr - base >= region.cursor() {
            return MarkRootSeed::Nothing;
        }
        if self.mark_root_is_object_start(regions, addr) {
            return MarkRootSeed::Object(addr);
        }
        let off = addr - base;
        let seed = match self.mark_root_grid_place(grids, region, idx, off) {
            Some(GridPlace::Start) => MarkRootSeed::Object(addr),
            Some(GridPlace::Interior(start_off)) => MarkRootSeed::Object(base + start_off),
            Some(GridPlace::Unallocated) => MarkRootSeed::Nothing,
            Some(GridPlace::Flagless) => {
                // Counted as the gray gate counts the seed it would have refused.
                self.mark_gray_without_header_flag
                    .fetch_add(1, Ordering::Relaxed);
                MarkRootSeed::Unresolved
            }
            Some(GridPlace::Desync) | None => MarkRootSeed::Unresolved,
        };
        let n = MARK_ROOT_PLACED.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 8 || n.is_power_of_two() {
            tracing::debug!(
                "[g1] mark root 0x{addr:x} is not a published object start (#{n}) -> {:?} [{}]",
                seed,
                self.describe_rejected_address(regions, addr),
            );
        }
        seed
    }

}
