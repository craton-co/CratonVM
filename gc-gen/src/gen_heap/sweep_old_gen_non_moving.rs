#[allow(unused_imports)]
use super::*;

impl GenerationalHeap {
    /// Run a non-moving mark-sweep of old space while conservative JIT roots are
    /// active.  The supplied roots are copied only because the shared marker's
    /// compacting mode can rewrite its mutable root slice; this mode never does.
    /// Returns `(bytes_reclaimed, watched_survivor_identity_map)`. The second
    /// element is the survival proof post-GC reference processing needs: this
    /// sweep frees dead old-gen blocks in place, so after it runs "the address
    /// is inside old gen" no longer implies "the object is still there".
    ///
    /// gen r4w5/oldcompact5 (2026-09-24): with `CRATONVM_GC_OLD_PINNED_COMPACT`
    /// (opt-in) a pending FRAGMENTATION compaction request is no longer
    /// discarded here: the collection compacts around pinned objects instead
    /// of sweeping in place (see [`OldPinnedCompact`]), and the returned map
    /// then also holds `old -> new` for every object that moved. Without the
    /// flag, or when the plan is declined, this is exactly the in-place sweep
    /// it was.
    pub fn sweep_old_gen_non_moving(
        &self,
        roots: &[ObjectRef],
        promotions: &cratonvm_types::PointerMap,
    ) -> (usize, cratonvm_types::PointerMap) {
        self.sweep_old_gen_non_moving_impl(roots, promotions, Self::pinned_compact_plan, None)
    }

    /// [`Self::sweep_old_gen_non_moving`] with the old generation's finalizer
    /// resurrection (gc-common w36-d, see [`OldGenFinalizerPass`]):
    /// `run_non_moving_young_cycle` passes the collection's `finalizer_addrs`
    /// so a finalizable that died OLD is kept for `finalize()` and reported,
    /// instead of freed in place.
    pub fn sweep_old_gen_non_moving_finalizing(
        &self,
        roots: &[ObjectRef],
        promotions: &cratonvm_types::PointerMap,
        fin_pass: &mut OldGenFinalizerPass<'_>,
    ) -> (usize, cratonvm_types::PointerMap) {
        self.sweep_old_gen_non_moving_impl(
            roots,
            promotions,
            Self::pinned_compact_plan,
            Some(fin_pass),
        )
    }

    /// The production pinned-compaction plan of the non-moving old sweep
    /// (`CRATONVM_GC_OLD_PINNED_COMPACT`, opt-in): `None` when the flag is
    /// off.
    pub fn pinned_compact_plan(caller_roots: usize) -> Option<Result<OldPinnedCompact, &'static str>> {
        gc_flags()
            .old_pinned_compact
            .then(|| OldPinnedCompact::for_this_pause(caller_roots))
    }

    /// [`Self::sweep_old_gen_non_moving`] with the pinned-compaction plan
    /// supplied by `plan_for` (called only when a fragmentation compaction is
    /// pending; `None` means "not enabled", an `Err` is a refusal that is
    /// counted). Tests pass an explicit plan here, since the flag is read once
    /// per process.
    ///
    /// gcd d1/d: `Option<Result<..>>` (it was `Result`, with `Err("")` as the
    /// "not enabled" sentinel — `gengc-r4w6-review6-old-pinned-compaction-residuals`,
    /// "Also noted" 2): an empty refusal reason can no longer be mistaken for
    /// "off", nor "off" for a refusal.
    pub fn sweep_old_gen_non_moving_with(
        &self,
        roots: &[ObjectRef],
        promotions: &cratonvm_types::PointerMap,
        plan_for: impl FnOnce(usize) -> Option<Result<OldPinnedCompact, &'static str>>,
    ) -> (usize, cratonvm_types::PointerMap) {
        self.sweep_old_gen_non_moving_impl(roots, promotions, plan_for, None)
    }

    /// The body of every `sweep_old_gen_non_moving*` form: the pinned plan
    /// from `plan_for`, and (gc-common w36-d) the optional finalizer pass.
    pub fn sweep_old_gen_non_moving_impl(
        &self,
        roots: &[ObjectRef],
        promotions: &cratonvm_types::PointerMap,
        plan_for: impl FnOnce(usize) -> Option<Result<OldPinnedCompact, &'static str>>,
        fin_pass: Option<&mut OldGenFinalizerPass<'_>>,
    ) -> (usize, cratonvm_types::PointerMap) {
        let young_from = self.lock_young_from();
        let mut old_gen = self.old_gen.lock();
        let before = old_gen.used();
        // gcd d4/n: a REQUESTED major's true-root young seed (armed by
        // `run_non_moving_young_cycle`; see `TrueRootYoung`). gcd d9/a: taken
        // up here, because on a promoting cycle the seed now resolves the
        // promotions below instead of falling back
        // (`CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE`, default on).
        let armed = TRUE_ROOT_MAJOR_ARMED.with(|c| c.replace(0));
        let true_root_armed = (armed & ARMED_TRUE_ROOTS) != 0;
        // gcf f2/c: a requested major (with or without the true-root seed),
        // for `CRATONVM_GC_REQUESTED_MAJOR_COMPACTS` below.
        let requested_major = (armed & ARMED_REQUESTED) != 0;
        let true_root_wide = true_root_armed && full_gc_true_roots_wide_enabled();
        // `(source, destination)` of every usable promotion, when the wide
        // seed may resolve them.
        let mut promoted_pairs: Vec<(usize, usize)> = Vec::new();
        TRUE_ROOT_SWEEP_OUT.with(|c| *c.borrow_mut() = TrueRootSweepOut::default());
        // H2-CID0: the reserved TLAB tails of forcibly-stopped in-JIT peers.
        // `mark_young_to_old_refs` must skip them exactly as the young sweep
        // does — see its comment for what walking one costs.
        let young_skips = {
            let base = young_from.base_ptr() as usize;
            self.jit_tlab_skip_offsets(base, base + young_from.used())
        };
        // gen r4/oldgen: sized for the promotion seeds appended below, so a
        // large promotion map does not regrow a copy of the whole root set.
        let mut root_shadow = Vec::with_capacity(roots.len().saturating_add(promotions.len()));
        root_shadow.extend_from_slice(roots);
        // Seed this sweep from THIS cycle's promotion destinations.
        //
        // `sweep_young_non_moving` commits selective promotions (young→old)
        // into `promotions` but leaves the caller's roots on their PRE-
        // promotion young addresses. `old_gen_gc`'s seed loop screens out any
        // address that is not a plausible old-gen object base, so a stale young
        // root seeds NOTHING — the object at its new old-gen home is never
        // marked and this sweep hands a LIVE object's block back to the free
        // list.
        //
        // `mark_young_to_old_refs` normally covers a promoted object via its
        // referrer, but only while that referrer is still in young from-space.
        // A promoted object whose every referrer was ALSO promoted this cycle
        // has nothing young pointing at it, so the only seed left is the stale
        // root — and it is freed. That shape is what
        // `old_sweep_seeds_from_this_cycles_promotion_destinations` stages, and
        // it is a genuine differential: disable the loop below and the test
        // fails with "returned a LIVE promoted object's block to the free list".
        //
        // It also explains why a freed-while-referenced assertion over heap→heap
        // edges stays silent on this defect: when a whole subgraph is condemned
        // together, no SURVIVING heap object points into the doomed set, so
        // there is no edge for such a detector to report. Silence there is not
        // evidence the mark was complete.
        //
        // Seed EVERY destination, not just the ones a root happens to name.
        // A promotion destination is live BY CONSTRUCTION — `sweep_young_non_
        // moving` promotes exactly the objects its young mark just proved live
        // — so marking all of them is correct rather than merely conservative.
        // Root-matching alone would miss the commoner case: a promoted object
        // referenced from a HEAP slot whose rewrite happens in the VM's post-GC
        // fixup, i.e. after this sweep has already run and freed it.
        //
        // ADDITIVE, and on the shadow only. Two things went wrong the first
        // time this was attempted (defect 4 in
        // `map-resize-unpinned-chain-cursors-nojit-segv-20260731-FIXED.md`):
        // rewriting the CALLER's slice corrupted a root snapshot that outlives
        // this call, and seeding destinations was itself destabilising because
        // that seed loop did not validate what it marked. The second cause is
        // gone: since `c3dbb011a` every push site into the mark worklist goes
        // through `old_gen_mark_candidate_plausible`, so a destination that is
        // not a real old-gen object base is REJECTED rather than blindly
        // `gc_flags`-RMW'd — which is precisely the "at least one address in
        // `promotions` is not a valid old-gen object base" the earlier attempt
        // correctly inferred from its crashes but could not screen for.
        // Appending (rather than replacing) keeps the pre-promotion entries
        // too, so this can only ever mark MORE — the safe direction for a sweep
        // that frees purely on `GC_FLAG_MARKED`.
        //
        // `CRATONVM_DBG_PROMO_SEED=1` reports how many destinations were seeded
        // and how many the screen rejected — the fact the earlier attempt
        // inferred but never measured.
        //
        // gengc-round1-core (2026-09-20) — THE SCREEN HERE WAS THE WRONG
        // SCREEN, and its false rejects were premature frees.
        //
        // `old_gen_mark_candidate_plausible` is a BYTE heuristic: besides
        // alignment and containment it inspects `_padding`/`_gc_reserved`,
        // undefined `gc_flags` bits and the claimed extent. GCAUD-8 established
        // (see `mark_and_push_old_gen`) that those clauses can reject a REAL
        // object, which is why every other precise push site ORs in
        // `rescue_mark_candidate_by_walk(..., &walked_objects)` — the object grid
        // `old_gen.walk_objects()` derives, which outranks the byte heuristic
        // on exactly the addresses where the two disagree.
        //
        // This was the one precise site with no rescue, because the grid
        // is derived later, INSIDE `old_gen_gc`. A rejected destination never
        // entered `root_shadow`, so `old_gen_gc`'s own root loop — which does
        // have the rescue — could never see it, nothing else marked it, and the
        // in-place sweep freed a live block. `close_live_set` cannot repair
        // that either: the shape this seed exists for is "a promoted object
        // whose every referrer was ALSO promoted this cycle", i.e. no marked
        // old-gen referrer exists to close over.
        //
        // So seed UNCONDITIONALLY and let `old_gen_gc`'s root loop do the
        // screening. That is strictly safer, not laxer: the root loop resolves
        // against the walked object grid FIRST and only then falls back to the
        // byte heuristic (plus the rescue), so nothing unvalidated reaches a
        // mark write. The only precondition this loop still has to uphold is
        // `ObjectRef::from_raw`'s own — non-null and 8-aligned — which a
        // corrupt map entry (BUG-Z) could violate, so that much is still
        // checked here. `promo_rejected` now counts what the OLD screen would
        // have dropped, which is the number that says whether this defect was
        // being hit.
        let mut promo_seeds = 0usize;
        let mut promo_rejected = 0usize;
        let mut promo_unusable = 0usize;
        // gen r4/oldgen (2026-09-23): `promo_rejected` is printed only under
        // `CRATONVM_DBG_PROMO_SEED`, but the byte heuristic behind it ran for
        // EVERY promotion destination on every in-place old sweep — a header
        // decode, a reserved-field screen and a `gen_object_total_size` (a
        // layout-cache lookup for a compact object) per promoted object,
        // inside the pause, to feed a counter nobody reads. Decided once.
        let count_promo_rejects = promo_seed_dbg();
        for (&src, &dest) in promotions.iter() {
            let ptr = dest as *mut u8;
            if dest == 0 || dest % 8 != 0 || !old_gen.contains(ptr) {
                // Not an old-gen destination at all: either a young→young
                // Cheney forward that never belonged here, or a corrupt entry.
                // Either way it cannot be `from_raw`'d.
                promo_unusable += 1;
                continue;
            }
            if count_promo_rejects && !old_gen_mark_candidate_plausible(ptr, &old_gen, false) {
                promo_rejected += 1;
            }
            promo_seeds += 1;
            // gcd d9/a: kept for the true-root seed's resolution (and for the
            // dead-promotion verdict after the sweep).
            if true_root_wide {
                promoted_pairs.push((src, dest));
            }
            // SAFETY: `dest` was checked non-null and 8-byte aligned above,
            // which is the whole of `ObjectRef::from_raw`'s contract. It is
            // never dereferenced here; `old_gen_gc`'s root loop screens it
            // against the walked object grid before any header access.
            root_shadow.push(unsafe { ObjectRef::from_raw(ptr) });
        }
        // gcd d9/a — the rows keyed by a promoted OWNER's source address.
        //
        // `mirror_pin` and `metadata_pin` are rebuilt by the VM's root
        // gatherer from PRE-collection addresses and remapped only after the
        // collector returns, and the value side of both is deferred to its
        // owner whenever it is OLD (`VmHeap::mirror_pin_deferrable`,
        // `metadata_pin_deferrable`): nothing but the owner's row keeps it. The
        // BFS looks a row up by the address it marked -- here the promotion's
        // DESTINATION -- and so found nothing for an owner (a user
        // `ClassLoader`) promoted in this very pause: its old mirrors and
        // metadata values were swept under it. Their values join the seeds as
        // roots (screened by the root loop like every other). Under the
        // true-root seed they sit in the skipped tail, and `TrueRootYoung::round`
        // follows the same rows for each destination the mark reaches.
        if promo_seeds > 0 {
            let mirror_rows = cratonvm_types::mirror_pin::snapshot();
            let metadata_rows = cratonvm_types::metadata_pin::snapshot();
            if mirror_rows.is_some() || metadata_rows.is_some() {
                let mut keyed_values = 0usize;
                for (&src, &dest) in promotions.iter() {
                    if dest == 0 || dest % 8 != 0 || !old_gen.contains(dest as *const u8) {
                        continue;
                    }
                    let rows = mirror_rows
                        .as_ref()
                        .and_then(|m| m.get(&src))
                        .into_iter()
                        .chain(metadata_rows.as_ref().and_then(|m| m.get(&src)));
                    for values in rows {
                        for &v in values {
                            if v != 0 && v % 8 == 0 {
                                keyed_values += 1;
                                // SAFETY: non-null and 8-aligned, checked just
                                // above (`ObjectRef::from_raw`'s contract); the
                                // root loop screens it before any access.
                                root_shadow.push(unsafe { ObjectRef::from_raw(v as *mut u8) });
                            }
                        }
                    }
                }
                if count_promo_rejects && keyed_values > 0 {
                    eprintln!(
                        "[promo-seed] old sweep seeded {keyed_values} pin value(s) keyed by \
                         promoted owners' source addresses"
                    );
                }
            }
        }
        if count_promo_rejects {
            eprintln!(
                "[promo-seed] old sweep seeded {promo_seeds} promotion destination(s) \
                 ({promo_rejected} of them the byte heuristic would have dropped -- \
                 see the gengc-round1-core note above), skipped {promo_unusable} \
                 non-old-gen/unaligned, from {} promotion entr(ies) \
                 against {} root(s)",
                promotions.len(),
                roots.len(),
            );
        }
        // gen r4w4/oldgen4: this collector never compacts (its roots are
        // conservative), so a fragmentation compaction request it was run for
        // is discarded rather than left to make some much later moving-path
        // major compact; the coalesce at the end of its in-place arm is the
        // repair this path can make. A NEW refusal re-arms the trigger.
        //
        // gen r4w5/oldcompact5: ...unless the pinned compaction is enabled
        // and its gates pass, in which case the request is answered HERE, by
        // compacting around the objects conservative words name. Consumed
        // either way, so it still cannot linger into a later cycle.
        //
        // gen r5w1/oldgen5: and by DEFAULT when the request is a refused
        // humongous allocation's (`OldGen::note_humongous_refusal`) and the
        // pause has NO conservative root to pin — no word published by the
        // JIT, the take-over or the interpreter probe, and no root range to pin
        // wholesale. Every root is then precise and remapped through the map
        // this returns, exactly as on the moving path's `major_gc`, which
        // already compacts on this request by default; the ladder's own major
        // request (`explicit_full_gc`) is what sends an interpreted program
        // here instead (`GenR4W4HumongousFragProbe`: `OutOfMemoryError` where
        // HotSpot Serial succeeds). With any conservative source live the
        // opt-in `CRATONVM_GC_OLD_PINNED_COMPACT` still decides, unchanged.
        let humongous_refusal = old_gen.humongous_refusal_pending();
        // gen r5w2/alloc6: the refused size, read before the take clears it,
        // for the compaction's log line (did it make room?).
        let humongous_request = old_gen.humongous_refused_request();
        let frag_request_bytes = old_gen.fragmentation_compaction_request_bytes();
        let frag_pending = old_gen.take_fragmentation_compaction_request();
        let frag_flags = gc_flags();
        let frag_due = old_gen.fragmentation_repair_due();
        // A blocked small allocation may be repairable even when the arena's
        // historical largest allocation has no proactive repair margin.
        // Revalidate this concrete request now; a stale pending bit alone
        // must never admit a compaction on a full heap.
        let (og_cap, og_free, _, _) = old_gen.fragmentation_metrics();
        let reactive_due = frag_flags.old_frag_pinned_compact
            && frag_pending
            && frag_request_bytes > 0
            && (og_free >= og_cap / 20 || frag_request_bytes >= og_cap / 10)
            && old_gen.fragmented_for(frag_request_bytes);
        let fragmentation_slide_goal = reactive_due.then_some(frag_request_bytes);
        if frag_flags.dbg_gcphase && frag_flags.old_frag_pinned_compact {
            let (capacity, free, largest, recent) = old_gen.fragmentation_metrics();
            eprintln!(
                "[GC] oldgen_frag_pinned_candidate: collections={} pending={} pending_bytes={} reactive_due={} due={} capacity={} free={} largest={} recent_request={}",
                old_gen.trigger_stats().collections,
                frag_pending,
                frag_request_bytes,
                reactive_due,
                frag_due,
                capacity,
                free,
                largest,
                recent,
            );
        }
        // The pinned-compaction parity lane is deliberately sparse: it only
        // considers a generation already meeting the established fragmented
        // request predicate, and at most once in every four old collections.
        // gcj og3 P2: and it is ADDITIVE -- it plans only after every older
        // route below (the opt-in pinned plan, the requested major, the
        // humongous default) has declined, so what those compacted before it
        // was default ON they still compact the same way.
        let frag_pinned_requested = frag_flags.old_frag_pinned_compact
            && fragmented_pinned_compaction_due(
                frag_due,
                reactive_due,
                old_gen.trigger_stats().collections,
                old_gen.fragmentation_pinned_last_attempt(),
            );
        // Preserve the older full-compaction routes. The repair lane is
        // additive and only builds its bounded plan after they decline.
        let frag_requested = frag_pending;
        let mut declined_plan: Option<Result<OldPinnedCompact, &'static str>> = None;
        let mut opt_in_refused = false;
        // gcf f2/c — `CRATONVM_GC_REQUESTED_MAJOR_COMPACTS` (default ON since WARNING gci-user-flip-20261013, `=0` kills): a
        // REQUESTED major compacts too, as HotSpot Serial's full collection
        // always does, under the humongous default's gate (no conservative
        // root to pin; with `CRATONVM_GC_OLD_PINNED_COMPACT` the pinned plan,
        // as for a fragmentation request). Not when the true-root seed is armed
        // but narrow (`CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE=0`): a planned
        // compaction would push that major back onto the legacy seed, undoing
        // gcd d4/n for every `System.gc()`. A fragmentation request already
        // plans the same compaction, so it is not counted twice.
        let requested_compacts = requested_major_compaction_wanted(
            frag_requested,
            requested_major,
            true_root_armed,
            true_root_wide,
            requested_major_compacts_enabled,
        );
        let mut plan = if frag_requested || requested_compacts {
            match plan_for(roots.len()) {
                Some(Ok(plan)) => {
                    if requested_compacts {
                        old_gen.note_requested_major_compaction(true);
                    }
                    Some(plan)
                }
                None if requested_compacts => match OldPinnedCompact::for_this_pause(roots.len()) {
                    Ok(plan) if plan.pins_nothing_conservative() => {
                        old_gen.note_requested_major_compaction(true);
                        Some(plan)
                    }
                    // A conservative root is live (or the take-over licence
                    // forbids a move): reclaim in place, as without the switch.
                    // Counted, not logged: every JIT-warm `System.gc()` takes
                    // this arm.
                    declined => {
                        old_gen.note_requested_major_compaction(false);
                        declined_plan = Some(declined);
                        None
                    }
                },
                None if humongous_refusal && gc_flags().old_oom_compact => {
                    match OldPinnedCompact::for_this_pause(roots.len()) {
                        Ok(plan) if plan.pins_nothing_conservative() => {
                            old_gen.note_humongous_default_compaction();
                            Some(plan)
                        }
                        // gen r5w2/alloc6: say WHY the default declined — on
                        // a JIT-warm pause it always does (the compiled
                        // frames' band words are published), which is the
                        // flag-off half of `GenR4W5OldPinnedCompactProbe`.
                        // Rate-limited like the refusal lines below.
                        // Not counted in `pinned_compaction_refusals`: no plan
                        // was built, and the default run's counters stay as
                        // they were.
                        declined => {
                            if HUMONGOUS_DEFAULT_DECLINE_REPORTS.fetch_add(1, Ordering::Relaxed) < 8
                            {
                                let (words, range) = match &declined {
                                    Ok(p) => (
                                        p.words.len(),
                                        p.pin_root_range.1.saturating_sub(p.pin_root_range.0),
                                    ),
                                    Err(_) => (0, 0),
                                };
                                tracing::info!(
                                    target: "cratonvm::gc",
                                    why = declined.as_ref().err().copied().unwrap_or(
                                        "conservative roots are live (CRATONVM_GC_OLD_PINNED_COMPACT is off)"
                                    ),
                                    published_words = words,
                                    pinned_root_range = range,
                                    humongous_request,
                                    "old-gen humongous compaction not taken by default; reclaiming in place",
                                );
                            }
                            declined_plan = Some(declined);
                            None
                        }
                    }
                }
                None => None,
                Some(Err(why)) => {
                    opt_in_refused = true;
                    let n = old_gen.note_pinned_compaction_refusal();
                    if n < 8 {
                        tracing::info!(
                            target: "cratonvm::gc",
                            why,
                            "old-gen pinned compaction declined for this pause; reclaiming in place",
                        );
                    }
                    None
                }
            }
        } else {
            None
        };
        // gcj og3 P2: the fragmentation lane, when nothing above compacts. It
        // pins what conservative words name and slides only the window chosen
        // for the request, or nothing when the in-place sweep already does.
        if plan.is_none() && frag_pinned_requested && !opt_in_refused {
            old_gen.note_fragmentation_pinned_attempt();
            match declined_plan.unwrap_or_else(|| OldPinnedCompact::for_this_pause(roots.len())) {
                Ok(mut lane) => {
                    lane.fragmentation_triggered = true;
                    lane.reactive_fragmentation_repair = reactive_due;
                    lane.fragmentation_slide_goal = fragmentation_slide_goal.or_else(|| {
                        let (_, _, _, request) = old_gen.fragmentation_metrics();
                        (request > 0).then_some(request)
                    });
                    plan = Some(lane);
                }
                Err(why) => {
                    let n = old_gen.note_pinned_compaction_refusal();
                    if n < 8 {
                        tracing::info!(
                            target: "cratonvm::gc",
                            why,
                            "old-gen fragmentation compaction declined for this pause; reclaiming in place",
                        );
                    }
                }
            }
        }
        // gcd d3/n (cards3 item 3, producer (d)): on an element-precise table
        // the compaction records a surviving reference array's young
        // ELEMENTS' cards, not its header's, as the moving path's promoted
        // scans do since gcd d2/h (`HolderCardDefer`).
        // `CRATONVM_GC_PRECISE_ARRAY_CARDS=0` leaves this `None` (header marks).
        if let Some(plan) = plan.as_mut() {
            plan.element_card_base = self
                .card_table
                .precise_ref_array_marks()
                .then(|| self.card_table.base_addr());
            // gcf f4/c: `CRATONVM_GC_OLD_PIN_UNREWRITABLE_ONLY`'s engagement.
            if plan.movable_words_unpinned > 0 {
                old_gen.note_movable_words_unpinned(plan.movable_words_unpinned);
            }
        }
        // gen r4w5/oomjit5: the census labels `root_shadow[roots.len()..]`
        // (the promotion destinations appended above) by their seeding site.
        let _census_tail = oldmark_census::enabled()
            .then(|| oldmark_census::TailScope::new(roots.len(), "promotion-dest"));
        // gcd d4/n: a REQUESTED major's true-root young seed (armed by
        // `run_non_moving_young_cycle`; see `TrueRootYoung`). A cycle that
        // promoted anything used to keep the legacy seed: the side tables and
        // the finalizer list still name the pre-promotion young addresses
        // until the VM's remap. gcd d9/a: the seed now resolves those through
        // `promoted_pairs` (`CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE`, default on);
        // with `=0` the fallback is as before.
        if true_root_armed && promo_seeds > 0 && !true_root_wide {
            note_true_root_fallback(
                &mut old_gen,
                crate::old_gen::TrueRootFallback::Promoted,
                promo_seeds,
            );
        }
        // gci i3/og (`CRATONVM_GC_TRUE_ROOT_BATCHED_VERDICT`, opt-in), read once
        // per armed sweep.
        let batched = true_root_armed
            && (promo_seeds == 0 || true_root_wide);
        let true_root_ask = (true_root_armed && (promo_seeds == 0 || true_root_wide)).then(|| {
            let mut promotions = std::mem::take(&mut promoted_pairs);
            promotions.sort_unstable();
            TrueRootAsk {
                promo_tail: roots.len(),
                promotions,
                wide: true_root_wide,
                pre_copy: false,
                watched_post_copy: Vec::new(),
                watched_snapshot: None,
                hand_back_judged: batched,
                // gci i5/og: the non-moving major's owners did not move.
                metadata_rows: None,
                // gci i8/pa: nor its loaders.
                mirror_rows: None,
                loader_rows: None,
                fallback_loaders: Vec::new(),
            }
        });
        // The destinations the dead-promotion verdict below judges (sorted by
        // source, as the ask's list).
        // gci i3/og (`CRATONVM_GC_TRUE_ROOT_BATCHED_VERDICT`, opt-in): instead
        // `(destination, source)`, sorted by destination, so the verdict after
        // the sweep is one merge walk over the free blocks
        // (`dead_promotion_sources_batched`). Timed under `CRATONVM_DBG_GCPHASE`
        // (`[gcphase] old-gen tr-judge:`, outside `old_gen_gc_inner`'s lines).
        // gci i4/og (`gci-i3og-true-root-major-residual-costs-FIXED-20261012` item
        // 2): under BV the list is no longer built here, before a fallback
        // could drop it unread, and the ask's list is not copied: the call
        // hands it back once the seed ran (`TrueRootMarkOut::judged_by_dest`,
        // timed there). The default arm still clones the ask's list here.
        let t_list = gc_flags().dbg_gcphase.then(std::time::Instant::now);
        let mut judged: Vec<(usize, usize)> = match true_root_ask.as_ref() {
            Some(ask) if !batched => ask.promotions.clone(),
            _ => Vec::new(),
        };
        let mut list_us = t_list.map_or(0, |t| t.elapsed().as_micros());
        // `(destination, source)` of a `judged` entry, whatever its order.
        let dest_src = |(a, b): (usize, usize)| if batched { (a, b) } else { (b, a) };
        TRUE_ROOT_MARK_THIS_CALL.with(|c| *c.borrow_mut() = true_root_ask);
        TRUE_ROOT_MARK_OUT.with(|c| *c.borrow_mut() = TrueRootMarkOut::default());
        let survivors = Self::old_gen_gc_inner(
            &mut root_shadow,
            &young_from,
            &mut old_gen,
            false,
            &young_skips,
            plan.as_mut(),
            fin_pass,
        );
        TRUE_ROOT_MARK_THIS_CALL.with(|c| *c.borrow_mut() = None);
        let mut mark_out = TRUE_ROOT_MARK_OUT.with(|c| std::mem::take(&mut *c.borrow_mut()));
        // gci i4/og: BV's judged list, handed back by the call (empty unless
        // the seed ran).
        if batched {
            judged = std::mem::take(&mut mark_out.judged_by_dest);
            list_us = u128::from(mark_out.judged_list_us);
        }
        // gcd d9/a — which promotions of this pause DIED. The true-root seed
        // no longer makes every destination a root, so a destination nothing
        // reached was freed; its source must stop being reported as a
        // survivor (`source -> freed block` in the returned map would re-key
        // a monitor, a mirror, an identity row onto memory the next
        // allocation reuses). After a pinned compaction the compaction says
        // which it dropped (a slid object may now sit at a dropped address);
        // after the in-place sweep the free list is the exact answer, whatever
        // kept a block (`close_live_set`, a walk that freed nothing).
        if mark_out.ran && !judged.is_empty() {
            let t_judge = gc_flags().dbg_gcphase.then(std::time::Instant::now);
            let dead_sources: Vec<usize> = if plan.as_ref().is_some_and(|p| p.compacted.is_some()) {
                let mut v: Vec<usize> = judged
                    .iter()
                    .map(|&pair| dest_src(pair))
                    .filter(|&(dest, _)| mark_out.dropped_dests.binary_search(&dest).is_ok())
                    .map(|(_, src)| src)
                    .collect();
                if batched {
                    // Source order, as the per-entry arm reports them.
                    v.sort_unstable();
                }
                v
            } else if batched {
                dead_promotion_sources_batched(&judged, |by_dest| {
                    old_gen.unallocated_indices_ascending_by(by_dest, |&(dest, _)| dest)
                })
            } else {
                judged
                    .iter()
                    .filter(|&&(_, dest)| !old_gen.is_allocated_addr(dest as *const u8))
                    .map(|&(src, _)| src)
                    .collect()
            };
            if let Some(t) = t_judge {
                eprintln!(
                    "[gcphase] old-gen tr-judge: part={}us list={list_us}us judged={} dead={} batched={}",
                    t.elapsed().as_micros(),
                    judged.len(),
                    dead_sources.len(),
                    u8::from(batched),
                );
            }
            old_gen.note_true_root_promotions(judged.len(), dead_sources.len());
            TRUE_ROOT_SWEEP_OUT.with(|c| c.borrow_mut().dead_sources = dead_sources);
        }
        // gce e2/c: gcd d9/a's step 1 (`CRATONVM_GC_FULL_GC_TRUE_ROOTS_FREE_YOUNG`,
        // freeing the young survivors the true-root seed left out in this
        // pause) was REMOVED: it did not make the one-major ladder pass
        // (`fy_nsoom` 0/5, as the off arm) and broke `NativeGrowthReclaimProbe`
        // with OSR off. The excluded survivors are freed by the next young
        // collection, as before d9/a.
        if let Some(plan) = plan.as_ref() {
            // Old->young edges live in the survivors that moved: their cards
            // must be dirty at the NEW addresses before mutators resume, or
            // the next young collection misses the edge (the moving path's
            // `deferred_dirty_cards` remap, for the same reason). The cards
            // left dirty at the old addresses only cost that scan a walk.
            if !plan.young_ref_holders.is_empty() {
                self.card_table.mark_dirty_bulk(&plan.young_ref_holders);
            }
            if let Some((pinned, moved, moved_bytes, largest_before, largest_after)) =
                plan.compacted
            {
                let n = old_gen.sizing_stats().pinned_compactions;
                // gen r5w2/alloc6: the pin attribution (`OldPinReport`), and —
                // when a refused humongous request armed this — whether the
                // largest free block now holds it. `made_room=false` with a
                // non-zero `humongous_request` is the JIT-warm failure's
                // signature: read `largest_pins` for what stood in the way
                // (offset, size, source bits: 1 interior, 2 range, 4 range
                // end, 8 word, 16 word end, 32 young stretch). Printed past
                // the rate limit whenever it did not make room.
                let r = &plan.report;
                let made_room = humongous_request == 0 || largest_after >= humongous_request;
                if n <= 8 || n.is_power_of_two() || !made_room {
                    tracing::info!(
                        target: "cratonvm::gc",
                        pinned,
                        moved,
                        moved_bytes,
                        largest_before,
                        largest_after,
                        humongous_request,
                        made_room,
                        pinned_bytes = r.pinned_bytes,
                        pins_by_source = ?r.by_source,
                        resurrected = r.resurrected,
                        dropped_unmarked = r.dropped_unmarked,
                        dropped_unmarked_bytes = r.dropped_unmarked_bytes,
                        largest_pins = ?r.largest,
                        largest_pins_also_rooted = ?r.largest_also_rooted,
                        movable_words_unpinned = plan.movable_words_unpinned,
                        "old-gen compaction around pinned objects (non-moving path)",
                    );
                }
            } else if let Some(why) = plan.refused {
                let n = old_gen.note_pinned_compaction_refusal();
                if n < 8 {
                    tracing::info!(
                        target: "cratonvm::gc",
                        why,
                        humongous_request,
                        pinned = plan.report.pinned,
                        largest_pins = ?plan.report.largest,
                        largest_pins_also_rooted = ?plan.report.largest_also_rooted,
                        "old-gen pinned compaction refused; reclaimed in place instead",
                    );
                }
            }
        }
        // gen r4w2/oldgen2: arms the trigger's hysteresis term and feeds the
        // low-yield instrument (see `major_trigger_decision`).
        old_gen.note_collection_end(before);
        Self::after_old_gen_collection(&mut old_gen);
        // gen r5w3/oldgen7: the resize's interior half (opt-in).
        self.decommit_old_interior_if_enabled(&mut old_gen);
        (before.saturating_sub(old_gen.used()), survivors)
    }

    /// gen r4w2/oldgen2 (2026-09-23) — THE old-gen trigger: should this young
    /// cycle also run an old-generation collection?
    ///
    /// Both trigger sites (`collect_garbage_inner`'s Phase 5 and
    /// `run_non_moving_young_cycle`) call this instead of repeating
    /// `used >= capacity * 75 / 100 || major_requested`, so the policy lives
    /// in one place (`OldGen::major_trigger_verdict`, a pure function with the
    /// tests) and every decision is counted (`OldGen::trigger_stats`).
    ///
    /// **Default: unchanged.** Without `CRATONVM_GC_OLD_TRIGGER_HYSTERESIS`
    /// this returns `true` in exactly the cases the old expression did
    /// (`old_gen::tests::the_trigger_with_hysteresis_off_is_the_bare_occupancy_test`).
    /// What it adds on a default run is the instrument
    /// `gengc-r4-oldgen-major-trigger-has-no-hysteresis-FIXED-20260924.md` asked for
    /// first: `would_suppress` (collections the hysteresis would have skipped)
    /// and `low_yield` (collections that reclaimed under `capacity / 16`).
    ///
    /// With the flag, a collection above the 75 % floor is skipped until
    /// half of what the previous one left free (at least `capacity / 16`) has
    /// been promoted since — unless an old-gen allocation has FAILED since,
    /// which always collects, so a genuinely full generation still reaches
    /// the `OutOfMemoryError` ladder on the cycle its promotion fails.
    ///
    /// gen r4w4/concmark4 (2026-09-24) — THE OLD-GEN POLICY SPLIT
    /// (`docs/feature-designs/generative-gc-architecture.md`).
    /// A method now (both callers are `&self`), because the decision needs the
    /// VM's concurrent-cycle state:
    ///
    /// * `Legacy` (`CRATONVM_GC_NO_CONCURRENT_FIRST`): the body above,
    ///   unchanged — `OldGen::major_trigger` with the explicit hysteresis flag.
    /// * `ConcurrentFirst` (default): the same verdict with the policy's
    ///   effective hysteresis (`OldGenPolicy::hysteresis`: an explicit flag
    ///   wins, else the policy default — still OFF, the pending flip), and —
    ///   while a concurrent cycle is open and nothing REQUESTED this
    ///   collection — an occupancy verdict below the 90 % ceiling with no
    ///   old-gen allocation failure since the last collection DEFERS to the
    ///   cycle: not counted as a decision (the peek records nothing; the census
    ///   counts `stw_deferred`), no collection. A failure or the ceiling runs
    ///   the collection anyway and tells the start policy the
    ///   cycle began too late (`note_stw_preempted`); that collection's frees
    ///   move `reclaim_epoch`, so the open cycle is abandoned at its next slice
    ///   (`cycle_is_stale`) or its sweep stops between slices, as before.
    ///
    /// gen r4w4/oldgen4 (2026-09-24): a pending FRAGMENTATION refusal (an
    /// old-gen allocation refused although the free list held its size in
    /// total — `OldGen::note_refused_request`) also runs a collection — below
    /// the occupancy floor too, which is the point: "there are bytes but no
    /// room" is a state the 75 % test cannot see — and marks it to compact
    /// (`OldGen::arm_fragmentation_compaction`, consumed by `major_gc`; the
    /// non-moving path, which cannot compact, discards it). One collection per
    /// refusal episode. `CRATONVM_GC_OLD_OOM_COMPACT` (default on) governs the
    /// refusal-armed request, `CRATONVM_GC_OLD_FRAG_COMPACT` (opt-in) adds the
    /// proactive post-collection arm; with both off this function is exactly
    /// what it was. The occupancy verdict is still asked, and counted, first
    /// and unchanged — and the hysteresis page's `alloc_failures` term already
    /// treated a refusal as a reason to collect.
    ///
    /// gen r4w6/concsvc6 (2026-09-24) — two changes, both confined to a cycle
    /// that is OPEN when a fragmentation compaction is armed
    /// (`gengc-r4w5-review5-fragmentation-preemption-feeds-the-concurrent-start-policy`,
    /// its "row 1"): the collection still runs, but it is no longer reported
    /// to the start policy as a pre-emption (`note_stw_preempted` lowers the
    /// next start; a fragmentation compaction says nothing about when the
    /// cycle began — it is `note_stw_fragmentation_preempt` now), and it no
    /// longer returns early, so the occupancy verdict is COUNTED like in every
    /// other row. Row 1 needs a request armed with no allocation failure since
    /// the last collection, which only the opt-in `CRATONVM_GC_OLD_FRAG_COMPACT`
    /// produces, so a default run is unchanged.
    ///
    /// It also returns WHY it runs (`Some(cause)` iff it runs), for the
    /// cadence census's split of the majors (`MajorCause`): the decision is
    /// unchanged by the classification, which only reads.
    pub fn major_trigger_decision_policy(
        &self,
        old_gen: &mut OldGen,
        major_requested: bool,
    ) -> (bool, Option<crate::concurrent_mark::MajorCause>) {
        use crate::concurrent_mark::MajorCause;
        let flags = gc_flags();
        // gen r4w4 merge: oldgen4's refusal-armed compaction is never deferred
        // to an open concurrent cycle (the cycle cannot compact).
        let frag_enabled = flags.old_oom_compact || flags.old_frag_compact;
        let policy = OldGenPolicy::current();
        let hysteresis = match policy {
            OldGenPolicy::Legacy => flags.old_trigger_hysteresis,
            OldGenPolicy::ConcurrentFirst => {
                policy.hysteresis(flags.old_trigger_hysteresis_explicit)
            }
        };
        // gcj j1yg-pause round 3: a pause that promised to stay minor
        // (`decide_next_pause_minor`) defers a major that only its own
        // promotions made due by occupancy; the next pause's decision finds
        // it due and runs it. Anything else (a request, an allocation
        // failure) runs as before.
        // A deferral makes the NEXT pause ineligible to promise, so a deferred
        // major runs no later than the pause after.
        let defer = self.minor_promised.swap(false, Ordering::AcqRel)
            && !major_requested
            && old_gen.alloc_failures_since_last_collection() == 0
            && old_gen.major_trigger_peek(false, hysteresis) == MajorTrigger::Occupancy;
        self.minor_promise_just_deferred.store(defer, Ordering::Release);
        if defer {
            self.minor_promise_deferrals.fetch_add(1, Ordering::Relaxed);
            old_gen.sample_young_cycle_promotion();
            return (false, None);
        }
        // Row 1 of the review5 page: armed while the verdict would defer.
        let mut frag_armed_over_deferral = false;
        // The collection runs through an open cycle it could not defer to.
        let mut through_open_cycle = false;
        // gcd d9/e: a REQUESTED collection through an open cycle is counted
        // (`[GC] conc_cycles: conccyc_requested_through_cycle`), nothing else.
        if policy == OldGenPolicy::ConcurrentFirst && major_requested {
            if let Some(state) = self.conc_cycle_state.as_deref() {
                if state.cycle_in_progress() {
                    state.note_requested_through_cycle();
                }
            }
        }
        if policy == OldGenPolicy::ConcurrentFirst && !major_requested {
            if let Some(state) = self.conc_cycle_state.as_deref() {
                if state.cycle_in_progress() {
                    let peek = old_gen.major_trigger_peek(false, hysteresis);
                    if peek.runs() {
                        let failures = old_gen.alloc_failures_since_last_collection();
                        // gcd d9/e: `CRATONVM_GEN_CONC_PRECEDENCE` defers at any
                        // occupancy until an old-gen allocation fails.
                        if ConcurrentStartPolicy::stw_defers_with(
                            old_gen.capacity(),
                            old_gen.used(),
                            peek,
                            failures,
                            crate::concurrent_mark::gen_conc_precedence_enabled(),
                        ) {
                            if frag_enabled && old_gen.arm_fragmentation_compaction() {
                                state.note_stw_fragmentation_preempt();
                                frag_armed_over_deferral = true;
                            } else {
                                state.note_stw_deferred();
                                // gcj j3/og3: this young cycle's promotion
                                // sample (`CRATONVM_GC_OLD_TRIGGER_SERIAL`; no
                                // counted decision is made on this return).
                                old_gen.sample_young_cycle_promotion();
                                return (false, None);
                            }
                        } else {
                            // gcd d9/e: counted by cause and phase.
                            state.note_stw_preempted_why(
                                ConcurrentStartPolicy::preempt_cause(failures),
                                old_gen_allocated_total(old_gen),
                                old_gen.capacity(),
                            );
                            through_open_cycle = true;
                        }
                    }
                }
            }
        }
        let verdict = old_gen.major_trigger(major_requested, hysteresis);
        let runs = verdict.runs();
        // `arm_fragmentation_compaction` consumes the pending request, so
        // row 1 (which armed it above) must not ask again.
        let fragmented =
            frag_armed_over_deferral || (frag_enabled && old_gen.arm_fragmentation_compaction());
        if !(runs || fragmented) {
            return (false, None);
        }
        let cause = if major_requested {
            MajorCause::Requested
        } else if through_open_cycle {
            MajorCause::FallbackCycleOpen
        } else if frag_armed_over_deferral || !runs {
            MajorCause::Fragmentation
        } else if policy == OldGenPolicy::ConcurrentFirst && self.concurrent_start_owed(old_gen) {
            MajorCause::FallbackStartLate
        } else if verdict == MajorTrigger::AllocationFailure {
            MajorCause::AllocationFailure
        } else {
            MajorCause::Occupancy
        };
        (true, Some(cause))
    }

    /// gen r4w6/concsvc6 — for the cadence census only: with NO cycle past its
    /// initial mark, was a concurrent cycle owed right now? `true` when a
    /// driver has claimed the cycle but not yet run its initial mark
    /// (`InitialMark`), or when the concurrent start is due
    /// (`ConcurrentGcState::concurrent_start_due`, which also records the
    /// instant for the trigger-to-start census). A STW collection that runs in
    /// that state ran where the concurrent cycle should already have been
    /// running. Old-gen lock held by the caller; takes the policy lock (leaf).
    pub fn concurrent_start_owed(&self, old_gen: &OldGen) -> bool {
        use crate::concurrent_mark::ConcurrentGcPhase;
        let Some(state) = self.conc_cycle_state.as_deref() else {
            return false;
        };
        match state.phase() {
            ConcurrentGcPhase::InitialMark => true,
            ConcurrentGcPhase::Idle => state.concurrent_start_due(
                old_gen.capacity(),
                old_gen.used(),
                old_gen.used_after_last_collection(),
            ),
            // A cycle past its initial mark was handled by the caller.
            _ => false,
        }
    }

    /// The young pause's STW old-gen decision: exactly
    /// [`Self::major_trigger_decision_policy`] (whose doc is the policy),
    /// plus — gen r4w5/concmark5 (2026-09-24) — one cadence sample per call
    /// for the count-based hysteresis measurement
    /// (`ConcurrentGcState::note_major_decision`, printed at shutdown as
    /// `[GC] major_cadence:`). The sample resolves the PREVIOUS decision
    /// against the generation's STW collection count and freed bytes as they
    /// stand now (that decision's major, if it ran, ended in between), so the
    /// sample changes no decision and needs nothing from either caller.
    /// Nothing is sampled before `enable_concurrent_gc`.
    ///
    /// gen r4w6/concsvc6: the sample carries the decision's cause
    /// (`MajorCause`), so the census splits the majors into "the concurrent
    /// cycle was too slow" (`majcad_fallback_*`) and the rest.
    pub fn major_trigger_decision(&self, old_gen: &mut OldGen, major_requested: bool) -> bool {
        let (runs, cause) = self.major_trigger_decision_policy(old_gen, major_requested);
        // gcg h1/ob: an unrequested major's young half ends here; its old
        // figure (after the young half's promotions) is the split's.
        if runs && !major_requested {
            self.note_escalation_decision(old_gen.used());
        }
        if let Some(state) = self.conc_cycle_state.as_deref() {
            let t = old_gen.trigger_stats();
            state.note_major_decision_caused(
                t.collections.saturating_sub(t.concurrent_collections),
                t.freed_bytes,
                old_gen.capacity(),
                cause,
            );
        }
        runs
    }

    /// gen r4w2/oldgen2 — the old-gen trigger's per-heap counters (see
    /// [`crate::old_gen::OldGenTriggerStats`]), plus the bytes the old
    /// generation's backing store has committed and given back. For the
    /// `[GC]` shutdown summary (`VmHeap::print_gc_summary`, lane `obs`).
    pub fn old_gen_trigger_stats(&self) -> (crate::old_gen::OldGenTriggerStats, usize, u64) {
        let og = self.old_gen.lock();
        (
            og.trigger_stats(),
            og.committed_bytes(),
            og.bytes_given_back(),
        )
    }

    /// gen r4w4/oldgen4 (2026-09-24) — the old generation's sizing,
    /// fragmentation-trigger, walk-gap and allocator-bound counters (see
    /// [`crate::old_gen::OldGenSizingStats`]), for the `[GC]` shutdown summary
    /// (lane `obs`).
    pub fn old_gen_sizing_stats(&self) -> crate::old_gen::OldGenSizingStats {
        self.old_gen.lock().sizing_stats()
    }

    /// gen r4w4/oldgen4 — the old generation's COMMITTED size, HotSpot's
    /// `committed` for the `Tenured Gen` pool: it grows as allocation commits
    /// pages and falls when a stop-the-world collection shrinks the free tail
    /// (`OldGen::resize_after_collection`). [`Self::old_gen_capacity`] stays
    /// the reservation — the pool's `max`, and the figure every occupancy
    /// trigger and the `-Xmx` budget are computed against.
    pub fn old_gen_committed_capacity(&self) -> usize {
        self.old_gen.lock().committed_capacity()
    }

    /// gen r4w4/oldgen4 (2026-09-24) — the allocation-failure ladder's
    /// question before it gives up on a request of `request_bytes` that the
    /// old generation must serve (a humongous array, or a promotion): does
    /// the old generation have the room in TOTAL but no single hole for it?
    /// If so, ask the next old-gen collection to COMPACT (once), and return
    /// `true` — the caller then requests the major collection it was about to
    /// request anyway (`gc_quiescence::request_major_gc`) and retries.
    ///
    /// For lane `oom`'s `gengc-r4w4-oom-humongous-on-fragmented-old-gen`
    /// page: a 33 MiB array after 17 MiB arrays A, B, C with A and C
    /// dropped finds ~47 MiB free in two holes and throws a false
    /// `OutOfMemoryError`; HotSpot Serial's full GC is a mark-compact and
    /// succeeds. The one-line call belongs in
    /// `vm/src/runtime/interpreter/gc_and_alloc.rs::last_ditch_reclaim`, right
    /// before its major request, AFTER the ladder's forced collection has
    /// freed the dead blocks in place (the question is about the free list
    /// that collection left):
    ///
    /// ```ignore
    /// if let VmHeap::Generational(h) = &*heap { let _ = h.request_old_gen_compaction_if_fragmented_for(request_bytes); }
    /// ```
    ///
    /// Exposure: compaction is off by default (`oldgen_compact_enabled`, an
    /// unattributed corruption in 2026-08); this runs it only on the path
    /// that otherwise ends in `OutOfMemoryError`, and only on the moving
    /// path's major (`major_gc`) — a cycle with conservative roots downgrades
    /// to the in-place sweep and the error stands, as today.
    /// `CRATONVM_GC_OLD_OOM_COMPACT=0` turns it off.
    ///
    /// gen r4w5/oldcompact5: ...unless `CRATONVM_GC_OLD_PINNED_COMPACT` is set,
    /// in which case the NON-MOVING path's collection (`sweep_old_gen_non_moving`)
    /// answers the request too, compacting around the objects conservative
    /// words name (`OldGen::compact_around_pins`). The ladder's own major
    /// request diverts its cycle to that path (`explicit_full_gc`), so this is
    /// the arm that reaches the false `OutOfMemoryError`.
    pub fn request_old_gen_compaction_if_fragmented_for(&self, request_bytes: usize) -> bool {
        if !gc_flags().old_oom_compact {
            return false;
        }
        let mut og = self.old_gen.lock();
        if !og.fragmented_for(request_bytes) {
            return false;
        }
        og.request_fragmentation_compaction_for(request_bytes);
        true
    }

    /// Run a major garbage collection on the old generation — mark-compact
    /// when `CRATONVM_OLDGEN_COMPACT` is on (the default since WARNING
    /// gci-user-flip-20261013; `=0` kills) and the caller does not veto it;
    /// otherwise an in-place mark-sweep (see below).
    ///
    /// 1. **Mark phase:** starting from `roots` + all young-gen objects, traverse
    ///    the heap marking live old-gen objects (set `GC_FLAG_MARKED`).
    /// 2. **Compact phase:** slide all live objects toward the start of the heap,
    ///    eliminating fragmentation. Update all internal references.
    /// 3. **Cross-gen fixup:** update young-gen references that pointed into
    ///    old gen to use the new compacted addresses.
    /// 4. **Root fixup:** update external root references pointing into old gen.
    ///
    /// gen r4/oldgen (2026-09-23): steps 2-4 describe the OPT-IN arm. With
    /// `oldgen_compact_enabled()` false — the default since 2026-08-03 —
    /// `old_gen_gc` takes its `compact = false` arm: dead blocks are freed in
    /// place, the free list is coalesced, nothing moves, and the returned map
    /// holds only identity entries for watched survivors. Every old-gen
    /// collection on a default run is therefore non-moving, on this path as
    /// much as on `sweep_old_gen_non_moving`'s; documents that call this
    /// "the old-gen mark-compact" describe the opt-in.
    ///
    /// Returns a pointer map (old_addr → new_addr) for relocated old-gen objects.
    /// The caller merges this into the overall `GcResult` for VM-level root updates.
    pub fn major_gc(
        roots: &mut [ObjectRef],
        young_from: &Arena,
        old_gen: &mut OldGen,
        young_skips: &[(usize, usize)],
    ) -> cratonvm_types::PointerMap {
        Self::major_gc_finalizing(roots, young_from, old_gen, young_skips, None, false)
    }

    /// [`Self::major_gc`] with the old generation's finalizer resurrection
    /// (gc-common w36-d, see [`OldGenFinalizerPass`]). The moving path's
    /// Phase 5 passes its `finalizer_addrs`; the resurrected addresses come
    /// back PRE-compaction, and the caller maps them through the returned map
    /// with the rest of its dead-finalizer list.
    ///
    /// gcd d4/n: `no_compact` vetoes the slide (compaction only: a pending
    /// request is still consumed and the collection still runs, in place);
    /// see the moving path's Phase 5 for when it is set. `major_gc` passes
    /// `false`.
    pub fn major_gc_finalizing(
        roots: &mut [ObjectRef],
        young_from: &Arena,
        old_gen: &mut OldGen,
        young_skips: &[(usize, usize)],
        fin_pass: Option<&mut OldGenFinalizerPass<'_>>,
        no_compact: bool,
    ) -> cratonvm_types::PointerMap {
        Self::major_gc_finalizing_with(roots, young_from, old_gen, young_skips, fin_pass, no_compact, false)
    }

    /// gci i8/pa — [`Self::major_gc_finalizing`] with `pc_in_place`: the moving
    /// cycle's Phase 5 decided (`moving_major_compaction_plan`) that P-C wins
    /// this major, so `CRATONVM_OLDGEN_COMPACT` does not compact it. Unlike
    /// `no_compact` this is not a veto (no log line, no
    /// `oldsz_moving_major_vetoes` count), and it never suppresses a
    /// fragmentation request: the plan only answers `true` when none was
    /// pending, and this call consumes the request as before.
    #[allow(clippy::too_many_arguments)]
    pub fn major_gc_finalizing_with(
        roots: &mut [ObjectRef],
        young_from: &Arena,
        old_gen: &mut OldGen,
        young_skips: &[(usize, usize)],
        fin_pass: Option<&mut OldGenFinalizerPass<'_>>,
        no_compact: bool,
        pc_in_place: bool,
    ) -> cratonvm_types::PointerMap {
        // See `oldgen_compact_enabled`: compaction is disabled by default as
        // of 2026-08-03 pending root-cause attribution of the corruption it
        // was found to cause (map-resize-unpinned-chain-cursors-nojit-segv-20260731-FIXED.md).
        let used_before = old_gen.used();
        // gen r4w4/oldgen4: a fragmentation compaction request compacts THIS
        // cycle only. Two setters, each behind its own switch: the proactive
        // trigger (`major_trigger_decision`, `CRATONVM_GC_OLD_FRAG_COMPACT`,
        // opt-in) and the allocation-failure ladder on its way to
        // `OutOfMemoryError` (`request_old_gen_compaction_if_fragmented_for`,
        // `CRATONVM_GC_OLD_OOM_COMPACT`, default on). The request is consumed
        // whether or not `old_gen_gc` can honour it (an interior root or a
        // walk gap downgrades it to the in-place arm).
        let frag_compact = old_gen.take_fragmentation_compaction_request();
        // gci i8/pa: P-C's in-place win drops only the OLDGEN_COMPACT half.
        let compact = (oldgen_compact_enabled() && !pc_in_place) || frag_compact;
        // Native copies of raw JNI handles cannot be rewritten by a slide.
        // Reclaim old garbage with the existing non-moving major instead.
        let no_compact = no_compact || cratonvm_gc_core::gc_quiescence::raw_jni_locals_open_or_closed();
        if compact && no_compact && old_gen.note_moving_major_compaction_veto() < 8 {
            tracing::info!(
                target: "cratonvm::gc",
                "old-gen compaction vetoed on a moving young cycle (live compiled frames or a \
                 ledger-pinned copy: unmapped words could name old objects); reclaiming in place",
            );
        }
        let map = Self::old_gen_gc_inner(
            roots,
            young_from,
            old_gen,
            compact && !no_compact,
            young_skips,
            None,
            fin_pass,
        );
        // gen r4w2/oldgen2: arms the trigger's hysteresis term and feeds the
        // low-yield instrument (see `major_trigger_decision`).
        old_gen.note_collection_end(used_before);
        Self::after_old_gen_collection(old_gen);
        map
    }

    /// gen r4w4/oldgen4 (2026-09-24) — the end of every STOP-THE-WORLD
    /// old-gen collection (`major_gc`, `sweep_old_gen_non_moving`), after
    /// `note_collection_end`: the post-collection half of the fragmentation
    /// trigger (opt-in) and the committed-size shrink (default on,
    /// `CRATONVM_GC_OLD_SHRINK=0` off). Both are `OldGen` policy; see
    /// `OldGen::note_fragmentation_after_collection` and
    /// `OldGen::resize_after_collection` for what they decide and why a
    /// stop-the-world pause holding the old-gen lock is where they may run.
    /// The concurrent sweep's end does NOT call this: its slices run beside
    /// mutators, and decommitting while they run is not argued anywhere.
    pub fn after_old_gen_collection(old_gen: &mut OldGen) {
        let flags = gc_flags();
        if flags.old_frag_compact {
            let _ = old_gen.note_fragmentation_after_collection();
        }
        let _ = old_gen.resize_after_collection(flags.old_shrink);
        // gen r5w3/oldgen7 (`CRATONVM_GC_OLD_BORROW_YOUNG`): a refusal since
        // the last collection that this one did not make room for grows the
        // capacity into the reservation's headroom. A no-op without headroom
        // (every heap built with the flag off). Both callers then cap the
        // young trigger and republish the region bounds before mutators run
        // (`cede_young_budget_to_old`; the moving path's end-of-cycle
        // republish, `run_non_moving_young_cycle`'s explicit one).
        let _ = old_gen.grow_after_refusal();
        // gcj j1/og (`CRATONVM_GC_OLD_FREE_RATIO_GROWTH`, default ON): HotSpot's
        // `MinHeapFreeRatio` expansion after a full collection, up to the
        // soft cap; the same two follow-ups as the refusal growth above (the
        // callers compare the capacity, cede the young budget and republish).
        // Inert without headroom or with the switch off.
        let _ = old_gen.grow_to_min_free_ratio();
        // gcj j3/og3 (B2, `CRATONVM_GC_OLD_CAPACITY_SHRINK`, default ON): a capacity
        // that grew into the reservation's headroom comes back down to where
        // the committed resize above left the generation (never below the
        // construction capacity or `MinHeapFreeRatio`; damped one collection
        // after any growth). The same follow-ups as a growth: both callers
        // compare the capacity, republish the region bounds and recompute the
        // young trigger (`cede_young_budget_to_old` raises it back under this
        // switch). Inert without a growth or with the switch off.
        let _ = old_gen.ungrow_after_collection();
        old_gen.defer_capacity_shrink(flags.old_shrink);
        // gen r5w5/old9 — `CRATONVM_DBG_OLD_PLANT_WALK_BREAK=<n>` (DEBUG, off
        // by default): at the end of the first stop-the-world old-gen
        // collection at or past the n-th (`OldGenTriggerStats::collections`),
        // plant one unsizable header in a fresh unreferenced block
        // (`OldGen::plant_walk_break`), so the walk-gap recoveries
        // (`CRATONVM_GC_OLD_WALK_GAP_RECOVERY`, `CRATONVM_GC_CONC_WALK_GAP_RECOVERY`)
        // and the live sweep's fallback can be exercised on demand. Unset: one
        // declared-flag lookup per old-gen collection.
        if let Some(n) = None::<std::ffi::OsString>
            .and_then(|v| v.to_str().and_then(|s| s.trim().parse::<u64>().ok()))
        {
            if old_gen.trigger_stats().collections >= n {
                if let Some(at) = old_gen.plant_walk_break() {
                    tracing::warn!(
                        target: "cratonvm::gc",
                        at = format!("{at:#x}"),
                        "CRATONVM_DBG_OLD_PLANT_WALK_BREAK: planted an unsizable old-gen header; \
                         every later walk of its region reports a gap",
                    );
                }
            }
        }
        // gen r4w5/oomjit5: print the root census `old_gen_gc` parked for this
        // collection, with what the collection left live. Inert without
        // `CRATONVM_DBG=oldmark-root-census` (nothing was parked).
        if oldmark_census::enabled() {
            oldmark_census::flush(old_gen.used());
        }
    }

    /// gen r5w3/oldgen7 — the interior half of the committed-size resize
    /// (`CRATONVM_GC_OLD_INTERIOR_DECOMMIT`, opt-in; needs
    /// `CRATONVM_GC_OLD_SHRINK`, on by default): run
    /// `OldGen::decommit_interior_free_runs` after a resize, i.e. after
    /// [`Self::after_old_gen_collection`] or after a post-concurrent resize
    /// that ran. Old-gen lock held, inside the stop-the-world pause.
    ///
    /// Not while a concurrent old-gen cycle is OPEN: its mark stack can still
    /// name an object a stop-the-world collection freed since it was pushed,
    /// and its scan checks only an object's two ends
    /// (`concurrent_mark.rs::scan_object_into`). A released interior granule
    /// under such an object would fault if a slice scanned it. (gci i13/cm
    /// item 3: no slice does today -- `concurrent_mark_budget` drops the queue
    /// when `reclaim_epoch` moved, at the top of every slice, under the same
    /// old-gen hold as its scans, and a stop-the-world collection cannot run
    /// inside a slice. This used to say the slices "keep draining until
    /// remark"; they stopped on 2026-09-23. The gate stays as defence in depth
    /// for the sweep's resume offset and the remark's walk.) The cycle ending
    /// re-arms the post-concurrent resize, which calls this again.
    pub fn decommit_old_interior_if_enabled(&self, old_gen: &mut OldGen) {
        let flags = gc_flags();
        if !flags.old_interior_decommit || !flags.old_shrink {
            return;
        }
        // Any phase but `Idle`: `cycle_in_progress` leaves out `InitialMark`,
        // the phase a cycle holds from `try_open_cycle` until its marking
        // starts, and an opened cycle is exactly what must not meet a hole.
        if self
            .conc_cycle_state
            .as_deref()
            .is_some_and(|s| s.phase() != crate::concurrent_mark::ConcurrentGcPhase::Idle)
        {
            return;
        }
        let _ = old_gen.decommit_interior_free_runs();
    }

    /// gen r5w6/old10 — [`Self::decommit_old_interior_if_enabled`] for the
    /// two young-pause call sites that run it after a POST-CONCURRENT resize
    /// (`resize_old_gen_after_concurrent_sweep` returned `true`). Same gates;
    /// the difference is the retry: when the interior pass is damped, or
    /// cannot run because a concurrent cycle is open, the episode keeps an
    /// attempt for the next young pause (`OldGen::keep_concurrent_resize_attempt`,
    /// bounded by `CONCURRENT_RESIZE_ATTEMPTS`) instead of waiting for the next
    /// concurrent cycle to end. `gengc-r4w5-oldcompact5-old-gen-shrink-waits-for-a-stop-the-world-collection`:
    /// after a growth phase the first interior pass after the drop is always
    /// damped (the committed size grew since the previous pass), and an
    /// undamped TAIL resize on that same pause used to end the episode.
    pub fn decommit_old_interior_after_concurrent_resize(&self, old_gen: &mut OldGen) {
        let flags = gc_flags();
        if !flags.old_interior_decommit || !flags.old_shrink {
            return;
        }
        if self
            .conc_cycle_state
            .as_deref()
            .is_some_and(|s| s.phase() != crate::concurrent_mark::ConcurrentGcPhase::Idle)
        {
            let _ = old_gen.keep_concurrent_resize_attempt();
            return;
        }
        let _ = old_gen.decommit_interior_after_concurrent_resize();
    }

    /// gen r5w3/oldgen7 (`CRATONVM_GC_OLD_BORROW_YOUNG`) — once the old
    /// generation has grown past its construction capacity
    /// (`OldGen::grow_after_refusal`), cap the young trigger at `(Xmx - old
    /// capacity) / 2`, so the young generation cedes the budget the old one
    /// borrowed: from-space is collected at or below that occupancy and its
    /// survivors (the to-space's worth) fit in it too, so both semi-spaces
    /// plus the old generation stay within `-Xmx` in COMMITTED terms. Called
    /// inside the pause that grew it (trigger writes are stop-the-world, see
    /// `young_gc_threshold`); a no-op on a heap that never grew, so the
    /// default trigger is untouched. `adapt_young_trigger_to_pause` applies
    /// the same cap to its ceiling.
    pub fn cede_young_budget_to_old(&self, old_cap: usize) {
        // gcj j3/og3 (B2, `CRATONVM_GC_OLD_CAPACITY_SHRINK`, default ON): after a
        // capacity shrink the cap rises (or goes), so hand back what the cede
        // took; read once per call (the end of an old collection that changed
        // the capacity, or of every moving major).
        // WARNING gcj-j2-residuals-flip-20261025: default ON, `=0` kills.
        if true {
            self.return_young_budget_from_old(old_cap);        }
        let Some(cap) = self.borrowed_young_trigger_cap(old_cap) else {
            return;
        };
        let current = self.young_gc_threshold.load(Ordering::Relaxed);
        if current > cap {
            self.young_gc_threshold.store(cap, Ordering::Relaxed);
            // gcj j3/og3: what the cede took, for `return_young_budget_from_old`.
            self.young_ceded_cap.store(cap, Ordering::Relaxed);
        }
    }

    /// gcj j3/og3 (B2 of `docs/feature-designs/generative-gc-architecture.md`)
    /// — the inverse of [`Self::cede_young_budget_to_old`] after the old
    /// capacity came back down (`OldGen::ungrow_after_collection`): when the
    /// young trigger still sits exactly at the cap the last cede lowered it to
    /// (no other policy moved it since) and the cap for `old_cap` is higher
    /// (or gone), raise the trigger to the configured one
    /// (`configured_young_trigger` over the published semi-space capacity),
    /// bounded by the new cap. A trigger some other policy (the pause-goal
    /// loop, the serial eden) has moved off the cap is left to that policy,
    /// whose own ceiling reads the new cap. Stop-the-world, as every trigger
    /// write.
    pub fn return_young_budget_from_old(&self, old_cap: usize) {
        let ceded = self.young_ceded_cap.load(Ordering::Relaxed);
        if ceded == 0 {
            return;
        }
        let now_cap = self.borrowed_young_trigger_cap(old_cap);
        if now_cap.is_some_and(|c| c <= ceded) {
            return;
        }
        self.young_ceded_cap.store(now_cap.unwrap_or(0), Ordering::Relaxed);
        let current = self.young_gc_threshold.load(Ordering::Relaxed);
        if current != ceded {
            return;
        }
        let semi = self.young_from_pub_capacity.load(Ordering::Relaxed);
        if semi == 0 {
            return;
        }
        let ceiling = self.configured_young_trigger(semi).min(now_cap.unwrap_or(usize::MAX));
        if ceiling > current {
            self.young_gc_threshold.store(ceiling, Ordering::Relaxed);
        }
    }

    /// The young-trigger cap [`Self::cede_young_budget_to_old`] applies, or
    /// `None` when the old generation has not grown past its construction
    /// capacity (or the heap has no `-Xmx` budget).
    pub fn borrowed_young_trigger_cap(&self, old_cap: usize) -> Option<usize> {
        if self.max_heap_bytes == usize::MAX || old_cap <= self.initial_old_gen_capacity {
            return None;
        }
        Some((self.max_heap_bytes.saturating_sub(old_cap) / 2).max(1))
    }

    /// gen r4w6/oldpin6 (2026-09-24) — the young-pause half of the resize for
    /// a CONCURRENT old-gen cycle: called by both young paths (the moving
    /// `collect_garbage_inner`'s Phase 5 and `run_non_moving_young_cycle`) on
    /// a pause that ran NO old-gen collection, under the old-gen lock, inside
    /// the stop-the-world pause. Free-ratio capacity expansion obeys
    /// `CRATONVM_GC_OLD_FREE_RATIO_GROWTH` independently of commitment shrinking.
    /// The latter is opt-in, `CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT`, and still
    /// obeys `CRATONVM_GC_OLD_SHRINK`. See
    /// `OldGen::resize_after_concurrent_sweep_if_due` for the policy and the
    /// safety argument, and
    /// `docs/feature-designs/generative-gc-architecture.md`.
    ///
    /// Returns whether a resize RAN (gen r5w3/oldgen7: the caller then runs
    /// the interior half, [`Self::decommit_old_interior_if_enabled`], only
    /// then — not on every young pause).
    pub fn resize_old_gen_after_concurrent_sweep(old_gen: &mut OldGen) -> bool {
        let flags = gc_flags();
        let resized = old_gen.resize_capacity_after_young_pause(flags.old_shrink)
            || (flags.old_shrink_after_concurrent
                && old_gen
                    .resize_after_concurrent_sweep_if_due(flags.old_shrink)
                    .is_some());
        resized
    }

    #[cfg(test)]
    pub fn assert_concurrent_capacity_growth_at_young_pause_for_test(enabled: bool) {
        const M: usize = 1024 * 1024;
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_GC_OLD_BORROW_YOUNG", Some("1")),
                ("CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT", Some("0")),
            ],
            || {
                let heap = Self::with_capacity(64 * M);
                let initial = heap.old_gen_capacity();
                let mut old = heap.old_gen.lock();
                if old.growth_max() == initial {
                    return;
                }
                old.set_free_ratio_growth_for_test(enabled);
                old.alloc(30 * M, 8).expect("a retained old live set");
                old.note_concurrent_collection_end(0);
                assert!(!gc_flags().old_shrink_after_concurrent);
                let resized = Self::resize_old_gen_after_concurrent_sweep(&mut old);
                let grew = heap.grow_old_gen_after_concurrent_sweep(&mut old);
                assert_eq!(resized || grew, enabled);
                let capacity = old.capacity();
                assert_eq!(
                    capacity,
                    if enabled {
                        old_borrow_max(64 * M)
                    } else {
                        initial
                    }
                );
                assert!(!Self::resize_old_gen_after_concurrent_sweep(&mut old));
                drop(old);
                heap.cede_young_budget_to_old(capacity);
                heap.refresh_region_bounds();
                if enabled {
                    assert!(
                        heap.young_gc_threshold.load(Ordering::Relaxed) <= (64 * M - capacity) / 2
                    );
                    let object = heap
                        .old_gen
                        .lock()
                        .alloc(4 * M, 8)
                        .expect("published grown span");
                    assert!(heap.is_old_gen_addr(object as usize));
                }
            },
        );
    }

    /// gcj j2/og2 B4 — the young-pause half of the free-ratio growth for a
    /// CONCURRENT old-gen cycle (`OldGen::grow_to_min_free_ratio_after_concurrent_sweep_if_due`),
    /// on a pause that ran no old-gen collection, under the old-gen lock.
    /// Skipped, and kept due, while a concurrent cycle is open (any phase but
    /// `Idle`, as [`Self::decommit_old_interior_if_enabled`] asks): a growth
    /// moves the reclaim epoch, which would abandon that cycle. Returns
    /// whether the capacity grew; the callers then cede the young budget and
    /// republish the region bounds, as after a stop-the-world growth.
    pub fn grow_old_gen_after_concurrent_sweep(&self, old_gen: &mut OldGen) -> bool {
        if self
            .conc_cycle_state
            .as_deref()
            .is_some_and(|s| s.phase() != crate::concurrent_mark::ConcurrentGcPhase::Idle)
        {
            return false;
        }
        old_gen.grow_to_min_free_ratio_after_concurrent_sweep_if_due() > 0
    }

    /// Mark old space from the complete root set and either compact it or
    /// reclaim dead blocks in place. `compact` is the caller's answer: YES by
    /// default since 2026-10-13 (`oldgen_compact_enabled`,
    /// `CRATONVM_OLDGEN_COMPACT`, WARNING gci-user-flip-20261013) or on one
    /// cycle's fragmentation request; a moving Phase 5 major P-C takes
    /// reclaims in place (`moving_major_compaction_plan`), and the
    /// moving Phase 5 vetoes it under live compiled frames
    /// (`major_gc_finalizing`'s `no_compact`). (gch h1/jo: this used to say
    /// "compact when every root is rewritable", a default that does not exist.)
    ///
    /// `young_skips` is the from-space skip list — the free list MERGED with
    /// the reserved-TLAB-tail regions of forcibly-stopped in-JIT peers. Both
    /// young walks below need it; see `mark_young_to_old_refs` for what goes
    /// wrong when a reserved tail is walked as if it held objects.
    pub fn old_gen_gc(
        roots: &mut [ObjectRef],
        young_from: &Arena,
        old_gen: &mut OldGen,
        compact: bool,
        young_skips: &[(usize, usize)],
    ) -> cratonvm_types::PointerMap {
        Self::old_gen_gc_inner(roots, young_from, old_gen, compact, young_skips, None, None)
    }

    /// gcj j3/og3 — the `[gcphase]` lines after a compaction (both arms of
    /// [`Self::old_gen_gc_inner`]; printed only under `CRATONVM_DBG_GCPHASE`):
    /// `old-gen <arm>-forward-index: count=1` when the pinned compactor's
    /// Phase 2 used the forward index (`fwd_index_before` is `Some` only for
    /// it; the slide prints its own `compact-forward-index` line), `old-gen
    /// compact-phase0: part=<us>us` (0 when skipped), `old-gen
    /// compact-phase0-skipped: count=1` when the Phase 0 drop engaged, and
    /// `old-gen compact-trust-mark-disagreement: count=<n>` under the verify
    /// mode. The `*_before` arguments are the counters read before the call.
    pub fn note_compaction_phase0_lines(
        timer: &OldPhaseTimer,
        old_gen: &OldGen,
        arm: &str,
        fwd_index_before: Option<u64>,
        phase0_skips_before: u64,
        disagreements_before: u64,
    ) {
        if !timer.is_on() {
            return;
        }
        if let Some(before) = fwd_index_before {
            let used = old_gen.forward_index_compactions().wrapping_sub(before);
            if used > 0 {
                timer.count(&format!("{arm}-forward-index"), used as usize);
            }
        }
        timer.part("compact-phase0", u128::from(old_gen.last_phase0_us()));
        let skipped = old_gen.phase0_skips().wrapping_sub(phase0_skips_before);
        if skipped > 0 {
            timer.count("compact-phase0-skipped", skipped as usize);
        }
        let disagreed = old_gen.trust_mark_disagreements().wrapping_sub(disagreements_before);
        if disagreed > 0 {
            timer.count("compact-trust-mark-disagreement", disagreed as usize);
        }
    }

    /// [`Self::old_gen_gc`], plus (gen r4w5/oldcompact5) an optional PINNED
    /// compaction for the non-moving path: with `pinned`, the mark also
    /// collects the bases conservative words name (see [`OldPinnedCompact`]),
    /// and after the mark the generation is compacted around them
    /// (`OldGen::compact_around_pins`) instead of swept in place. A refusal
    /// (walk gap, escape) falls through to the in-place arm below, over the
    /// same grid; nothing has moved by then. `pinned` is only ever passed with
    /// `compact == false`.
    ///
    /// gc-common w36-d: `fin_pass`, when given, runs the old generation's
    /// FINALIZER RESURRECTION after the root closure (see
    /// [`OldGenFinalizerPass`]): a registered finalizable that the closure did
    /// not reach is marked, its subgraph traced, and its PRE-compaction address
    /// recorded, so the collection keeps it for `finalize()` instead of freeing
    /// it in place. The caller maps the recorded addresses through the map this
    /// returns.
    pub fn old_gen_gc_inner(
        roots: &mut [ObjectRef],
        young_from: &Arena,
        old_gen: &mut OldGen,
        compact: bool,
        young_skips: &[(usize, usize)],
        mut pinned: Option<&mut OldPinnedCompact>,
        mut fin_pass: Option<&mut OldGenFinalizerPass<'_>>,
    ) -> cratonvm_types::PointerMap {
        // gcd d4/n: a REQUESTED major's true-root young seed (see
        // `TrueRootYoung`), taken once per call whatever the arm below; only an
        // in-place collection uses it (gcd d9/a: or one with a pinned plan,
        // unless `CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE=0`).
        // gcf f2/c: old9 step 1, the phase timer (`CRATONVM_DBG_GCPHASE`).
        let mut phase_timer = OldPhaseTimer::new();
        // gch h2/og (ob's X3): the `-Xlog:gc+phases` split, always stamped
        // (three clock reads per old collection; see `OldGcPhaseSplit`).
        let t_entry = std::time::Instant::now();
        let mut true_root_ask = TRUE_ROOT_MARK_THIS_CALL.with(|c| c.borrow_mut().take());
        let true_roots = true_root_ask
            .as_ref()
            .is_some_and(|ask| !compact && (pinned.is_none() || ask.wide));
        // gcj j2/rf2 (ec47): the true-root seed leaves the finalizer candidates
        // to the resurrection round (`seed_deferred_finalizer_seeds`). gcj
        // j3/x2 R3: the pass's own latch (`OldGenFinalizerPass::defers_seeds`),
        // so a moving major and its Phase 5 caller read the switch once.
        let defer_fin_seeds = fin_pass.as_ref().is_some_and(|p| p.defers_seeds);
        // gcd d5/r: a declined ask is a fallback too (it was counted nowhere:
        // neither a true-root major nor a fallback).
        // gci i4/og (`gch-h3mj-...-review-findings` item 5): labelled by the
        // reason that declined it (no caller arms an ask under `compact`
        // today, so `Compacting` counts 0 until one does).
        if true_root_ask.is_some() && !true_roots {
            // gc wave 9 lane F (h3mj item 5): name the real reason.
            let why = if compact {
                crate::old_gen::TrueRootFallback::Compacting
            } else {
                crate::old_gen::TrueRootFallback::PinnedPlan
            };
            note_true_root_fallback(old_gen, why, 0);
        }
        // gcg g5/og (2026-10-02): the O(live) collection that used to answer
        // here (`CRATONVM_GC_OLD_LIVE_SWEEP` and its four sub-switches, gen
        // r5w5/old9 .. gcg g3/e) is DELETED. It lost to this walked
        // collection on every measured shape: the steady major 1147 vs 437 ms
        // and the sparse major 84 vs 75 ms with the true-root seed (vg3
        // `e3_tr_kb` vs `e3_tr_claim`), 1.66 vs 0.48 s per occupancy major on
        // `GenR4W4SteadyPromotionProbe` (vg4 `og4_ls_all_A` vs `og4_del_adm`).
        // `gcg-g5og-the-o-live-old-gen-sweep-is-deleted-20261002`.
        // ---- Mark phase ---- BFS from roots + young-gen cross-references ----

        // gcj j3/og3 (Phase 0 drop): the mark-fact counter before this mark
        // set its first bit; the compacting arm compares it after.
        let mark_facts_before = old_gen.mark_gap_facts();
        // t1 G2 (`CRATONVM_GC_OLD_PAR_MARK`, default ON since t1 2026-10-27): the parallel drain's
        // worker count, read once per generation (per-VM); `< 2` is serial.
        let par_mark_workers = old_gen.par_mark_workers();
        let closures_before = crate::old_gen::CLOSE_LIVE_SET_CALLS.load(Ordering::Relaxed);
        // t1 w8 J1OG (`CRATONVM_GC_OLD_PAR_CLOSE`): engagement, `[gcphase]` only.
        let par_close_before =
            crate::old_gen::CLOSE_LIVE_SET_PAR_VERIFIED.load(Ordering::Relaxed);
        let mut worklist: Vec<*mut u8> = Vec::new();
        // gen r4w5/oldcompact5: the bases this pause pins, when a pinned
        // compaction is planned — built by the mark below, consumed by the
        // compaction, dropped with the pause.
        // gen r4w6/oldpin6: a range of the slice, no longer a prefix.
        let (pin_root_lo, pin_root_hi) = pinned.as_ref().map_or((0, 0), |p| p.pin_root_range);
        let mut pin_bases: Vec<usize> = Vec::new();
        // gen r5w2/alloc6: `pin_srcs[i]` is the `OLD_PIN_SRC_*` bit that pushed
        // `pin_bases[i]` (see `settle_old_pins`). Kept in step at every push.
        let mut pin_srcs: Vec<u8> = Vec::new();

        // GCAUD-8: derive the old-gen object grid ONCE, up front, and use it as
        // the mark phase's fail-closed admission oracle.
        //
        // `walk_objects` does not guess at headers the way
        // `old_gen_mark_candidate_plausible` does: it derives allocated extents
        // as the gaps between free blocks and strides header-by-header, so an
        // address it yields is an object BASE by construction. That is a
        // strictly stronger proof than the byte-plausibility screen, and it is
        // the proof `mark_young_to_old_refs`'s conservative fallback has always
        // used for exactly this purpose (its conservative word scan). Hoisted
        // here so every precise push site can use it too, and so the `!compact`
        // arm below does not walk a second time.
        //
        // Stability: nothing between here and the free loop allocates or frees
        // in old gen, and the only header bytes the mark writes are
        // `GC_FLAG_MARKED` — which no sizing path reads (`gen_object_total_size`
        // and `scan_region` look at `kind`, `shape`, `element_type`,
        // `class_id`, and `GC_FLAG_COMPACT`). So this grid stays exact for the
        // whole phase. It must NOT outlive the old-gen lock: admitting a mark
        // through a stale base is the corruption this oracle exists to prevent.
        //
        // gengc-round2-core2 (2026-09-20): there used to be a second list here,
        // `walked_bases: Vec<usize>` — a full n-element allocation and copy of
        // the `.0`s of the slice above, made inside the pause and proportional
        // to the old live set, so that the membership test could be a bare
        // `binary_search`. Every consumer now searches `walked_objects` by key
        // through [`walked_grid_has_base`], which is what
        // `close_live_set_over_old_gen` had always done against the same
        // slice. Pure deletion: the same addresses, the same order, the same
        // answers.
        //
        // gen r4w4/oldgen4 (2026-09-24): with the ranges the walk could NOT
        // cover (`walk_gaps`, offsets). When they account exactly for the
        // bytes the walk missed, they are scanned as conservative roots below
        // (`seed_old_gen_walk_gaps`) and the in-place sweep keeps reclaiming
        // instead of freeing nothing forever — see `gap_recovery`.
        let (walked_objects, walk_gaps) = old_gen.walk_objects_with_gaps();
        // t1 w7 G2: the workers this mark starts, scaled to the walked old
        // generation (serial below `par_mark::PAR_MARK_MIN_OBJECTS`).
        let par_mark_workers = crate::old_gen::par_mark::effective_workers(
            par_mark_workers,
            walked_objects.len(),
            old_gen.par_mark_min_objects(),
        );
        // The BFS pushes only newly marked objects. A mark inherited from an
        // earlier collector therefore cannot certify its outgoing closure.
        // Retain Phase 0 on that cycle, including when trust-mark is requested.
        let trust_mark_requested = compact;
        let marks_at_entry = trust_mark_requested && walked_objects.iter().any(|&(p, _)| {
            unsafe { (*(p as *const ObjectHeader)).gc_flags() & GC_FLAG_MARKED != 0 }
        });

        let gap_bytes: usize = walk_gaps.iter().map(|&(s, e)| e - s).sum();
        let gap_recovery = !walk_gaps.is_empty()
            && gc_flags().old_walk_gap_recovery
            && walked_objects
                .iter()
                .map(|&(_, size)| size)
                .sum::<usize>()
                .checked_add(gap_bytes)
                == Some(old_gen.used());
        // gen r4/oldgen (2026-09-23): the free-list stamp this grid is exact
        // for. The compacting arm hands both to
        // `OldGen::compact_with_walked_objects` instead of letting `compact`
        // walk the generation a second time; a stamp that has moved by then
        // makes it re-walk rather than trust the grid.
        let walked_at_seq = old_gen.free_list_seq();
        debug_assert!(
            walked_objects
                .windows(2)
                .all(|w| (w[0].0 as usize) < (w[1].0 as usize)),
            "walk_objects must yield ascending object starts — the mark oracle binary-searches it",
        );
        // gcg g3/e — `CRATONVM_GC_OLD_SHARED_STARTS` (opt-in): one start set
        // per collection, for the BFS's and the young seed's admission
        // (`MarkAdmission`) and for every closure call below. Off: the walked
        // grid's binary search. (gcg g4/og: `CRATONVM_GC_OLD_MARK_ADMISSION`,
        // gcf f2/c's `bitmap` / `bot` admission, is deleted -- triage tg1
        // measured no gain; its `bitmap` arm is this set.)
        let walked_starts = if old_shared_starts_enabled() {
            WalkedStarts::build(&walked_objects)
        } else {
            None
        };
        if walked_starts.is_some() {
            old_gen.note_mark_admission();
        }
        let closure_starts: Option<&crate::old_gen::WalkStartBits> =
            walked_starts.as_ref().map(WalkedStarts::bits);
        phase_timer.phase("walk");

        // gcd d4/n: a requested major over a gap-free old walk seeds young
        // from the true roots (`TrueRootYoung`, built below); every other
        // collection keeps the legacy seed. gcd d9/a: decided HERE, before
        // the root loop, because the true-root seed also resolves this
        // pause's promotions (`source -> destination`), so the root loop and
        // the census skip the destinations the caller appended
        // (`roots[promo_tail..]`); on a fallback they stay roots, as before.
        // `promoted` is `(source, destination, walked size)`, ascending by
        // source; a destination that is not a walked base cannot be resolved
        // by extent, so the whole major keeps the legacy seed (a safety
        // fallback that a sane heap never takes).
        let mut promoted: Vec<(usize, usize, usize)> = Vec::new();
        let true_young_on = match true_root_ask.as_ref() {
            Some(ask) if true_roots => {
                if !walk_gaps.is_empty() {
                    note_true_root_fallback(
                        old_gen,
                        crate::old_gen::TrueRootFallback::WalkGap,
                        gap_bytes,
                    );
                    false
                } else {
                    promoted.reserve(ask.promotions.len());
                    let mut off_grid = 0usize;
                    for &(src, dest) in &ask.promotions {
                        match walked_objects.binary_search_by_key(&dest, |&(p, _)| p as usize) {
                            Ok(i) => promoted.push((src, dest, walked_objects[i].1)),
                            Err(_) => off_grid += 1,
                        }
                    }
                    if off_grid > 0 {
                        note_true_root_fallback(
                            old_gen,
                            crate::old_gen::TrueRootFallback::PromotionOffGrid,
                            off_grid,
                        );
                        promoted.clear();
                        false
                    } else {
                        true                    }                }            }
            _ => false,        };
        let promo_tail = match true_root_ask.as_ref() {
            Some(ask) if true_young_on => ask.promo_tail.min(roots.len()),
            _ => roots.len(),
        };
        // gcj j3/rf3 residual (ec47, the legacy seed's PROMOTED members;
        // `CRATONVM_GEN_LEGACY_SEED_DEFERS_RESURRECTED`, read once here): a
        // member of the young resurrection set the young phase promoted has
        // an OLD current address, so the young walk below never meets it, but
        // the caller roots it as a promotion destination (the non-moving
        // `root_shadow`, a moving fallback's tail). Rooted, it marked its old
        // children strongly. On the legacy arm such a root is a deferred seed
        // instead (the round always marks one, so it is kept as the root kept
        // it): the young phase judged a member reachable only through a dead
        // finalizable, so no true root names it. Not inside a pinned plan's
        // conservative range, whose roots must stay pins.
        let legacy_defers = defer_fin_seeds
            && !true_young_on
            && fin_pass.as_ref().is_some_and(|p| !p.young_resurrected.is_empty())
            && legacy_seed_defers_resurrected_enabled();
        let promoted_members: Vec<usize> = match fin_pass.as_deref() {
            Some(pass) if legacy_defers => pass
                .young_resurrected
                .iter()
                .copied()
                .filter(|&a| old_gen.contains(a as *const u8))
                .collect(),
            _ => Vec::new(),
        };
        let mut promoted_deferred: Vec<usize> = Vec::new();

        // Seed: root ObjectRefs that point into old gen.
        //
        // xt-hardening follow-up (2026-07-03): `roots` includes CONSERVATIVE
        // candidates (register/stack-scanned guesses — the same `xt_roots`
        // whose flood exposed the young-gen `mark_young` header-write
        // corruptor). `OldGen::contains` is a bare bounds check (no
        // alignment, no header validation), so before this fix ANY garbage
        // address landing inside old gen's byte range got `gc_flags` blindly
        // RMW'd — the exact same corruption family, on the OTHER generation.
        // Reject implausible candidates instead of marking them
        // (skip-on-implausible, never corrupt) — old-gen compaction decides
        // liveness purely from `gc_flags & GC_FLAG_MARKED`, so unlike the young
        // sweep there is no side-mark-set escape hatch; the safe choice for an
        // address that fails these checks is to not mark it (over-retention is
        // not even at stake here — a failing candidate was never a valid object
        // to begin with).
        //
        // 2026-07-31: this screen is no longer local to the root seed. It lives
        // in `old_gen_mark_candidate_plausible` and EVERY push site into the
        // mark worklist goes through it, because the seven that did not were
        // still doing exactly what the paragraph above describes — see
        // `gc-old-gen-mark-accepts-unvalidated-addresses-FIXED.md`.
        //
        // NOTE this predicate deliberately does NOT require a non-zero first
        // header word (unlike `mark_young`'s zero-word0 side-mark split):
        // `ClassId(0)` ad-hoc containers (class_id=0, kind=Object=0,
        // element_type=Reference=0, `_padding`=0) are a first-class supported
        // shape whose word0 is legitimately all-zero — rejecting them here
        // broke real promoted objects (major_gc_frees_old_gen_garbage et al).
        // A zero-word0 candidate additionally passes through
        // `victim8_neighbor_explains_zero_prefix` — a targeted check for the
        // dominant real-world shape of this ambiguity, `candidate = victim
        // − 8`: if the 8 bytes at `candidate` are just the tail
        // zero-padding/hash-prefix of a SEPARATE, independently-plausible
        // object starting at `candidate + 8`, `candidate` itself is not a
        // real header and is rejected. A genuine zero-hash `ClassId(0)`
        // container's successor 8 bytes are its own body/next-object data,
        // essentially never a coincidentally-valid, independently-fitting
        // header — false rejects of real containers are not expected. Old
        // gen has no side-mark-set escape hatch (compaction PHYSICALLY
        // SLIDES live objects; treating an unrelated garbage candidate as
        // live would copy garbage over/into a real neighbor during the
        // slide — strictly worse than skipping), so reject is the only safe
        // response to a candidate that fails this check; verified this
        // session (disassembly + byte-exact match on the fabricated
        // pointer `0x0000020000000000` = hash(0)‖array_length(512) read
        // from a corrupted victim's header) to be the mechanism behind the
        // pre-xt-activation background DoHead crash face.
        // H2-CID0 (2026-08-02) — INTERIOR conservative roots.
        //
        // Both screens below ask the same question: "is this address an object
        // BASE?" A conservative root frequently is not. It is a field address,
        // an array element, a derived pointer, or a register spilled
        // mid-object — and the young collector has an entire exact-base oracle
        // for precisely that reason: `mark_young` resolves an interior word to
        // the object that CONTAINS it and keeps that object alive. Old gen had
        // no such resolution, so an old-gen object whose only surviving
        // reference was an interior word in a register or a stack slot was
        // never marked, and this sweep frees purely on `GC_FLAG_MARKED`.
        //
        // That is the same defect `b5fc69a6fc` fixed one generation over (see
        // `tests::interior_conservative_root_pins_the_object_it_points_into`),
        // and BOTH old-gen reclamation arms have it. `CRATONVM_NO_MOVING_YOUNG=1`
        // with the JIT ON — the H2 `MVStore` reproduction's configuration — puts
        // the in-place sweep on the cycles `has_conservative_roots` catches and
        // lets every OTHER cycle fall through to the compactor, which is where
        // the measured victim was actually lost. Full argument and
        // measurements: old-sweep-liveness.md section 7.
        //
        // Pinning is pure over-retention: the object does not move, and a
        // false positive retains one block for one cycle, which is what
        // conservative marking does anyway.
        //
        // On the COMPACTING arm a mark would not be enough — the object would
        // be slid and the interior word could not be rewritten — so that arm
        // does not get a pin, it gets DOWNGRADED to this one. The first version
        // of this fix instead argued the compacting arm could not see such a
        // root (`major_gc` runs from the moving young cycle, whose collector
        // rewrites every root it is handed); `COMPACT_DROPPED_INTERIOR_ROOT`
        // refuted that on the first H2 run. See the downgrade below.
        // gen r4/oldgen (2026-09-23): value-aware, not presence. As a bare
        // `.is_some()`, `CRATONVM_GC_NO_OLD_INTERIOR_PINS=0` DISABLED the pin
        // (and with it the compaction downgrade) — i.e. the negative control
        // switched on by the spelling that means "leave it alone".
        let no_interior_pins =
            false;
        // gen r4/oldgen: Fx, not SipHash. Besides the per-root insert, the
        // in-place free loop asks this set twice per DEAD object (the
        // negative-control reporter and `drop_flags_for`), and under JIT
        // conservative roots it is routinely non-empty. Integer keys, no
        // adversary: the crate's usual choice (`mirror_pin_owners` below).
        let mut interior_pins: FxHashSet<usize> = FxHashSet::default();

        // gen r4w5/oomjit5 (2026-09-24): `CRATONVM_DBG=oldmark-root-census`.
        // Attribute, per root category, the old-gen bytes the root set below
        // keeps alive — over exactly the slice this seed loop is about to
        // consume, before it consumes it. Read-only: its own visited set, no
        // mark bit written, so the mark below is unchanged with the token on.
        if oldmark_census::enabled() {
            oldmark_census::run(
                &roots[..promo_tail],
                young_from,
                old_gen,
                &walked_objects,
                young_skips,
                compact,
                true_young_on,
            );
        }

        // gengc-round2-core2 (2026-09-20): ONE pass over `roots`, not two.
        //
        // This loop used to be two: a pin-collection pass that resolved every
        // root with `old_gen_interior_root_base` and recorded the bases, then
        // the compaction downgrade, then a seed pass that resolved every root
        // with `old_gen_interior_root_base` AGAIN to answer the identical
        // question. Under conservative stack/register scanning — the default
        // whenever compiled code exists — `roots` carries whole frozen-peer
        // register files and stack bands, so that is two `partition_point`s
        // per root over the old-gen object grid, inside the pause, to compute
        // the same answer twice. See
        // `docs/feature-designs/generative-gc-architecture.md`.
        //
        // Fusing them is behaviour-preserving, and the one ordering question
        // is worth stating because it looks like it should not be. The
        // interior branch's own note says "Retaining is sufficient here
        // because the block above has already guaranteed this cycle does not
        // move anything" — i.e. the mark below used to rely on the downgrade
        // having ALREADY set `compact = false`, and the downgrade now runs
        // after this loop. The implication still holds, because it never
        // depended on the order: the mark is guarded by `!no_interior_pins`,
        // and it can only run after `interior_pins.insert(base)` has made the
        // set non-empty, so `compact && !interior_pins.is_empty() &&
        // !no_interior_pins` is necessarily true by the time it is evaluated.
        // Both halves are decided by the same two facts, whichever is read
        // first. (The pin counter and the reporter are run-totals, so
        // interleaving them with the marks changes no observable value.)
        //
        // gcd d9/a: not over `roots[promo_tail..]` when the true-root seed
        // resolves the promotions (see `promo_tail`); the whole slice
        // otherwise, as before.
        for (root_index, root) in roots[..promo_tail].iter().enumerate() {
            let ptr = root.as_ptr();
            if !promoted_members.is_empty()
                && promoted_members.binary_search(&(ptr as usize)).is_ok()
                && !(pinned.is_some() && root_index >= pin_root_lo && root_index < pin_root_hi)
            {
                promoted_deferred.push(ptr as usize);
                continue;
            }
            // `conservative = true`: these are register/stack-scanned GUESSES,
            // so they also need the zero-word0 neighbour disambiguation. The
            // rest of the screen is the same one every other push site now
            // uses — see `old_gen_mark_candidate_plausible`.
            // GCAUD-8: the walked object grid overrides a screen reject. The
            // `conservative` clause above adds `victim8_neighbor_explains_zero_prefix`,
            // a heuristic about a zero first header word — and this slice mixes
            // PRECISE roots (JNI globals, statics, pins: see
            // `vm/src/memory/roots.rs`) with conservative stack guesses, with no
            // way to tell them apart here. A precise root whose referent the
            // heuristic rejects is a live object nobody else may name, and this
            // sweep would free it. The oracle is safe for the conservative half
            // too: a walked base IS an object, so admitting one can only
            // over-retain, which is what conservative marking does anyway.
            // H2-CID0 (2026-08-02): containment in the DERIVED object grid is
            // asked FIRST, because it outranks the byte-plausibility heuristic
            // below on exactly the addresses where the two can disagree. An
            // address strictly inside a walked object is not an object base —
            // the screen calling it one writes `GC_FLAG_MARKED` into that
            // object's payload and then decodes the payload as an
            // `ObjectHeader` — and a zeroed body reads as a perfectly plausible
            // `num_slots=0` header, so this is the common case rather than a
            // corner one.
            if let Some((base, extent)) = old_gen_interior_root_base(ptr, &walked_objects) {
                if interior_pins.insert(base) {
                    let n = OLDMARK_INTERIOR_ROOT_PINS.fetch_add(1, Ordering::Relaxed);
                    if n < 8 {
                        note_interior_old_root(ptr, base, extent);
                    }
                    // gen r4w5/oldcompact5: an interior word is never rewritable,
                    // so its object is a pin of any pinned compaction.
                    if pinned.is_some() {
                        pin_bases.push(base);
                        pin_srcs.push(OLD_PIN_SRC_INTERIOR);
                    }
                }
                // The RESOLUTION always wins over the screens below: an interior
                // address is not an object base whatever its bytes say, and
                // letting the screen call it one writes a mark bit into the
                // containing object's PAYLOAD and then decodes that payload as
                // a header. Retaining is sufficient here because the downgrade
                // below is guaranteed to take this cycle off the compacting
                // arm — see the fusion note above this loop.
                if !no_interior_pins {
                    // SAFETY: `base` is an object base `walk_objects` yielded,
                    // so its header is valid and mutable.
                    let header = unsafe { &mut *(base as *mut ObjectHeader) };
                    if header.gc_flags() & GC_FLAG_MARKED == 0 {
                        header.add_gc_flags(GC_FLAG_MARKED);
                        worklist.push(base as *mut u8);
                    }
                }
                continue;
            }
            // gen r5w1/oldgen5 (review6 finding 3, the root-slice half; the
            // published-word half landed in wave 6 below): a root in the
            // slice's pin range is an unrewritable conservative word, and a
            // compiled loop's one-past-the-end bound (`arr + len * 8`) equals
            // the NEXT object's base, the start of a free block or the
            // generation's end. Pin the object `ptr - 1` lies strictly inside
            // too, whatever the screen below says about `ptr` itself.
            // Over-pinning only moves less; `compact_around_pins` sorts and
            // deduplicates. Only with a pinned compaction planned, so every
            // other collection is unchanged.
            if pinned.is_some() && root_index >= pin_root_lo && root_index < pin_root_hi {
                if let Some((base, _)) = old_gen_interior_root_base(
                    (ptr as usize).wrapping_sub(1) as *mut u8,
                    &walked_objects,
                ) {
                    pin_bases.push(base);
                    pin_srcs.push(OLD_PIN_SRC_RANGE_END);
                }
            }
            if old_gen_mark_candidate_plausible(ptr, old_gen, true)
                || rescue_mark_candidate_by_walk(ptr, old_gen, &walked_objects)
            {
                // SAFETY: the screen verified 8-alignment, old-gen containment
                // and a header whose claimed extent fits inside old gen; on the
                // rescue path `walk_objects` yielded `ptr` as an object base,
                // which implies all three.
                let header = unsafe { &mut *(ptr as *mut ObjectHeader) };
                if header.gc_flags() & GC_FLAG_MARKED == 0 {
                    header.add_gc_flags(GC_FLAG_MARKED);
                    worklist.push(ptr);
                }
                // gen r4w5/oldcompact5: a root of the caller's slice is pinned
                // when an unpublished conservative source ran this pause (see
                // `OldPinnedCompact`). Only a walked base can be a pin: an
                // address the screen admitted inside a free block names no
                // object the compaction could move.
                if root_index >= pin_root_lo
                    && root_index < pin_root_hi
                    && walked_grid_has_base(&walked_objects, ptr as usize)
                {
                    pin_bases.push(ptr as usize);
                    pin_srcs.push(OLD_PIN_SRC_RANGE);
                }
            }
        }
        // gcj j3/rf3 residual: the promoted members the loop left to the
        // resurrection round (counted with the young walk's deferrals).
        if !promoted_deferred.is_empty() {
            promoted_deferred.sort_unstable();
            promoted_deferred.dedup();
            LEGACY_SEED_DEFERRED_REFERENTS
                .fetch_add(promoted_deferred.len() as u64, Ordering::Relaxed);
            if watchref_dbg() {
                eprintln!(
                    "[watchref] major: legacy seed deferred {} promoted resurrection member root(s) to the resurrection round",
                    promoted_deferred.len(),
                );
            }
            if let Some(pass) = fin_pass.as_deref_mut() {
                pass.deferred_seeds.extend_from_slice(&promoted_deferred);
            }
        }
        // gen r4w5/oldcompact5: the words the root gatherer published as
        // conservative for this pause, resolved against the grid (base or
        // interior). A word that names no walked object pins nothing.
        //
        // gen r4w6 (review6 finding 3): each word also pins the object that
        // `word - 1` lies in. A compiled loop's one-past-the-end bound
        // (`arr + len * 8`) equals the NEXT object's base; without the second
        // probe `arr` slid and the unrewritable register ran over whatever
        // took its place. Over-pinning only moves less. `compact_around_pins`
        // sorts and deduplicates.
        if let Some(plan) = pinned.as_ref() {
            for &word in &plan.words {
                for (probe, src) in [
                    (word, OLD_PIN_SRC_WORD),
                    (word.wrapping_sub(1), OLD_PIN_SRC_WORD_END),
                ] {
                    if walked_grid_has_base(&walked_objects, probe) {
                        pin_bases.push(probe);
                        pin_srcs.push(src);
                    } else if let Some((base, _)) =
                        old_gen_interior_root_base(probe as *mut u8, &walked_objects)
                    {
                        pin_bases.push(base);
                        pin_srcs.push(src);
                    }
                }
            }
        }

        // H2-CID0 (2026-08-02) — an interior root FORBIDS compaction.
        //
        // The first version of this fix marked the containing object and left
        // the compacting arm alone, on the argument that `major_gc` runs only
        // from the moving young cycle and the moving collector rewrites every
        // root it is handed, so a conservative interior word cannot reach it.
        // `COMPACT_DROPPED_INTERIOR_ROOT` refuted that on the first H2
        // `MVStore` run: the compactor was dropping blocks an interior root
        // pointed into, and marking them would not have been enough anyway —
        // it would have SLID them, and the one word that still names the object
        // cannot be rewritten.
        //
        // So do not compact this cycle. The in-place sweep is precisely the
        // collector for an un-rewritable root set: it reclaims dead blocks
        // where they lie, retains the pinned ones, and coalesces the free list
        // afterwards, so the generation still shrinks and the fragmentation the
        // compactor exists to fix is still bounded. Compaction resumes on the
        // next cycle whose roots are all object bases.
        //
        // `CRATONVM_GC_NO_OLD_INTERIOR_PINS=1` keeps the pre-fix behaviour
        // (compact anyway, pin nothing) so the counters above stay meaningful
        // as a negative control.
        //
        // gengc-round2-core2 (2026-09-20): this block used to sit BETWEEN the
        // two root passes; it now sits after the single fused one. Nothing
        // between the loop and here reads `compact`, and the interior marks the
        // loop makes still imply this downgrade — see the fusion note on the
        // loop.
        let compact = if compact && !interior_pins.is_empty() && !no_interior_pins {
            COMPACT_DOWNGRADED_INTERIOR_ROOT.fetch_add(1, Ordering::Relaxed);
            let n = COMPACT_DOWNGRADE_REPORTS.fetch_add(1, Ordering::Relaxed);
            if n < 4 {
                tracing::warn!(
                    interior_roots = interior_pins.len(),
                    "old-gen major GC: {} root(s) are INTERIOR words of live old-gen objects, \
                     so this cycle reclaims IN PLACE instead of compacting — a slid object \
                     would leave those roots dangling and they cannot be rewritten. See \
                     old-sweep-liveness.md section 7.",
                    interior_pins.len(),
                );
            }
            false
        } else {
            compact
        };
        // gen r4w4/oldgen4 — a recoverable walk gap forbids compaction the
        // same way: the compactor would abandon over it anyway
        // (`COMPACT_WALK_GAP_HITS`, it cannot slide over bytes it did not
        // walk) and reclaim nothing; the in-place arm below can reclaim
        // everything the walk DID cover.
        let compact = if compact && gap_recovery {
            old_gen.note_walk_gap_compact_downgrade();
            false
        } else {
            compact
        };

        phase_timer.phase("roots");

        // Seed: young from-space references into old gen
        //
        // gen r4w5/oldcompact5: with a pinned compaction planned, the bases
        // the young walk's conservative fallback names are pins too (see
        // `OldPinnedCompact`).
        // gcd d4/n: a requested major over a gap-free old walk seeds young from
        // the true roots instead (`TrueRootYoung`, seeded before the BFS
        // below); every other collection keeps the legacy seed. (gcd d9/a:
        // the decision, and its fallbacks, moved above the root loop.)
        // gci i2/og: the P-C major's reached set is a grid-index bitset
        // (`TrueRootVisited`); the non-moving majors keep the hash set.
        // gci i3/og: unless `CRATONVM_GC_TRUE_ROOT_BATCHED_VERDICT` (opt-in)
        // gives them the bitset too (asked only for a non-P-C ask).
        // gci i4/og (`gci-i3og-true-root-major-residual-costs-FIXED-20261012` item 1):
        // the same "lean" asks also defer `promoted_by_dest` until the pin
        // snapshots below say a source-keyed row exists (a P-C ask has no
        // promotions, so the deferral is moot there). BV is the caller's one
        // read of the switch (`TrueRootAsk::hand_back_judged`), so the whole
        // major sees one answer.
        let true_root_lean = true_young_on
            && true_root_ask
                .as_ref()
                .is_some_and(|ask| ask.pre_copy || ask.hand_back_judged);
        // gci i8/pa: a P-C ask's re-keyed loader / mirror rows (`None`: the
        // registries, as before) and the promoted loaders a fallback roots.
        // Taken whether or not the seed runs: the moving cycle rooted no
        // moved loader for this call.
        let (rk_mirror_rows, rk_loader_rows, rk_fallback_loaders) = match true_root_ask.as_mut() {
            Some(ask) => (
                ask.mirror_rows.take(),
                ask.loader_rows.take(),
                std::mem::take(&mut ask.fallback_loaders),
            ),
            None => (None, None, Vec::new()),
        };
        let mut true_young = if true_young_on {
            Some(TrueRootYoung::build(
                young_from,
                young_skips,
                old_gen,
                std::mem::take(&mut promoted),
                true_root_lean,
                !true_root_lean,
            ))
        } else {
            None
        };
        // gci i8/pa: the trace's `class id -> loader` and mirror rows from
        // the re-keyed ones, screened as `build` screens the registry's (an
        // old loader or one in the post-swap young grid).
        if let Some(y) = true_young.as_mut() {
            if let Some(rows) = rk_loader_rows.as_ref() {
                let (lo, hi) = (y.lo, y.hi);
                y.loaders = Some(
                    rows.iter()
                        .filter(|&(_, &l)| old_gen.contains(l as *const u8) || (l >= lo && l < hi))
                        .map(|(&c, &l)| (c, l))
                        .collect(),
                );
            }
            y.mirror_rows = rk_mirror_rows.clone();
            // t1 G2: the parallel old mark marks the trace's old targets.
            if par_mark_workers >= 2 {
                y.defer_old = Some(Vec::new());
            }
        }
        if true_young.is_none() {
            let anomaly_pins = if pinned.is_some() {
                Some(&mut pin_bases)
            } else {
                None
            };
            // gcj j3/rf3 (ec47, the legacy seed,
            // `CRATONVM_GEN_LEGACY_SEED_DEFERS_RESURRECTED`, default ON, `=0`
            // kills; dormant with `CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS=0`):
            // the young phase's resurrection set seeds its old referents into
            // the resurrection round (`deferred_seeds`, marked there by
            // `seed_deferred_finalizer_seeds`), not into the strong closure.
            // The final mark is the same set; only the depth-2 set grows.
            let legacy_members: Vec<usize> = match fin_pass.as_deref_mut() {
                Some(pass) if legacy_defers => {
                    std::mem::take(&mut pass.young_resurrected)
                }
                _ => Vec::new(),
            };
            let mut legacy_deferred: Vec<usize> = Vec::new();
            let defer = (!legacy_members.is_empty())
                .then_some((legacy_members.as_slice(), &mut legacy_deferred));
            // gcf f2/c: through the shared start set when one was built
            // (`MarkAdmission`); the default is the walked grid, as before.
            if walked_starts.is_none() {
                Self::mark_young_to_old_refs_deferring(
                    young_from,
                    old_gen,
                    &walked_objects,
                    young_skips,
                    &mut worklist,
                    anomaly_pins,
                    defer,
                );
            } else {
                let admission = MarkAdmission {
                    walked: &walked_objects,
                    starts: walked_starts.as_ref(),
                };
                Self::mark_young_to_old_refs_deferring(
                    young_from,
                    &*old_gen,
                    &admission,
                    young_skips,
                    &mut worklist,
                    anomaly_pins,
                    defer,
                );
            }
            if !legacy_deferred.is_empty() {
                LEGACY_SEED_DEFERRED_REFERENTS
                    .fetch_add(legacy_deferred.len() as u64, Ordering::Relaxed);
                if watchref_dbg() {
                    eprintln!(
                        "[watchref] major: legacy young seed deferred {} old referent(s) of {} resurrected young object(s) to the resurrection round (total {})",
                        legacy_deferred.len(),
                        legacy_members.len(),
                        LEGACY_SEED_DEFERRED_REFERENTS.load(Ordering::Relaxed),
                    );
                }
                if let Some(pass) = fin_pass.as_deref_mut() {
                    pass.deferred_seeds.extend_from_slice(&legacy_deferred);
                }
            }
        }
        // gen r5w2/alloc6: whatever that pushed came from an unparseable young
        // stretch (a no-op when no compaction is planned: both are empty).
        pin_srcs.resize(pin_bases.len(), OLD_PIN_SRC_YOUNG_STRETCH);

        // `mark_young_to_old_refs` walks only real Java heap fields. Follow
        // the equivalent out-of-heap edges for all current young owners too;
        // young from-space is conservatively retained for this major cycle,
        // matching the ordinary cross-generation seed's contract. (gcd d4/n:
        // the true-root seed follows them only for the young owners it
        // reaches.)
        if true_young.is_none() {
            for overlay_ref in
                cratonvm_gc_core::external_roots::external_roots_for_matching_owners(&|owner_addr| {
                    young_from.contains(owner_addr as *mut u8)
                })
            {
                mark_and_push_old_gen(
                    overlay_ref.as_ptr(),
                    old_gen,
                    &walked_objects,
                    &mut worklist,
                    "external-overlay(young owner)",
                );
            }
            // gci i8/pa: under the loader re-key the moving cycle did not root
            // this pause's promoted loaders; the legacy seed's young walk
            // looks a young instance's loader up by its pre-copy row and
            // would miss a promoted one. Empty off the re-key.
            for &loader in &rk_fallback_loaders {
                mark_and_push_old_gen(
                    loader as *mut u8,
                    old_gen,
                    &walked_objects,
                    &mut worklist,
                    "loader_pin(promoted, re-key fallback)",
                );
            }
        }

        // BFS: transitively mark all reachable old-gen objects
        // HIB-CV-24: a live object keeps its class's defining ClassLoader alive
        // (instance→loader). Conservative — only ever marks MORE live, so it can
        // never free a still-referenced loader. Covers the old-gen instance →
        // old-gen loader case (young loaders are already live as major-GC roots).
        let loader_pin_on = cratonvm_types::loader_pin::loader_pinning_enabled();
        // gengc-round1-core (2026-09-20): hoist the two pin-registry lookups
        // out of the per-object loop.
        //
        // `mirror_pin::mirrors_for_loader` and `metadata_pin::roots_for_loader`
        // each take the registry `RwLock` and CLONE a `Vec` — and they were
        // being called once per BFS pop, inside the stop-the-world pause. The
        // `NON_EMPTY` latch only short-circuits the wholly-empty case, so on
        // any workload with a user-defined `ClassLoader` (H2, Hibernate — the
        // very workloads these pins exist for) that is two lock acquisitions
        // per marked old-gen object. `metadata_pin::snapshot`'s own doc names
        // this exact cost, the adjacent `loader_pin` check is already gated on
        // a hoisted `loader_pin_on`, and the young marker next door already
        // takes `metadata_pin::snapshot()` once per cycle
        // (`YoungMarkCtx::metadata_pins`).
        //
        // Sound because the marker is at a safepoint: nothing can publish a
        // pin while this loop runs, so one snapshot taken before it is exactly
        // equivalent to N lookups inside it. Both fall back to "no pins", the
        // common case, for free.
        // gci i8/pa: the re-keyed rows' owners when the ask carried them.
        let mirror_pin_owners: FxHashSet<usize> = match rk_mirror_rows.as_ref() {
            Some(rows) => rows.keys().copied().collect(),
            None => cratonvm_types::mirror_pin::pinned_owner_addrs()
                .into_iter()
                .collect(),
        };
        // gci i5/og: a P-C ask carries the rows re-keyed by post-copy owner
        // (`TrueRootAsk::metadata_rows`); taken whether or not the seed runs
        // (the moving cycle rooted no moved owner's value for it).
        let metadata_pins = match true_root_ask.as_mut().and_then(|ask| ask.metadata_rows.take()) {
            Some(rows) => Some(rows),
            None => cratonvm_types::metadata_pin::snapshot(),
        };
        // gcj j1/og (`CRATONVM_GC_OLD_MARK_LOADER_SNAPSHOT`, opt-in since j1c, `=1`
        // arms): the BFS's instance -> defining-loader edge answered from ONE
        // snapshot of this heap's `loader_pin` rows, taken here, instead of the
        // registry's read lock and hash probe once per marked old object (the
        // young copier's gci i10/lp cut, for the full-GC mark). Same answers:
        // `snapshot_where` applies the marker's row choice with the same
        // `is_own` test per class id, and the rows naming this heap cannot
        // change inside the pause (another VM's write adds or drops only a row
        // the test rejects; `mark_and_push_old_gen` screens a foreign address
        // either way). An empty or unpinned registry is an empty map: the
        // per-object arm would answer `None` for every id too. A P-C ask's
        // re-keyed rows (`rk_loader_rows`) still take precedence.
        let old_mark_loader_rows: Option<FxHashMap<u32, usize>> = if loader_pin_on
            && rk_loader_rows.is_none()
            // t1 G2: the parallel mark always takes the snapshot (same
            // answers, see above), so its workers never queue on the
            // registry's read lock once per marked object.
            && (old_mark_loader_snapshot_enabled() || par_mark_workers >= 2)
        {
            // gcj j2/x1: the arm's engagement (`[gcphase]` and the stats).
            old_gen.note_loader_snapshot_mark();
            phase_timer.count("mark-loader-snapshot", 1);
            Some(
                cratonvm_types::loader_pin::snapshot_where(|a| old_gen.contains(a as *const u8))
                    .unwrap_or_default(),
            )
        } else {
            None
        };
        // gcd d9/a (review of the legacy seed): it retains EVERY young object
        // for this major, so every young OWNER's `mirror_pin` and
        // `metadata_pin` row is live too. Their OLD values are deferred to the
        // owner by the root gatherer (`VmHeap::mirror_pin_deferrable`,
        // `metadata_pin_deferrable`: an old value is never a root), and the BFS
        // below looks rows up only for the OLD objects it marks, so an old
        // mirror or metadata value of a young user loader was marked by
        // nobody and swept while its loader lived. Keyed by the owner's
        // current address, which on this non-moving path is its address in
        // young from-space (a moving cycle's pre-copy keys do not match here;
        // see `gcd-d9a-moving-major-misses-side-table-rows-of-moved-owners-20260928`).
        // The true-root seed follows the rows of the young owners it reaches
        // (`TrueRootYoung::side_table_targets`).
        if true_young.is_none() {
            let young_owner = |a: usize| young_from.contains(a as *mut u8);
            for &owner in &mirror_pin_owners {
                if !young_owner(owner) {
                    continue;
                }
                // gci i8/pa: the re-keyed rows when the ask carried them.
                if let Some(mirrors) = pinned_mirrors_of(owner, rk_mirror_rows.as_ref()) {
                    for mirror_addr in mirrors {
                        mark_and_push_old_gen(
                            mirror_addr as *mut u8,
                            old_gen,
                            &walked_objects,
                            &mut worklist,
                            "mirror_pin(young owner)",
                        );
                    }
                }
            }
            if let Some(pins) = metadata_pins.as_ref() {
                for (&owner, values) in pins {
                    if !young_owner(owner) {
                        continue;
                    }
                    for &value in values {
                        mark_and_push_old_gen(
                            value as *mut u8,
                            old_gen,
                            &walked_objects,
                            &mut worklist,
                            "metadata_pin(young owner)",
                        );
                    }
                }
            }
        }
        // gen r4w4/oldgen4: the unwalked bytes are roots — every reference an
        // object behind the break could hold, and every side-table edge such
        // an object owns. Seeded before the BFS so what they reach is traced
        // like anything else.
        if gap_recovery {
            let _ = seed_old_gen_walk_gaps(
                old_gen,
                &walked_objects,
                &walk_gaps,
                &mirror_pin_owners,
                metadata_pins.as_ref(),
                loader_pin_on,
                // gci i8/pa: the re-keyed rows (`None`: the registries).
                rk_mirror_rows.as_ref(),
                rk_loader_rows.as_ref(),
                &mut worklist,
            );
        }
        // gc-common w36-d — FINALIZER RESURRECTION, as a second round of the
        // same BFS. When the root closure's worklist first runs dry, "unmarked"
        // means "unreachable", so that is the one moment the dead finalizables
        // can be told apart from the live ones: `OldGenFinalizerPass::seed`
        // marks every unmarked old-gen candidate, and this loop then traces
        // their subgraphs through exactly the body below (overlays, loader /
        // mirror / metadata pins included). Every candidate is seeded before
        // any is traced, so two finalizables that reach only each other are
        // BOTH reported (`reference.rs::mutually_reachable_finalizers_both_enqueued`).
        // Until w36-d a dead OLD finalizable was simply freed in place and its
        // `finalize()` never ran (`OldGenFinalizeProbe` 0/64). Without a pass,
        // or with no candidate in old gen, this is the plain drain it was.
        // gcd d4/n: the true-root young seed -- the root slice (precise and
        // conservative words, resolved against the young grid), every
        // registered finalizable (as the legacy seed keeps every young one),
        // and an unparseable stretch's whole content; the rest is the
        // fixed-point `round` below, whenever the worklist runs dry.
        //
        // H2-CID0 (2026-08-02): every address the reference processor holds
        // (the true-root seed's watched words, the sweep's survival proofs,
        // the compacting arm's drop flags). gce e1/c: one snapshot per call;
        // nothing publishes to the set during the pause.
        phase_timer.phase("young-seed");
        // gci i3/og: the caller's snapshot when it carried one (P-C).
        let watched = match true_root_ask
            .as_mut()
            .and_then(|ask| ask.watched_snapshot.take())
        {
            Some(carried) => carried,
            // gcj j3/rf3: with the seed-only row (`CRATONVM_GC_STRING_TABLE_SEED_ONLY_YOUNG`).
            None => cratonvm_gc_core::gc_quiescence::watched_major_snapshot_for(old_gen.watched_set_key()),
        };
        // gci i1/og (h4pc review item 3): a moving cycle's ask (P-C) holds
        // PRE-copy watched addresses that can never lie in its post-swap
        // grid (see `TrueRootAsk::pre_copy`): neither seeded nor judged.
        let watched_pre_copy = true_root_ask.as_ref().is_some_and(|ask| ask.pre_copy);
        // gch h4/wr: read once per true-root major, only when the seed runs
        // and something is watched.
        let watched_dead = true_young.is_some()
            && !watched_pre_copy
            && watched.is_some()
            && full_gc_true_roots_watched_dead_enabled();
        // t1 G2: the true-root young seed + drain below, timed under
        // `CRATONVM_DBG_GCPHASE` (`mark-young-seed` part): serial on both arms.
        let t_young_seed = phase_timer.is_on().then(std::time::Instant::now);
        if let Some(y) = true_young.as_mut() {
            // gci i3/og (`CRATONVM_GC_TRUE_ROOT_BATCHED_VERDICT`, opt-in): with
            // no source-keyed table rows at all (no `mirror_pin` owner, no
            // `metadata_pin` row: the snapshots above, fixed for the pause),
            // `source_keyed_targets` yields nothing for any source, so the
            // destination lookup `seed_from_bfs` and `round` make for EVERY
            // marked old object finds nothing to follow. An empty map makes
            // `seed_from_bfs` return at its `is_empty` test and `round` miss
            // at once: the same marks, one hash probe per marked old object
            // fewer.
            // gci i4/og (`gci-i3og-true-root-major-residual-costs-FIXED-20261012`
            // item 1): a lean ask's `build` no longer builds the map at all;
            // it is built here, from `promoted`, only when a source-keyed row
            // exists. Nothing reads it before this point.
            if true_root_lean
                && !y.promoted.is_empty()
                && !(mirror_pin_owners.is_empty()
                    && metadata_pins.as_ref().is_none_or(|m| m.is_empty()))
            {
                y.promoted_by_dest = promoted_by_dest_of(&y.promoted);
            }
            let ctx = TrueRootCtx {
                old_gen: &*old_gen,
                walked: &walked_objects,
                mirror_pin_owners: &mirror_pin_owners,
                metadata_pins: metadata_pins.as_ref(),
            };
            // gcd d9/a: the caller's slice only; its appended promotion
            // destinations are resolved through their sources instead, and a
            // source-keyed pin value is followed for a destination the mark
            // reaches (`round`).
            for root in roots[..promo_tail].iter() {
                y.seed_word(root.as_ptr() as usize);
            }
            // gcj j2/rf2 (ec47, `CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS`,
            // default ON, `=0` kills): NOT seeded here. A dead young
            // finalizable seeded with the true roots made its OLD children
            // strongly marked, so the reference processor kept their weak
            // references through the pause that found it dead (HotSpot clears
            // them before `finalize()`). The candidates are seeded in the
            // resurrection round below instead: a live one is reached by the
            // trace anyway (seeding is idempotent), and what only a dead one
            // reaches is marked by that round and recorded in `closure`. The
            // final mark state is the same set either way.
            if !defer_fin_seeds {
                if let Some(pass) = fin_pass.as_ref() {
                    for &candidate in pass.candidates {
                        y.seed_word(candidate);
                    }
                }
            }
            // gcd d5/r (review of d4/n): and every address a WEAK table hands
            // back after this pause -- the watched set the VM published for
            // this collection: every address the reference processor holds
            // (referents, `Reference` objects, queues) and every JNI weak
            // global's referent. The young phase keeps such an object when a
            // dead old holder's card names it, and the post-collection
            // restore then hands it back through its weak reference (it
            // "survived": `watched_pre_gc_addr_survived`), and an enqueue
            // writes into its queue. Left out, its old referents -- the
            // queue included -- were freed under it: a use-after-free on the
            // default path. Over-retention relative to HotSpot for these
            // objects only, exactly as the legacy seed.
            // gce e1/c: the one snapshot this call takes (below: the sweep's
            // survival proofs) -- it used to be cloned twice per true-root
            // major, a SipHash set of every tracked reference address.
            // gch h4/wr (`CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD`, default ON since gci i1/og and jr20 wave 9):
            // NOT seeded. The trace runs from the true roots alone, and after
            // its fixed point every watched young survivor it did not reach is
            // judged DEAD instead (`watched_left_out_by_trace`, below): the
            // caller removes its identity entry and the heap answers "did not
            // survive" for it, so no weak table hands it back and its old
            // referents may be freed. A watched promotion needs nothing: an
            // unreached destination is freed and its source leaves the map
            // (`TrueRootSweepOut::dead_sources`), as for any promotion.
            if let Some(w) = watched.as_ref().filter(|_| !watched_dead && !watched_pre_copy) {
                for &addr in w {
                    y.seed_word(addr);
                }
            }
            // gcd d9/a: a planned pinned compaction pins what a stretch word
            // names, as the legacy seed's conservative fallback does.
            let stretch_pins = if pinned.is_some() {
                Some(&mut pin_bases)
            } else {
                None
            };
            y.seed_stretches(&ctx, &mut worklist, stretch_pins);
            pin_srcs.resize(pin_bases.len(), OLD_PIN_SRC_YOUNG_STRETCH);
            y.drain(&ctx, &mut worklist);
        }
        let young_seed_us = t_young_seed.map_or(0, |t| t.elapsed().as_micros());
        // gcj j2/rf2 (ec47): a deferred seed must be marked by the round, so a
        // non-empty `deferred_seeds` makes the round due on its own.
        let mut resurrection_due = fin_pass
            .as_ref()
            .is_some_and(|p| !p.candidates.is_empty() || !p.deferred_seeds.is_empty());
        let mut resurrection_draining = false;
        // gcj j4/rf: see where the round starts below.
        let fin_round_records_young = fin_pass.is_some()
            && true_young.is_some()
            && true;
        let mut fin_round_dest_to_src: FxHashMap<usize, usize> = FxHashMap::default();
        // gce e1/c (`gcd-d9a-true-root-seed-skips-young-targets-of-close-live-set-rescues-20260928`):
        // under the true-root seed, the in-place sweep's `close_live_set` used
        // to be the first to see a mark push-site gap, AFTER the young fixed
        // point had finished -- so a rescued old object's young targets, and
        // the old objects only they reach, were never traced and were freed
        // under it. The closure now also runs HERE, when the strong closure
        // (old BFS + young `round`) runs dry and before the finalizer round;
        // whatever it rescues goes back through the mark body (overlays,
        // pins, `pending_old`), so the next `round` traces its young targets.
        // Repeated until it rescues nothing (one O(live) pass on a healthy
        // cycle, requested majors only). The sweep's own closure still runs,
        // and still counts what the finalizer round's subgraphs miss.
        let mut pre_rescued = 0usize;
        let mut pre_close_done = true_young.is_none();
        // The last in-mark closure's escape verdict: when nothing was marked
        // after it (no finalizer round), the sweep's closure would repeat it
        // exactly, so the sweep reuses it instead of a second O(live) pass.
        let mut pre_close_escaped: Option<bool> = None;
        let mut pre_rescue_buf: Vec<usize> = Vec::new();
        // gcf f1/c (`gcd-d9a-true-root-seed-skips-young-targets-of-close-live-set-rescues`,
        // the residual): the same closure once more AFTER the finalizer
        // round's drain, under the true-root seed only. A block it rescues
        // now is named by a marked object but was not reached by the strong
        // closure (the in-mark closure above found nothing more), so it is in
        // the resurrected subgraph: it goes back through the mark body while
        // `resurrection_draining` is still set, which records it in the
        // depth-2 set, and the next `round` traces its young targets. Before
        // this, such a block was rescued only by the sweep's closure, after
        // the young fixed point, and the old objects only its young targets
        // reach were freed under it.
        let mut post_close_done = true_young.is_none();
        let mut post_close_escaped: Option<bool> = None;
        // gcg g3/e (the g2/e report's hunk B): `mark` split into the in-mark
        // closures and the true-root young rounds, printed as `part=` lines
        // after `mark` under `CRATONVM_DBG_GCPHASE` (old9's gate: is the BFS
        // itself what dominates?). Off: no clock read.
        let timing = phase_timer.is_on();
        let mut mark_close_us: u128 = 0;
        let mut mark_round_us: u128 = 0;
        // gch h4/pc: `close_live_set` calls this mark made (a `count=` line
        // under `CRATONVM_DBG_GCPHASE`, next to `mark-in-closures`).
        let mut mark_closures: usize = 0;
        // gch h4/pc (`gch-h3mj-proposal-true-root-rounds-rescan-every-marked-old-object-DONE-20261015`):
        // `CRATONVM_GC_TRUE_ROOT_ROUNDS_IN_BFS` (default ON, `=0` kills) -- each pop below
        // hands its young targets to the true-root trace itself instead of
        // queueing the object for `round`'s rescan (see
        // `true_root_rounds_in_bfs_enabled`). Read only on a true-root major.
        // `bfs_young` is the per-pop scratch; `bfs_young_loaders` holds the
        // `class id -> loader` rows whose loader is YOUNG, the only rows
        // `round`'s loader lookup keeps for an old owner (empty on most heaps).
        let rounds_in_bfs = true_young.is_some() && true_root_rounds_in_bfs_enabled();
        let (bfs_lo, bfs_hi) = match true_young.as_ref() {
            Some(y) if rounds_in_bfs => (y.lo, y.hi),
            _ => (0, 0),
        };
        let bfs_young_loaders: FxHashMap<u32, usize> = match true_young.as_ref() {
            Some(y) if rounds_in_bfs => y.loaders.as_ref().map_or_else(FxHashMap::default, |m| {
                m.iter()
                    .filter(|&(_, &l)| l >= bfs_lo && l < bfs_hi)
                    .map(|(&c, &l)| (c, l))
                    .collect()
            }),
            _ => FxHashMap::default(),
        };
        let mut bfs_young: Vec<usize> = Vec::new();
        // gch h4/pc: `CRATONVM_GC_TRUE_ROOT_SKIP_IDLE_POST_CLOSE` (default ON):
        // the objects popped in the finalizer round. None popped means the
        // round marked nothing (every mark of `seed` and of the round's own
        // drain is pushed, so it is popped), so the post-close would see the
        // last pre-close's mark state: skipped, its verdict reused.
        let skip_idle_post_close =
            resurrection_due && !post_close_done && true_root_skip_idle_post_close_enabled();
        let mut resurrection_pops: usize = 0;
        let mut post_close_skipped = false;
        // gci i28/cl (`CRATONVM_GC_OLD_POST_CLOSE_INCREMENTAL`, opt-in; see
        // `old_post_close_incremental_enabled`): the post-close visits only the
        // objects popped since the last closure returned (`post_seeds`), not
        // every walked header. Armed only with the idle skip (whose `live_set_closed`
        // keeps `OldGenFinalizerPass::seed` from marking without a push) and
        // only on a true-root major with a finalizer round. `post_escaped_acc`
        // is the escape verdict of every incremental call so far, ORed with
        // the last pre-close's (which covered every other marked object).
        let post_close_incremental = skip_idle_post_close && old_post_close_incremental_enabled();
        let mut post_seeds: Vec<usize> = Vec::new();
        let mut post_escaped_acc = false;
        let mut post_close_incremental_calls: usize = 0;
        // gcj j2/og2 (`CRATONVM_GC_OLD_MARK_PREFETCH`, opt-in): pops go through
        // a small prefetching ring (`old_gen/mark_prefetch.rs`); `None` only
        // when the worklist AND the ring are empty. Off: `worklist.pop()`.
        let mut mark_prefetch = crate::old_gen::mark_prefetch::MarkPrefetchFifo::new(
            cratonvm_types::flags::runtime_flag_on(
                crate::old_gen::mark_prefetch::MARK_PREFETCH_FLAG,
            ),
        );
        // t1 G2 (`CRATONVM_GC_OLD_PAR_MARK`): the parallel drains' engagement
        // (`[gcphase]` counts after `mark`).
        let mut par_drains: usize = 0;
        let mut par_scanned: usize = 0;
        let mut par_max_worker: usize = 0;
        // t1 w7 G2: the CPU accounting of the mark (`CRATONVM_DBG_GCPHASE`
        // only): the collecting thread's own CPU over the whole mark loop
        // (both arms), and the drains' worker sums.
        let mark_cpu0 = if timing {
            cratonvm_gc_core::thread_cpu::current_thread_cpu_ns()
        } else {
            None
        };
        let mut par_diag = crate::old_gen::par_mark::DrainCounts::default();
        let mut par_us: u128 = 0;
        let mut serial_pops: usize = 0;
        loop {
            // t1 G2 (`CRATONVM_GC_OLD_PAR_MARK`, default ON since 2026-10-27; step 4 of
            // `gcj-j1og-proposal-parallel-full-gc-adjust-and-mark`): once the
            // worklist is wide, drain it -- and everything it reaches -- on
            // `par_mark_workers` workers (`old_gen::par_mark::drain`), each
            // running this loop's per-pop body (`par_mark_scan_one`). The
            // young targets the serial body seeds at each pop are recorded per
            // worker and seeded here after the join; seeding is idempotent and
            // the young trace runs at the next empty point either way, so the
            // phases below see the same marks. Never inside the finalizer
            // resurrection round (its depth-2 set records pops), so the
            // reference-processing order and the round's subgraph are the
            // serial ones. The mark claim is a CAS (`mark_and_push_old_gen_in`),
            // so the mark set is the serial closure's whatever the interleaving.
            // A huge reference array on top goes wide at once: its slot
            // ranges are split across the workers (`par_mark_array_split_len`).
            // The true-root trace's deferred old targets (`TrueRootYoung::
            // defer_old`) are marked by the drain when there are many, and
            // flushed through the serial push site here when there are few.
            let deferred_n = true_young
                .as_ref()
                .and_then(|y| y.defer_old.as_ref())
                .map_or(0, Vec::len);
            if deferred_n > 0
                && deferred_n < crate::old_gen::par_mark::PAR_MARK_MIN_WORKLIST
                && worklist.len() < crate::old_gen::par_mark::PAR_MARK_MIN_WORKLIST
            {
                if let Some(d) = true_young.as_mut().and_then(|y| y.defer_old.as_mut()) {
                    for a in d.drain(..) {
                        mark_and_push_old_gen_in(
                            a as *mut u8,
                            old_gen,
                            &walked_objects,
                            &mut worklist,
                            "true-root young edge",
                        );
                    }
                }
                continue;
            }
            if par_mark_workers >= 2
                && !resurrection_draining
                && (deferred_n > 0
                    || worklist.len() >= crate::old_gen::par_mark::PAR_MARK_MIN_WORKLIST
                    || worklist
                        .last()
                        .is_some_and(|&p| par_mark_array_split_len(p, old_gen) > 0))
            {
                let t = timing.then(std::time::Instant::now);
                mark_prefetch.spill_into(&mut worklist);
                let seed = std::mem::take(&mut worklist);
                let candidates: Vec<usize> = true_young
                    .as_mut()
                    .and_then(|y| y.defer_old.as_mut())
                    .map(std::mem::take)
                    .unwrap_or_default();
                let (outs, counts) = {
                    let c = ParMarkCtx {
                        old_gen: &*old_gen,
                        walked: walked_objects.as_slice(),
                        starts: walked_starts.as_ref(),
                        rounds_in_bfs,
                        track_young: true_young.is_some(),
                        bfs_lo,
                        bfs_hi,
                        bfs_young_loaders: &bfs_young_loaders,
                        loader_pin_on,
                        rk_loader_rows: rk_loader_rows.as_ref(),
                        old_mark_loader_rows: old_mark_loader_rows.as_ref(),
                        mirror_pin_owners: &mirror_pin_owners,
                        rk_mirror_rows: rk_mirror_rows.as_ref(),
                        metadata_pins: metadata_pins.as_ref(),
                        promoted_by_dest: true_young
                            .as_ref()
                            .map(|y| &y.promoted_by_dest)
                            .filter(|m| !m.is_empty()),
                        candidates: &candidates,
                        // t1 w8 J1OG: `CRATONVM_GC_OLD_PAR_MARK_PREFETCH`
                        // (default ON, `=0` kills; per drain, for the A/B).
                        prefetch: true,
                    };
                    // SAFETY: the workers read only pause-frozen state through
                    // `c` (the walk, the start set, the snapshots) and the
                    // `OldGen` methods `contains` / `contains_range` /
                    // `in_extent` / `note_mark_gap_fact` (plain fields, the
                    // commit bitmap, an atomic); see `AssertSync`. Nothing
                    // mutates any of it until `drain` has joined them.
                    let shared = unsafe { crate::old_gen::par_mark::AssertSync::new(c) };
                    crate::old_gen::par_mark::drain(
                        seed,
                        crate::old_gen::par_mark::candidate_ranges(candidates.len()),
                        par_mark_workers,
                        timing,
                        ParMarkOut::default,
                        |p, stack, out: &mut ParMarkOut| {
                            par_mark_scan_one(p, shared.get(), stack, out)
                        },
                        |p, s, e, stack, out: &mut ParMarkOut| {
                            par_mark_scan_chunk(p, s, e, shared.get(), stack, out)                        },
                    )
                };
                let mut loader_lookups: u64 = 0;
                for out in outs {
                    loader_lookups += out.loader_lookups;
                    if let Some(y) = true_young.as_mut() {
                        if !out.young_recs.is_empty() {
                            let ctx = TrueRootCtx {
                                old_gen: &*old_gen,
                                walked: &walked_objects,
                                mirror_pin_owners: &mirror_pin_owners,
                                metadata_pins: metadata_pins.as_ref(),
                            };
                            let recs = &out.young_recs;
                            let mut i = 0usize;
                            while i + 1 < recs.len() {
                                let obj = recs[i];
                                let n = recs[i + 1];
                                y.seed_from_bfs(obj, &recs[i + 2..i + 2 + n], &ctx, &mut worklist);
                                i += 2 + n;
                            }
                        }
                        y.pending_old.extend_from_slice(&out.pending_old);
                    }
                }
                if loader_lookups > 0 {
                    old_gen.note_loader_snapshot_lookups(loader_lookups);
                }
                old_gen.note_par_mark_drain();
                par_drains += 1;
                par_scanned += counts.scanned;
                par_max_worker = par_max_worker.max(counts.max_worker);
                par_diag.cpu_ns += counts.cpu_ns;
                par_diag.wait_ns += counts.wait_ns;
                par_diag.locks += counts.locks;
                par_diag.publishes += counts.publishes;
                par_diag.wakes += counts.wakes;
                if let Some(t) = t {
                    par_us += t.elapsed().as_micros();
                }
                continue;
            }
            let Some(obj_ptr) = mark_prefetch.pop(&mut worklist) else {
                // gcd d4/n: the young objects the old mark newly reached.
                if let Some(y) = true_young.as_mut() {
                    let ctx = TrueRootCtx {
                        old_gen: &*old_gen,
                        walked: &walked_objects,
                        mirror_pin_owners: &mirror_pin_owners,
                        metadata_pins: metadata_pins.as_ref(),
                    };
                    let t = timing.then(std::time::Instant::now);
                    y.round(&ctx, &mut worklist);
                    if let Some(t) = t {
                        mark_round_us += t.elapsed().as_micros();
                    }
                    // t1 G2: deferred old targets are work too.
                    if !worklist.is_empty() || y.defer_old.as_ref().is_some_and(|d| !d.is_empty()) {
                        continue;
                    }
                }
                if !pre_close_done && !resurrection_draining {
                    pre_rescue_buf.clear();
                    let t = timing.then(std::time::Instant::now);
                    // gcg g3/e: through the collection's shared start set
                    // when `CRATONVM_GC_OLD_SHARED_STARTS` built one.
                    let (n, escaped) = match closure_starts {
                        Some(s) => old_gen.close_live_set_collecting_shared(
                            &walked_objects,
                            &walk_gaps,
                            &mut pre_rescue_buf,
                            s,
                        ),
                        None => old_gen.close_live_set_collecting(
                            &walked_objects,
                            &walk_gaps,
                            &mut pre_rescue_buf,
                        ),
                    };
                    if let Some(t) = t {
                        mark_close_us += t.elapsed().as_micros();
                    }
                    mark_closures += 1;
                    if n == 0 {
                        pre_close_done = true;
                        pre_close_escaped = Some(escaped);
                    } else {
                        pre_rescued += n;
                        // Already MARKED by the closure: pushed as a marked
                        // object is, so the pop below runs the whole body.
                        worklist.extend(pre_rescue_buf.iter().map(|&a| a as *mut u8));
                        continue;
                    }
                }
                // gch h4/pc: an idle finalizer round (nothing popped in it)
                // left the mark state of the last pre-close, whose `n == 0`
                // verdict is this closure's answer too.
                if let Some(escaped) = idle_post_close_verdict(
                    skip_idle_post_close,
                    resurrection_draining,
                    post_close_done,
                    resurrection_pops,
                    pre_close_escaped,
                ) {
                    post_close_done = true;
                    post_close_escaped = Some(escaped);
                    post_close_skipped = true;
                }
                if resurrection_draining && !post_close_done {
                    pre_rescue_buf.clear();
                    let t = timing.then(std::time::Instant::now);
                    // gci i28/cl: only the objects popped since the last
                    // closure returned (the pre-close, then each post-close
                    // that rescued), when armed and the pre-close has a verdict.
                    let incremental_pre = pre_close_escaped.filter(|_| post_close_incremental);
                    let (n, escaped) = match (incremental_pre, closure_starts) {
                        (Some(pre_escaped), starts) => {
                            let (n, escaped) = old_gen.close_live_set_from(
                                &walked_objects,
                                &walk_gaps,
                                &post_seeds,
                                &mut pre_rescue_buf,
                                starts,
                            );
                            post_seeds.clear();
                            post_close_incremental_calls += 1;
                            post_escaped_acc |= escaped;
                            (n, pre_escaped || post_escaped_acc)
                        }
                        (None, Some(s)) => old_gen.close_live_set_collecting_shared(
                            &walked_objects,
                            &walk_gaps,
                            &mut pre_rescue_buf,
                            s,
                        ),
                        (None, None) => old_gen.close_live_set_collecting(
                            &walked_objects,
                            &walk_gaps,
                            &mut pre_rescue_buf,
                        ),
                    };
                    if let Some(t) = t {
                        mark_close_us += t.elapsed().as_micros();
                    }
                    mark_closures += 1;
                    if n == 0 {
                        post_close_done = true;
                        post_close_escaped = Some(escaped);
                    } else {
                        pre_rescued += n;
                        worklist.extend(pre_rescue_buf.iter().map(|&a| a as *mut u8));
                        continue;
                    }
                }
                if !resurrection_due {
                    break;
                }
                resurrection_due = false;
                resurrection_draining = true;
                // t1 G2: the finalizer round marks serially, in pop order;
                // nothing is deferred (the loop top flushed the last batch).
                if let Some(y) = true_young.as_mut() {
                    debug_assert!(y.defer_old.as_ref().is_none_or(|d| d.is_empty()));
                    y.defer_old = None;
                }
                // gcj j4/rf (`gcj-j1rf-gen-reference-and-finalizer-review-residuals`
                // item 1, `CRATONVM_GEN_FIN_ROUND_RECORDS_YOUNG`, default ON,
                // `=0` kills): the round's depth-2 set had only the OLD objects
                // it popped. The young objects it reaches (the true-root
                // trace's grid) are recorded from here on, and a popped
                // promotion destination is recorded as its young SOURCE, the
                // pre-collection address the processor's rows hold.
                if fin_round_records_young {
                    if let Some(y) = true_young.as_mut() {
                        y.reached_in_round = Some(Vec::new());
                        if !y.promoted.is_empty() {
                            fin_round_dest_to_src = y
                                .promoted
                                .iter()
                                .map(|&(src, dest, _)| (dest, src))
                                .collect();
                        }
                    }
                }
                // gch h4/pc (`CRATONVM_GC_TRUE_ROOT_SKIP_IDLE_POST_CLOSE`): the
                // pre-close just closed this exact mark state (`n == 0`), so
                // the seed's own closure is skipped too.
                let live_set_closed = skip_idle_post_close && pre_close_escaped.is_some();
                if let Some(pass) = fin_pass.as_deref_mut() {
                    pass.seed(
                        old_gen,
                        &walked_objects,
                        &walk_gaps,
                        gap_recovery,
                        &mut worklist,
                        live_set_closed,
                    );
                    // gcj j2/rf2 (ec47): the finalizer seeds the strong closure
                    // left out, AFTER `seed` (an old candidate only a dead young
                    // finalizable reaches is unmarked when `seed` judges it, so
                    // both are finalized at this pause, as HotSpot's keep-alive
                    // phase enqueues both). Everything they mark is pushed and
                    // popped in this round, so it lands in `closure`.
                    let deferred = std::mem::take(&mut pass.deferred_seeds);
                    let candidates: &[usize] = if defer_fin_seeds {
                        pass.candidates
                    } else {
                        &[]
                    };
                    seed_deferred_finalizer_seeds(
                        &deferred,
                        candidates,
                        true_young.as_mut(),
                        &TrueRootCtx {
                            old_gen: &*old_gen,
                            walked: &walked_objects,
                            mirror_pin_owners: &mirror_pin_owners,
                            metadata_pins: metadata_pins.as_ref(),
                        },
                        &mut worklist,
                    );
                }
                continue;
            };
            serial_pops += 1;
            if resurrection_draining {
                if let Some(pass) = fin_pass.as_deref_mut() {
                    // Popped in the resurrection round, so marked by it and
                    // NOT by the strong closure: the depth-2 set
                    // (`VmHeap::take_resurrection_closure`).
                    // gcj j4/rf: a promotion destination as its source.
                    let popped = obj_ptr as usize;
                    pass.closure.push(
                        fin_round_dest_to_src
                            .get(&popped)
                            .copied()
                            .unwrap_or(popped),
                    );
                }
                resurrection_pops += 1;
                // gci i28/cl: every mark since the last closure was pushed
                // (each push site marks before it pushes; `seed`'s own closure
                // is skipped when armed), so it is popped here.
                if post_close_incremental {
                    post_seeds.push(obj_ptr as usize);
                }
            }
            // gch h4/pc: under `CRATONVM_GC_TRUE_ROOT_ROUNDS_IN_BFS` the scan
            // below collects this object's young slot targets (`bfs_young`),
            // and so do the side-table lookups after it; they are seeded at
            // the end of this pop. `false`: rejected unscanned (see
            // `scan_object_for_old_refs_split_in`), so `round` takes it.
            let mut bfs_scanned = true;
            if rounds_in_bfs {
                bfs_young.clear();
                bfs_scanned = if walked_starts.is_none() {
                    Self::scan_object_for_old_refs_split_in(
                        obj_ptr,
                        &*old_gen,
                        walked_objects.as_slice(),
                        &mut worklist,
                        Some((bfs_lo, bfs_hi, &mut bfs_young)),
                    )
                } else {
                    let admission = MarkAdmission {
                        walked: &walked_objects,
                        starts: walked_starts.as_ref(),
                    };
                    Self::scan_object_for_old_refs_split_in(
                        obj_ptr,
                        &*old_gen,
                        &admission,
                        &mut worklist,
                        Some((bfs_lo, bfs_hi, &mut bfs_young)),
                    )
                };
            } else {
                // gcd d4/n: its young targets are seeded at the next `round`.
                if let Some(y) = true_young.as_mut() {
                    y.pending_old.push(obj_ptr as usize);
                }
                // gcf f2/c: through the shared start set when one was built
                // (`MarkAdmission`); the default is the walked grid, as before.
                if walked_starts.is_none() {
                    Self::scan_object_for_old_refs(
                        obj_ptr,
                        old_gen,
                        &walked_objects,
                        &mut worklist,
                    );
                } else {
                    let admission = MarkAdmission {
                        walked: &walked_objects,
                        starts: walked_starts.as_ref(),
                    };
                    Self::scan_object_for_old_refs_in(
                        obj_ptr,
                        &*old_gen,
                        &admission,
                        &mut worklist,
                    );
                }
            }
            // Same owner→overlay propagation as the young non-moving marker
            // above. Major GC also uses stable pre-compaction addresses, so it
            // can reclaim an unreachable old collection and its side-table
            // graph together instead of treating every entry as a global root.
            // Owner identity: the index is keyed by ADDRESS, so a recycled
            // block hands this BFS the previous tenant's overlay refs. The
            // header here is already validated (this object is marked), so
            // pass its class id and let the provider reject a stale entry.
            // SAFETY: `obj_ptr` is a marked old-gen object with a valid header.
            let owner_class = Some(
                unsafe { &*(obj_ptr as *const ObjectHeader) }
                    .class_id
                    .as_u32(),
            );
            // `with_...` rather than `external_roots_for_owner`: this runs once
            // per marked object and the returning form allocates a `Vec` for
            // every one that owns anything. The scratch buffer behind this is
            // per-thread and lives for the whole cycle.
            cratonvm_gc_core::external_roots::with_external_roots_for_owner(
                obj_ptr as usize,
                owner_class,
                |overlay_refs| {
                    for overlay_ref in overlay_refs {
                        // gch h4/pc: a young overlay value is the trace's (the
                        // old mark returns at once for it).
                        if rounds_in_bfs {
                            let a = overlay_ref.as_ptr() as usize;
                            if a >= bfs_lo && a < bfs_hi {
                                bfs_young.push(a);
                            }
                        }
                        mark_and_push_old_gen(
                            overlay_ref.as_ptr(),
                            old_gen,
                            &walked_objects,
                            &mut worklist,
                            "external-overlay(BFS owner)",
                        );
                    }
                },
            );
            // gch h4/pc: the class's YOUNG defining loader (`round` asks the
            // whole `loaders` map and keeps only a young answer).
            if rounds_in_bfs && !bfs_young_loaders.is_empty() {
                if let Some(&loader) = owner_class.and_then(|c| bfs_young_loaders.get(&c)) {
                    bfs_young.push(loader);
                }
            }
            if loader_pin_on {
                // SAFETY: `obj_ptr` is a marked old-gen object with a valid header.
                let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                // gc-common w36-d: the row inside THIS old generation when VMs
                // share the class id (`common-w4b-loader-pin-collision-...`);
                // a young loader needs no mark here (young is a major root).
                // gci i8/pa: from the re-keyed rows when the ask carried them
                // (a loader this pause promoted is found at its destination).
                let loader_addr = match (rk_loader_rows.as_ref(), old_mark_loader_rows.as_ref()) {
                    (Some(rows), _) => rows
                        .get(&header.class_id.as_u32())
                        .copied()
                        .filter(|&a| old_gen.contains(a as *const u8)),
                    // gcj j1/og: the pause's snapshot (see `old_mark_loader_rows`).
                    (None, Some(rows)) => {
                        old_gen.note_loader_snapshot_lookup();
                        rows.get(&header.class_id.as_u32()).copied()
                    }
                    (None, None) => cratonvm_types::loader_pin::loader_pin_addr_where(
                        header.class_id.as_u32(),
                        |a| old_gen.contains(a as *const u8),
                    ),
                };
                if let Some(loader_addr) = loader_addr {
                    mark_and_push_old_gen(
                        loader_addr as *mut u8,
                        old_gen,
                        &walked_objects,
                        &mut worklist,
                        "loader_pin",
                    );
                }
            }
            // Class-mirror liveness pin (mirror_pin, companion to loader_pin
            // above — see `vm::memory::roots` step 6 and
            // `cratonvm_types::mirror_pin`). Old gen doesn't move during this
            // BFS (compaction happens after), so `obj_ptr` is a stable key.
            // If this marked object IS itself a user-defined `ClassLoader`
            // that defined mirror-having classes, mark those mirrors alive
            // too — same "conservative, only ever marks MORE live" property
            // as the loader_pin check above. Same young-gen exemption as
            // loader_pin: a still-young mirror is already live as a major-GC
            // root.
            //
            // The owner-address set is the gate: `mirrors_for_loader` is keyed
            // by owner, so an object that owns nothing can be rejected without
            // touching the registry at all. Only a genuine pinned-loader hit
            // pays the lock, and there are a handful of those per process.
            if mirror_pin_owners.contains(&(obj_ptr as usize)) {
                // gci i8/pa: the re-keyed rows when the ask carried them.
                if let Some(mirror_addrs) =
                    pinned_mirrors_of(obj_ptr as usize, rk_mirror_rows.as_ref())
                {
                    for mirror_addr in mirror_addrs {
                        if rounds_in_bfs && mirror_addr >= bfs_lo && mirror_addr < bfs_hi {
                            bfs_young.push(mirror_addr);
                        }
                        mark_and_push_old_gen(
                            mirror_addr as *mut u8,
                            old_gen,
                            &walked_objects,
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
                    if rounds_in_bfs && metadata_addr >= bfs_lo && metadata_addr < bfs_hi {
                        bfs_young.push(metadata_addr);
                    }
                    mark_and_push_old_gen(
                        metadata_addr as *mut u8,
                        old_gen,
                        &walked_objects,
                        &mut worklist,
                        "metadata_pin",
                    );
                }
            }
            // gch h4/pc: hand this pop's young targets to the trace (and the
            // source-keyed rows of a promotion destination), or -- for an
            // object the scan rejected -- queue it for `round`, as before.
            if rounds_in_bfs {
                if let Some(y) = true_young.as_mut() {
                    if bfs_scanned {
                        let ctx = TrueRootCtx {
                            old_gen: &*old_gen,
                            walked: &walked_objects,
                            mirror_pin_owners: &mirror_pin_owners,
                            metadata_pins: metadata_pins.as_ref(),
                        };
                        y.seed_from_bfs(obj_ptr as usize, &bfs_young, &ctx, &mut worklist);
                    } else {
                        y.pending_old.push(obj_ptr as usize);
                    }
                }
            }
        }
        // gcj j4/rf (`CRATONVM_GEN_FIN_ROUND_RECORDS_YOUNG`): the young objects
        // the finalizer round reached join its depth-2 set -- in place (their
        // pre-collection address) on a non-moving major, post-copy on a P-C
        // one, which the moving caller maps back to the sources.
        if let (Some(y), Some(pass)) = (true_young.as_mut(), fin_pass) {
            if let Some(reached) = y.reached_in_round.take() {
                if true_root_ask.as_ref().is_some_and(|a| a.pre_copy) {
                    pass.closure_young_post.extend(reached);
                } else {
                    pass.closure.extend(reached);
                }
            }
        }

        // gce e1/c: the in-mark closure's rescues are the same push-site gap
        // the sweep's closure reports; count and say so the same way.
        if pre_rescued > 0 {
            OLD_SWEEP_CLOSURE_RESCUES.fetch_add(pre_rescued as u64, Ordering::Relaxed);
            tracing::warn!(
                "true-root old-gen mark: the mark phase missed {} of {} walked block(s) that a \
                 LIVE old-gen object still references; retained and traced (young targets \
                 included). This is a mark push-site gap -- see old-sweep-liveness.md.",
                pre_rescued,
                walked_objects.len(),
            );
        }
        let mark_coord_cpu_us = match (mark_cpu0, timing) {
            (Some(a), true) => cratonvm_gc_core::thread_cpu::current_thread_cpu_ns()
                .map_or(0, |b| b.saturating_sub(a) / 1000),
            _ => 0,
        };
        phase_timer.phase("mark");
        // t1 w7 G2: the collecting thread's CPU in the mark loop (worker 0 of
        // every drain included) and, under the parallel mark, all workers'.
        phase_timer.part("mark-coord-cpu", mark_coord_cpu_us as u128);
        if par_drains > 0 {
            phase_timer.part("mark-par-cpu", (par_diag.cpu_ns / 1000) as u128);
            phase_timer.part("mark-par-wait", (par_diag.wait_ns / 1000) as u128);
            phase_timer.count("mark-par-locks", par_diag.locks as usize);
            phase_timer.count("mark-par-publishes", par_diag.publishes as usize);
            phase_timer.count("mark-par-wakes", par_diag.wakes as usize);
        }
        let t_mark = std::time::Instant::now();
        phase_timer.part("mark-in-closures", mark_close_us);
        phase_timer.part("mark-young-rounds", mark_round_us);
        // gch h4/pc: the closures run in the mark, and which cut was armed
        // (the deciding rows' positive controls).
        phase_timer.count("mark-closures", mark_closures);
        let par_close_n = crate::old_gen::CLOSE_LIVE_SET_PAR_VERIFIED
            .load(Ordering::Relaxed)
            .saturating_sub(par_close_before);
        if par_close_n > 0 {
            phase_timer.count("mark-par-close-verified", par_close_n as usize);
        }
        if rounds_in_bfs {
            phase_timer.count("mark-rounds-in-bfs", 1);
        }
        if post_close_skipped {
            phase_timer.count("mark-post-close-skipped", 1);
        }
        // t1 G2 (`CRATONVM_GC_OLD_PAR_MARK`): the parallel drains' engagement
        // and their share of `mark` (`par-scanned` vs `par-max-worker`: one
        // worker doing it all reads as equal counts).
        phase_timer.part("mark-young-seed", young_seed_us);
        phase_timer.count("mark-serial-pops", serial_pops);
        if par_drains > 0 {
            phase_timer.count("mark-par-drains", par_drains);
            phase_timer.count("mark-par-workers", par_mark_workers);
            phase_timer.count("mark-par-scanned", par_scanned);
            phase_timer.count("mark-par-max-worker", par_max_worker);
            phase_timer.part("mark-par", par_us);
        }
        // gci i28/cl: the incremental post-close's engagement (the deciding
        // rows' positive control).
        if post_close_incremental_calls > 0 {
            phase_timer.count("mark-post-close-incremental", post_close_incremental_calls);
        }
        // gcj j2/og2: the prefetch ring's engagement (its rows' positive control).
        if mark_prefetch.prefetched() > 0 {
            phase_timer.count("mark-prefetched", mark_prefetch.prefetched());
        }
        // gcd d4/n: the true-root seed's census (young survivors it left out:
        // kept by the young phase only for dead old data).
        if let Some(y) = true_young.as_ref() {
            old_gen.note_true_root_major(Some(y.excluded()));
            // gcd d9/a: tell the caller the seed ran (it then judges which
            // promotions died), and how many it resolved, for the debug line.
            TRUE_ROOT_MARK_OUT.with(|c| c.borrow_mut().ran = true);
            // gci i4/og (`gci-i3og-true-root-major-residual-costs-FIXED-20261012`
            // item 2, `CRATONVM_GC_TRUE_ROOT_BATCHED_VERDICT` only): the
            // judged list, `(destination, source)` sorted by destination, is
            // built here, once the seed RAN, instead of by the caller before
            // the call (where a fallback dropped it unread). `promoted` holds
            // every promotion of the ask whenever the seed runs (an off-grid
            // destination is a fallback), so the list is the one the caller
            // built from `ask.promotions` before.
            if true_root_ask.as_ref().is_some_and(|ask| ask.hand_back_judged) {
                let t_list = std::time::Instant::now();
                let mut by_dest: Vec<(usize, usize)> =
                    y.promoted.iter().map(|&(src, dest, _)| (dest, src)).collect();
                by_dest.sort_unstable();
                let list_us = u64::try_from(t_list.elapsed().as_micros()).unwrap_or(u64::MAX);
                TRUE_ROOT_MARK_OUT.with(|c| {
                    let mut out = c.borrow_mut();
                    out.judged_by_dest = by_dest;
                    out.judged_list_us = list_us;
                });
            }
            tracing::debug!(
                target: "cratonvm::gc",
                excluded = y.excluded(),
                promotions = y.promoted.len(),
                promotions_resolved = y.promoted_resolved(),
                "true-root major: young seed from the true roots",
            );
        }
        // gch h4/wr (`CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD`): the
        // watched young survivors the trace left out, judged here, after the
        // whole fixed point (the young rounds, the in-mark closures and the
        // finalizer round: an object only a resurrected finalizable reaches is
        // reached, as an old one is marked). Handed to the caller through the
        // sweep's out-slot, which `sweep_old_gen_non_moving_impl` reset before
        // this call; only a non-empty verdict is written.
        if let (true, Some(y), Some(w)) = (watched_dead, true_young.as_ref(), watched.as_ref()) {
            let left_out = watched_left_out_by_trace(y, w);
            if !left_out.is_empty() {
                TRUE_ROOT_SWEEP_OUT.with(|c| c.borrow_mut().watched_left_out = left_out);
            }
        }
        // gci i3/og (`CRATONVM_GC_MOVING_MAJOR_WATCHED_DEAD`, opt-in): the P-C
        // twin of the verdict above. The moving cycle handed the watched set's
        // young COPIES (post-copy, in this grid) instead of rooting them; the
        // ones the whole fixed point did not reach go back through the mark's
        // out-slot (the moving cycle owns the `pointer_map` they are keyed in).
        if let (Some(y), Some(ask)) = (true_young.as_ref(), true_root_ask.as_ref()) {
            if ask.pre_copy && !ask.watched_post_copy.is_empty() {
                let left_out = moving_watched_left_out_by_trace(y, &ask.watched_post_copy);
                if !left_out.is_empty() {
                    TRUE_ROOT_MARK_OUT.with(|c| c.borrow_mut().watched_left_out_post = left_out);
                }
            }
        }
        // gcd d9/a: the promotion destinations, ascending, for the pinned
        // compaction to report the ones it drops (`compact_around_pins_watching`).
        let watch_dests: Vec<usize> = match true_young.as_ref() {
            Some(y) if pinned.is_some() && !y.promoted.is_empty() => {
                let mut v: Vec<usize> = y.promoted.iter().map(|&(_, dest, _)| dest).collect();
                v.sort_unstable();
                v            }
            _ => Vec::new(),
        };

        // Snapshot for the free-loop diagnostic below; taken once, outside the
        // per-object loop, and only when the gate is on.
        let sweep_owner_dbg =
            false;
        let sweep_owner_addrs = if sweep_owner_dbg {
            cratonvm_gc_core::external_roots::external_owner_addrs()
        } else {
            None
        };

        // H2-CID0 (2026-08-02): every address the reference processor holds.
        // Needed by BOTH arms now — the `!compact` arm to prove watched
        // survivors, and the compacting arm to say whether a block it is about
        // to drop was weakly referenced (see `COMPACT_DROPPED_WATCHED`).
        // gce e1/c: taken once, before the mark (`watched`, above).
        let drop_flags_for = |addr: usize| -> u8 {
            let mut f = 0u8;
            if watched.as_ref().is_some_and(|w| w.contains(&addr)) {
                f |= OLD_FREED_FLAG_WATCHED;
            }
            if interior_pins.contains(&addr) {
                f |= OLD_FREED_FLAG_INTERIOR_ROOT;
            }
            f
        };

        // ---- gen r4w5/oldcompact5: compaction around pinned objects ----
        //
        // The non-moving path's answer to a fragmentation request: everything
        // a conservative word names stays where it is, everything else slides.
        // Declined over a walk gap (the compactor cannot slide over bytes it
        // did not walk) and on an escape; either way nothing has moved, and
        // the in-place arm below runs over the same grid.
        if let Some(plan) = pinned {
            if gap_recovery {
                plan.refused = Some("the object walk has a gap");
            } else {
                // The compacting arm's drop explanation, for the reclamation
                // ring (an interior-pinned block is pinned here, never dropped,
                // so only the watched flag can apply).
                let mut drop_flags: HashMap<usize, u8> = HashMap::new();
                if watched.is_some() {
                    for &(obj_ptr, _size) in &walked_objects {
                        // SAFETY: `walk_objects` yielded this as a valid object
                        // start, under the old-gen lock held for the pause.
                        let h = unsafe { &*(obj_ptr as *const ObjectHeader) };
                        if h.gc_flags() & GC_FLAG_MARKED == 0 {
                            let f = drop_flags_for(obj_ptr as usize);
                            if f != 0 {
                                drop_flags.insert(obj_ptr as usize, f);
                            }
                        }
                    }
                }
                let young_lo = young_from.base_ptr() as usize;
                let young = (young_lo, young_lo + young_from.capacity());
                // gen r5w2/alloc6: merge, attribute and screen the pins (a
                // stale or one-past-the-end word no longer resurrects an
                // object the mark did not reach — `settle_old_pins`).
                let (pins, report) = settle_old_pins(
                    std::mem::take(&mut pin_bases),
                    std::mem::take(&mut pin_srcs),
                    &walked_objects,
                    old_gen.base_ptr() as usize,
                    plan.fragmentation_triggered,
                );
                plan.report = report;
                // gen r5w6/old10: which of the largest pins a non-word root
                // also names (see `OldPinReport::largest_also_rooted`). Four
                // entries at most, each a pass over the roots and the words:
                // paid only by a pinned compaction.
                let also_rooted = old_pins_also_rooted(
                    &plan.report.largest,
                    old_gen.base_ptr() as usize,
                    &roots[..promo_tail],
                    &plan.words,
                );
                plan.report.largest_also_rooted = also_rooted;
                // gcj j3/og3 (`CRATONVM_GC_OLD_PINNED_COMPACT_REUSE_CLOSE`,
                // opt-in): the mark's last in-mark closure verdict, exactly
                // as the in-place arm reuses it (`reuse_pre_close` below):
                // when nothing was marked after that closure it IS Phase 0b's
                // answer over this grid. Fragmentation repairs reuse this
                // proof directly; other compactions require the switch.
                // `None` (Phase 0b runs) with no in-mark closure (no true-root
                // seed), or after a finalizer round no closure followed.
                let pinned_trusted = if plan.fragmentation_triggered
                    || true
                {
                    match (pre_close_escaped, post_close_escaped) {
                        (Some(escaped), _) if !resurrection_draining => Some(escaped),
                        (_, Some(escaped)) => Some(escaped),
                        _ => None,
                    }
                } else {
                    None
                };
                let fwd_index_before = old_gen.forward_index_compactions();
                // t1 lane F: the pinned compactor's parallel Phase 2 engagement.
                let par_adjust_before = old_gen.par_adjust_compactions();
                let phase0_skips_before = old_gen.phase0_skips();
                let disagreements_before = old_gen.trust_mark_disagreements();
                let frag_pin_budget_ok = !plan.fragmentation_triggered
                    || fragmented_pinned_compaction_pin_budget_ok(
                        plan.report.pinned_bytes,
                        plan.report.live_bytes,
                    );
                if plan.fragmentation_triggered && gc_flags().dbg_gcphase {
                    eprintln!(
                        "[GC] oldgen_frag_pinned_budget: pinned_bytes={} live_bytes={} below_ten_percent={}",
                        plan.report.pinned_bytes,
                        plan.report.live_bytes,
                        frag_pin_budget_ok,
                    );
                }
                if !frag_pin_budget_ok {
                    plan.refused = Some("fragmentation pin set reaches 10% of marked live bytes");
                    let _ = old_gen.note_pinned_compaction_refusal();
                } else {
                match old_gen.compact_around_pins_trusting(
                        &walked_objects,
                        walked_at_seq,
                        pins,
                        young,
                        &drop_flags,
                        plan.element_card_base,
                        &watch_dests,
                        pinned_trusted,
                        closure_starts,
                        plan.fragmentation_slide_goal.or_else(|| {
                            plan.fragmentation_triggered.then(|| {
                                let (_, _, _, request) = old_gen.fragmentation_metrics();
                                request
                            })
                        }),
                    ) {
                        Ok(done) => {
                        if plan.reactive_fragmentation_repair {
                            old_gen.note_fragmentation_pinned_repair();
                        }
                        let t_reclaim = std::time::Instant::now();
                        // gcd d9/a: the true-root seed's dead promotions.
                        if !done.watch_dropped.is_empty() {
                            TRUE_ROOT_MARK_OUT.with(|c| {
                                c.borrow_mut().dropped_dests = done.watch_dropped.clone();
                            });
                        }
                        // Young objects do not move on this path; their
                        // references to moved old objects are rewritten here,
                        // and the roots and every side table by the VM through
                        // the returned map, as for the compacting arm.
                        if !done.pointer_map.is_empty() {
                            Self::fixup_young_old_refs(young_from, &done.pointer_map, young_skips);
                            for root in roots.iter_mut() {
                                let old_addr = root.as_ptr() as usize;
                                if let Some(&new_addr) = done.pointer_map.get(&old_addr) {
                                    // SAFETY: `new_addr` is a survivor's new
                                    // base from the compaction's own map.
                                    *root = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                                }
                            }
                        }
                        plan.compacted = Some((
                            done.pinned,
                            done.moved,
                            done.moved_bytes,
                            done.largest_free_before,
                            done.largest_free_after,
                        ));
                        plan.young_ref_holders = done.young_ref_holders;
                        phase_timer.phase("compact-around-pins");
                        // gcj j3/og3: the pinned compactor's engagement of the
                        // forward index (the j1c rows saw only the slide's
                        // `compact-forward-index` line, which this arm never
                        // printed), its Phase 0b time and the Phase 0 drop.
                        Self::note_compaction_phase0_lines(
                            &phase_timer,
                            &*old_gen,
                            "compact-around-pins",
                            Some(fwd_index_before),
                            phase0_skips_before,
                            disagreements_before,
                        );
                        // t1 lane F (`CRATONVM_GC_OLD_PAR_ADJUST`): its rows'
                        // positive control on the pinned arm.
                        if old_gen.par_adjust_compactions() != par_adjust_before {
                            phase_timer.count("compact-around-pins-par-adjust", 1);
                        }
                        store_old_gc_phase_split(t_entry, t_mark, t_reclaim);
                        return done.pointer_map;
                    }
                    // gcj og3 P2: the mark showed the in-place sweep makes
                    // the room; not a refusal (counted by the old gen).
                    Err(PinnedCompactRefusal::NotNeeded) => {}
                    Err(refusal) => plan.refused = Some(refusal.label()),
                }
                }
            }
        }

        if !compact {
            // Watched addresses (every address the reference processor holds —
            // see `ReferenceProcessor::all_tracked_addrs`) need an explicit
            // survival proof from this sweep: it frees dead blocks in place,
            // so post-GC reference processing can no longer infer survival
            // from "the address is inside old gen". Emit an identity entry for
            // each watched survivor, mirroring what `OldGen::compact` already
            // does for watched objects that happen not to move.
            let mut watched_survivors: cratonvm_types::PointerMap =
                cratonvm_types::PointerMap::default();
            // GCAUD-8: the grid derived before the mark. Nothing since then has
            // allocated or freed in old gen, so re-walking would return the
            // same slice; the mark oracle above and the free loop below now
            // decide against ONE view of the generation rather than two.
            let objects = walked_objects;

            // ---- CRATONVM_DBG_SWEEP_LIVENESS: freed-while-referenced assertion ----
            //
            // GCAUD-8 (2026-08-01): this block is now a DIAGNOSTIC ONLY. It
            // runs BEFORE `OldGen::close_live_set` below, so it still reports
            // what the mark phase actually missed — but the closure then
            // retains those blocks unconditionally, and `=rescue` is no longer
            // the only thing standing between a mark gap and a use-after-free.
            // The paragraphs below describe the world before that change and
            // are kept because they name the failure the counters look for.
            //
            // This sweep decided liveness purely from `GC_FLAG_MARKED`, and the
            // mark that set it has NINE worklist push sites of which only two
            // validate their input (see
            // gc-old-gen-mark-accepts-unvalidated-addresses-FIXED.md).
            // If the mark misses a root, this loop hands a still-referenced
            // block back to the free list and the damage surfaces only much
            // later, at whichever unlucky reader dereferences it next — exactly
            // the shape of the `--nojit` map-node corruption (`tail_node`
            // reading `num_slots=0 class_id=0` in `native_map_put_evict_pinned`,
            // already dead before it was ever pinned).
            //
            // Catch it AT THE FREE, where victim and referrer are both still
            // intact, instead of inferring it from the crash site. `=rescue`
            // additionally retains the referenced block: if the workload then
            // stops crashing, freed-while-live is proven causal, not correlated.
            let sweep_liveness = gc_flags().dbg_sweep_liveness;
            let sweep_rescue = sweep_liveness
                && gc_flags()
                    .dbg_sweep_liveness_value
                    .as_deref()
                    .is_some_and(|v| v.eq_ignore_ascii_case("rescue"));
            let mut doomed: std::collections::HashSet<usize> = std::collections::HashSet::new();
            if sweep_liveness {
                for (obj_ptr, _size) in &objects {
                    // SAFETY: `walk_objects` yields valid old-gen object starts.
                    let h = unsafe { &*(*obj_ptr as *const ObjectHeader) };
                    if h.gc_flags() & GC_FLAG_MARKED == 0 {
                        doomed.insert(*obj_ptr as usize);
                    }
                }
            }
            // Keyed by victim, so a block referenced from many places is
            // reported (and rescued) once. Value = (referrer, class_id, slot).
            let mut referenced_doomed: HashMap<usize, (usize, u32, usize)> = HashMap::new();
            if sweep_liveness && !doomed.is_empty() {
                for (obj_ptr, _size) in &objects {
                    // SAFETY: as above.
                    let h = unsafe { &*(*obj_ptr as *const ObjectHeader) };
                    if h.gc_flags() & GC_FLAG_MARKED == 0 {
                        continue; // itself doomed; its refs keep nothing alive
                    }
                    // SAFETY: a marked old-gen object has a valid header and an
                    // in-bounds body — `for_each_ref_slot`'s contract.
                    unsafe {
                        for_each_ref_slot(*obj_ptr, h, |ref_ptr, slot| {
                            let victim = ref_ptr as usize;
                            if doomed.contains(&victim) {
                                referenced_doomed.entry(victim).or_insert((
                                    *obj_ptr as usize,
                                    h.class_id.as_u32(),
                                    slot,
                                ));
                            }
                        });
                    }
                }
                // Same reasoning as the young half: a per-sweep line so that
                // "no output" cannot be mistaken for "checked and clean".
                eprintln!(
                    "[SWEEP-LIVENESS old] hits={} doomed_blocks={} old_gen_scanned={}{}",
                    referenced_doomed.len(),
                    doomed.len(),
                    objects.len(),
                    if sweep_rescue { " (rescuing)" } else { "" },
                );
                if !referenced_doomed.is_empty() {
                    eprintln!(
                        "[SWEEP-LIVENESS] {} of {} doomed old-gen blocks are STILL REFERENCED by a live old-gen object{}",
                        referenced_doomed.len(),
                        doomed.len(),
                        if sweep_rescue { " (rescuing)" } else { "" },
                    );
                    for (victim, (referrer, referrer_cid, slot)) in
                        referenced_doomed.iter().take(12)
                    {
                        // SAFETY: `victim` is a base `walk_objects` yielded.
                        let vh = unsafe { &*(*victim as *const ObjectHeader) };
                        eprintln!(
                            "[SWEEP-LIVENESS]   victim=0x{victim:x} class_id={} num_slots={} \
                             <- referrer=0x{referrer:x} class_id={referrer_cid} slot={slot}",
                            vh.class_id.as_u32(),
                            vh.num_slots(),
                        );
                    }
                }
            }

            // ---- GCAUD-8: close the live set before deciding what is dead ----
            //
            // `GC_FLAG_MARKED == 0` is the whole liveness test below, and it
            // asserts that the mark phase found every live object. That is an
            // assumption about eight push sites, not a check, and every one of
            // them fails OPEN: a screen reject, an extent bail in
            // `scan_object_for_old_refs`, a young-walk desync, or a root the
            // caller left on a pre-promotion address all drop a live edge
            // rather than over-retain. The block then goes back on the free
            // list while a live object still points at it.
            //
            // `OldGen::compact` has been closed against exactly this since
            // Phase 0 was written: "the compactor must not corrupt the heap when
            // the marker under-marks". This arm — the one that actually creates
            // the freed-but-referenced block the compactor later refuses to
            // relocate (`COMPACT_ESCAPE_HITS`) — had no such guard. Run the same
            // fixpoint here, using the grid walk as the admission proof, and an
            // unmarked object that a marked object still references is retained
            // for another cycle instead of freed under a live pointer.
            //
            // This does NOT retain everything: an object that is unmarked and
            // unreferenced is untouched by the closure and is still freed by the
            // loop below. That is what the positive controls pin.
            //
            // Cost: one extra pass over the live objects' reference slots per
            // in-place major sweep, plus — only when the mark was wrong — one
            // scan per rescued object that sat behind the pass's cursor (gen
            // r4/oldgen: a worklist since 2026-09-23; it used to repeat the
            // whole pass until nothing changed). Same routine as the
            // compactor's Phase 0; `old_gen::CLOSE_LIVE_SET_*` count both.
            //
            // gen r4w4/oldgen4: over a recovered walk gap, a reference INTO the
            // gap is not an escape (the gap is retained whole); see
            // `OldGen::close_live_set_with_gaps`.
            // gce e1/c: under the true-root seed the mark already closed the
            // live set (see `pre_close_done`); with no finalizer round after
            // that closure nothing has been marked since, so a second pass
            // would rescue nothing and report the same escape verdict.
            // gcf f1/c: or the post-resurrection closure's, which is the last
            // thing that marked anything.
            let reuse_pre_close = match (pre_close_escaped, post_close_escaped) {
                (Some(escaped), _) if !resurrection_draining => Some((0usize, escaped)),
                (_, Some(escaped)) => Some((0usize, escaped)),
                _ => None,
            };
            let (rescued, escaped) = if let Some(verdict) = reuse_pre_close {
                verdict
            } else if let Some(s) = closure_starts {
                // gcg g3/e: the collection's shared start set (the same
                // gap rule as the two arms below).
                let gaps: &[(usize, usize)] = if gap_recovery { &walk_gaps } else { &[] };
                old_gen.close_live_set_shared(&objects, gaps, s)
            } else if gap_recovery {
                old_gen.close_live_set_with_gaps(&objects, &walk_gaps)
            } else {
                old_gen.close_live_set(&objects)
            };
            phase_timer.phase("close");

            // H2-CID0 residual (2026-08-03) — WHO still points at what this
            // IN-PLACE sweep is about to free.
            //
            // Deliberately AFTER `close_live_set`, so the doomed set this scans
            // is the one that actually gets freed rather than the raw mark
            // result the closure then rescues. Every other instrument on this
            // arm reports a consequence — the ring says what the block held,
            // `reclaimed_hole_at` says the address is in a hole, the
            // `SWEEP_LIVENESS` block above reports the pre-closure gap — and
            // none of them names the surviving REFERRER, which is the whole
            // question. `close_live_set` structurally cannot: it closes over
            // MARKED OLD-GEN referrers only, so a young referrer or a bare root
            // is invisible to it.
            //
            // Three outcomes, and they point at three different defects:
            //   * referrer is a MARKED old-gen object -> the mark BFS missed an
            //     edge it was handed (close_live_set should have caught it);
            //   * referrer is in the YOUNG survivor space or the root slice ->
            //     `mark_young_to_old_refs` / the root seed missed it;
            //   * NO referrer anywhere -> the surviving reference is not in the
            //     heap or the root slice at all (a JIT spill slot, a native side
            //     table, a thread this cycle did not cover), which is a
            //     different bug entirely and would retire the "mark-phase gap"
            //     framing.
            //
            // A doomed block referenced only from ANOTHER doomed block is
            // reported with `marked=false` on the containing object: that is
            // the whole-subgraph-condemned-together case the promotion-seed
            // comment above warns about, and it is why silence from a
            // heap-edge-only detector is not evidence the mark was complete.
            if doomed_referrers_dbg() {
                report_doomed_referrers("sweep", &objects, old_gen, young_from, roots);
            }
            if rescued > 0 {
                OLD_SWEEP_CLOSURE_RESCUES.fetch_add(rescued as u64, Ordering::Relaxed);
                tracing::warn!(
                    "in-place old-gen sweep: the mark phase missed {} of {} walked block(s) \
                     that a LIVE old-gen object still references; retaining them. This is a \
                     mark push-site gap, not floating garbage — see \
                     old-sweep-liveness.md.",
                    rescued,
                    objects.len(),
                );
            }
            if escaped {
                // Distinct from `rescued`: the referent is not in `objects` at
                // all, so it is already on the free list (or behind a
                // `scan_region` anomaly). Freeing nothing this cycle would not
                // bring it back, so unlike `compact` this arm reports and
                // carries on — every decision it goes on to make is still
                // backed by the closure above.
                let n = OLD_SWEEP_ESCAPE_HITS.fetch_add(1, Ordering::Relaxed);
                if n < 8 {
                    tracing::warn!(
                        "in-place old-gen sweep ({} walked objects): a live old-gen object \
                         references an in-old-gen address the object walk did not yield — an \
                         EARLIER reclamation already freed a live block. See \
                         `old_gen::COMPACT_ESCAPE_HITS` and old-sweep-liveness.md.",
                        objects.len(),
                    );
                }
            }

            // gen r4/oldgen (2026-09-23): return each run of ADJACENT dead
            // objects to the free list as ONE block, not one block per object.
            //
            // The per-object form pushed every dead object into a size bucket
            // and then relied on the `coalesce_free_blocks` below to collect,
            // sort and merge them all again — an O(d log d) sort over d dead
            // OBJECTS per in-place sweep, inside the pause, when what it
            // produces depends only on the dead RUNS. Old-gen garbage is
            // typically contiguous (a promotion buffer's worth of objects dies
            // together), so d is routinely orders of magnitude above the run
            // count.
            //
            // Equivalent by construction: `walk_objects` yields ascending,
            // back-to-back `(base, size)` pairs inside each allocated region,
            // so "the next dead object starts where the run ends" means the
            // union of their spans is exactly one contiguous span of freed
            // objects — never a survivor, never an unwalked byte (a region
            // boundary or a `scan_region` break leaves a gap and ends the
            // run). The strict-adjacency merge below would have produced the
            // same block from the separate pieces; `used_bytes` falls by the
            // same total; `reclaim_epoch` still moves (it is compared for
            // equality, not counted). Every per-object instrument — the
            // reclamation ring, the A2 breadcrumb, the owner and interior-pin
            // reporters — still runs per object. Pinned at the `OldGen` level
            // by `freeing_a_contiguous_dead_run_as_one_block_equals_freeing_
            // each_then_coalescing`.
            //
            // Not done under `no_oldgen_coalesce`: that switch exists to keep
            // dead blocks UNmerged, so there each object is still its own
            // block (freed one object later than before, in the same order).
            let batch_dead_runs = !gc_flags().no_oldgen_coalesce;
            let mut dead_run: Option<(usize, usize)> = None;
            // gen r4w3/oldgen3 (2026-09-23) — FAIL CLOSED ON AN INCOMPLETE WALK
            // (the STW half of `gengc-r4-mark-walk-desync-tail-is-untraced`).
            //
            // `walk_objects` stops a region at the first implausible header
            // (`scan_region`'s break), and everything allocated behind the
            // break is absent from `objects`. The mark refuses references into
            // that unwalked tail (`mark_and_push_old_gen`'s
            // `interior_to_the_walk` rule), so a live tail object X is neither
            // marked nor TRACED — and a walked object C reachable only through
            // X keeps a clear mark bit while live. `close_live_set` cannot see
            // it either: it closes over MARKED objects, and X is not one. The
            // loop below would free C under X's pointer.
            //
            // The walk is complete exactly when it accounts for every
            // allocated byte — `used_bytes` is maintained by `alloc`/`free`
            // independently of headers — the same GCAUD-9 test `OldGen::compact`
            // abandons on (`COMPACT_WALK_GAP_HITS`) and the concurrent cycle
            // refuses its sweep on (`CONC_MARK_WALK_DESYNC_ABORTS`). When it is
            // not, reclaim nothing this cycle: every walked object is treated
            // as a survivor (mark bit cleared, watched ones proved alive), the
            // generation is retained, and the counter says so. Over-retention
            // until the anomaly is gone, instead of a use-after-free.
            //
            // gen r4w4/oldgen4 (2026-09-24) — ...UNLESS the gap was recovered
            // (`gap_recovery`): then every byte the walk missed is inside a
            // `walk_gaps` range (the sums were checked equal), each range was
            // scanned as a conservative root range before the BFS
            // (`seed_old_gen_walk_gaps` — heap words, narrow words, and every
            // side-table edge an owner inside a gap holds), so a walked object
            // reachable only through an unwalked one IS marked, and the free
            // loop below may run. The gap bytes themselves are never freed
            // (they are not in `objects`). `gengc-r4w3-oldgen3-a-walk-desync-stops-all-old-gen-reclamation`.
            let walked_bytes: usize = objects.iter().map(|&(_, size)| size).sum();
            let walk_covers_used = walked_bytes == old_gen.used() || gap_recovery;
            if gap_recovery {
                let n = old_gen.note_walk_gap_recovery(gap_bytes);
                if n < 8 || n.is_power_of_two() {
                    tracing::warn!(
                        gaps = walk_gaps.len(),
                        gap_bytes,
                        walked_bytes,
                        used_bytes = old_gen.used(),
                        recoveries = n + 1,
                        "in-place old-gen sweep RECOVERED from an incomplete object walk: the \
                         unwalked bytes behind an implausible header were scanned as conservative \
                         roots and retained, and the rest of the generation is reclaimed as usual. \
                         The header that broke the walk is still there (SCAN_REGION_BREAK_HITS). \
                         See gengc-r4w3-oldgen3-a-walk-desync-stops-all-old-gen-reclamation.",
                    );
                }
            }
            if !walk_covers_used {
                let n = old_gen.note_sweep_walk_gap();
                if n < 8 {
                    tracing::warn!(
                        walked_bytes,
                        used_bytes = old_gen.used(),
                        walked_objects = objects.len(),
                        "in-place old-gen sweep FREES NOTHING this cycle: the object walk \
                         covered fewer bytes than are allocated (a region's scan broke on an \
                         implausible header), so objects behind the break were never traced \
                         and a walked object reachable only through one of them would be \
                         freed while live. See gengc-r4-mark-walk-desync-tail-is-untraced.",
                    );
                }
            }
            // gcf f2/c: the watched set's OLD-GEN members, sorted, asked in the
            // grid's ascending order (`AscendingMembership`) instead of one
            // SipHash lookup per walked object, live or dead: the same answers
            // (`gcf_f2c_ascending_membership_answers_as_the_set`), and any
            // live `WeakReference` in the program made every in-place major
            // pay N hashes for N old objects.
            let mut watched_in_old =
                AscendingMembership::new(watched.as_ref(), |a| old_gen.contains(a as *const u8));
            for &(obj_ptr, total_size) in &objects {
                // SAFETY: `walk_objects` returns valid old-gen object starts.
                // Marked objects remain at their current address; every other
                // object was unreachable from the complete precise +
                // conservative root set and can be returned to the free list.
                let header = unsafe { &mut *(obj_ptr as *mut ObjectHeader) };
                if header.gc_flags() & GC_FLAG_MARKED != 0 || !walk_covers_used {
                    // gen r4w3/oldgen3: `!walk_covers_used` retains the
                    // object exactly as a marked survivor is retained (see
                    // above) — including the identity entry a watched
                    // survivor needs, so reference processing does not read
                    // "absent from the map" as "died this cycle".
                    header.clear_gc_flags(GC_FLAG_MARKED);
                    if watched_in_old.contains(obj_ptr as usize) {
                        watched_survivors.insert(obj_ptr as usize, obj_ptr as usize);
                    }
                } else if sweep_rescue && referenced_doomed.contains_key(&(obj_ptr as usize)) {
                    // CRATONVM_DBG_SWEEP_LIVENESS=rescue: a live object still
                    // points here, so keep the block. Diagnostic only — this
                    // leaks whatever the mark genuinely missed, and exists to
                    // answer "is freed-while-live the cause?" with a run, not
                    // an argument. Clear the mark bit like the survivor arm so
                    // the next cycle starts from a clean slate.
                    let header = unsafe { &mut *(obj_ptr as *mut ObjectHeader) };
                    header.clear_gc_flags(GC_FLAG_MARKED);
                } else {
                    // A2 forensic breadcrumb (CRATONVM_DBG_A2): preserve the
                    // victim's pre-free identity so a later zero-header /
                    // wild-receiver access at this address can be attributed
                    // to THIS sweep having freed a still-referenced object
                    // (the DoHead freed-while-live investigation).
                    if cratonvm_gc_core::a2dbg::enabled() {
                        cratonvm_gc_core::a2dbg::record_old_sweep_free(
                            obj_ptr as usize,
                            header.class_id.as_u32(),
                            header.num_slots(),
                            total_size,
                        );
                    }
                    // DIAGNOSTIC (`CRATONVM_DBG_OLDSWEEP_OWNERS=1`): name every
                    // freed object that is a REGISTERED OVERLAY OWNER — i.e. a
                    // collection whose backing state lives in a native side
                    // table. Freeing one is normal when the collection is
                    // genuinely dead, but it is also the exact event behind a
                    // live collection reading back as empty (the prune then
                    // observes `is_allocated_addr == false` and correctly drops
                    // its state), so this is the site that has to be watched to
                    // tell those two apart.
                    if sweep_owner_dbg {
                        if let Some(owners) = sweep_owner_addrs.as_ref() {
                            if owners.contains(&(obj_ptr as usize)) {
                                eprintln!(
                                    "[oldsweep] FREEING overlay owner 0x{:x} class_id={} \
                                     num_slots={} size={}",
                                    obj_ptr as usize,
                                    header.class_id.as_u32(),
                                    header.num_slots(),
                                    total_size,
                                );
                            }
                        }
                    }
                    // H2-CID0 (2026-08-01): remember WHAT was here. `OldGen::free`
                    // leaves the bytes in place, so a dangling read of this block
                    // returns the stale object until the allocator reuses it —
                    // and reuse zeroes the block before writing the new header,
                    // which is how a still-referenced old-gen object comes back as
                    // an all-zero `ClassId(0)` header, i.e. `java.lang.Object`.
                    // Always on, and cheap: an in-place old sweep only runs past
                    // 75 % occupancy, and the record is one relaxed `fetch_add`
                    // plus five relaxed stores. See old-sweep-liveness.md.
                    // gcf f2/c: `drop_flags_for`, answered from the ascending
                    // watched cursor (the same bits; see above the loop).
                    let interior_pinned =
                        !interior_pins.is_empty() && interior_pins.contains(&(obj_ptr as usize));
                    let mut freed_flags = 0u8;
                    if watched_in_old.contains(obj_ptr as usize) {
                        freed_flags |= OLD_FREED_FLAG_WATCHED;
                    }
                    if interior_pinned {
                        freed_flags |= OLD_FREED_FLAG_INTERIOR_ROOT;
                    }
                    if interior_pinned {
                        // Only reachable with `CRATONVM_GC_NO_OLD_INTERIOR_PINS`
                        // set: the pin above marks these, so the free loop never
                        // sees one otherwise. That makes this the negative
                        // control for the pin rather than dead code.
                        let n = OLD_SWEEP_FREED_INTERIOR_PINNED.fetch_add(1, Ordering::Relaxed);
                        if n < 8 {
                            tracing::error!(
                                target: "cratonvm::gc::guard",
                                obj = format!("{:#x}", obj_ptr as usize),
                                class_id = header.class_id.as_u32(),
                                size = total_size,
                                "in-place old-gen sweep is freeing a block an INTERIOR \
                                 conservative root points into. That root cannot be rewritten, \
                                 so the next allocation reuses and ZEROES the block under it — \
                                 the ClassId(0) / java.lang.Object face.",
                            );
                        }
                    }
                    record_old_freed(
                        obj_ptr as usize,
                        total_size,
                        header.class_id.as_u32(),
                        ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)),
                        OLD_FREED_SITE_INPLACE_SWEEP,
                        freed_flags,
                    );
                    // gen r4/oldgen: extend the current dead run, or retire it
                    // and start a new one at this object. See the note above
                    // the loop.
                    let addr = obj_ptr as usize;
                    let extends_run =
                        batch_dead_runs && dead_run.is_some_and(|(start, len)| start + len == addr);
                    if extends_run {
                        if let Some((_, len)) = dead_run.as_mut() {
                            *len += total_size;
                        }
                    } else {
                        if let Some((start, len)) = dead_run.take() {
                            // SAFETY: `[start, start + len)` is the union of
                            // back-to-back `(base, size)` pairs `walk_objects`
                            // yielded for now-unmarked old-gen objects (a
                            // single pair when runs are not batched), so
                            // returning that span to the free list is exactly
                            // returning each object.
                            unsafe { old_gen.free(start as *mut u8, len) };
                        }
                        dead_run = Some((addr, total_size));
                    }
                }
            }
            if let Some((start, len)) = dead_run.take() {
                // SAFETY: as for the in-loop retire above — the last run is a
                // union of back-to-back walked spans of unmarked objects.
                unsafe { old_gen.free(start as *mut u8, len) };
            }
            // FRAGMENTATION FIX (xt-helper-window OOM, 2026-07-31): this arm
            // is the ONLY old-gen reclamation that runs while a live JIT
            // frame's roots are conservative, and it never compacts. Without
            // this, every object it frees becomes a permanently isolated
            // free block (`OldGen::free` defers coalescing to `compact`,
            // which this path never reaches), so a workload that keeps the
            // moving-young coverage proof unprovable — e.g. 10-100 threads
            // blocked in a native read under a JIT frame, the
            // `xt-helper-window-conservative-scan` fallback — fragments old
            // gen monotonically until a modest array allocation OOMs on a
            // mostly-free generation. Amortised: one O(n log n) merge per
            // sweep, not per freed object — and since gen r4/oldgen `n` gains
            // one block per dead RUN rather than per dead object (see the
            // batched free above).
            let merged = old_gen.coalesce_free_blocks();
            if merged > 0 {
                tracing::debug!(
                    merged_blocks = merged,
                    free_blocks_now = old_gen.free_block_count(),
                    "in-place old-gen sweep: coalesced adjacent free blocks"
                );
            }
            // gen r4w3/oldgen3 (2026-09-23): sample what an in-place give-back
            // could return and, under `CRATONVM_GC_OLD_GIVE_BACK` (opt-in),
            // return the whole granules of large free blocks to the OS. Inside
            // this pause, under the old-gen lock, after the coalesce made the
            // blocks maximal — see `OldGen::after_in_place_sweep` for why that
            // placement is what makes `reset_range` safe here.
            let _ = old_gen.after_in_place_sweep(gc_flags().old_give_back);
            phase_timer.phase("free");
            // gch h2/og (X3): the in-place arm has no fix-up; the sweep is
            // the reclaim half.
            store_old_gc_phase_split(t_entry, t_mark, std::time::Instant::now());
            return watched_survivors;
        }

        // ---- Compact phase ---- sliding compaction of old gen ----

        // H2-CID0 (2026-08-02): the reclamation ring's `freed_by` said
        // `old-gen mark-compact` for the H2 `MVStore` victim, which moved the
        // question from "which sweep" to "which referrer did the mark miss".
        // `close_live_set_over_old_gen` (Phase 0, inside `compact`) already
        // covers every referrer that is a MARKED old-gen object, so only two
        // candidates remain that it structurally cannot: a `Reference` (weak
        // reachability, where the defect would be a missing CLEAR rather than a
        // missing mark) and an interior conservative root. Record both per
        // dropped block so the reader, minutes later, gets the answer instead
        // of the question.
        let mut drop_flags: HashMap<usize, u8> = HashMap::new();
        // gengc-round1-core (2026-09-20): skip the walk entirely when it can
        // only produce an empty map. `drop_flags_for` returns non-zero only if
        // `watched` is `Some` or `interior_pins` is non-empty;
        // `watched_referents_snapshot()` is `None` whenever no Weak/Soft/
        // Phantom referent is registered, and this arm is unreachable with a
        // non-empty `interior_pins` unless `CRATONVM_GC_NO_OLD_INTERIOR_PINS`
        // is set (the downgrade above sends such a cycle to the in-place
        // sweep). So on a normal cycle this was an O(old-gen) pass that faults
        // in every header inside the pause to build a map that is always
        // empty.
        if watched.is_some() || !interior_pins.is_empty() {
            let (mut n_watched, mut n_interior) = (0usize, 0usize);
            for &(obj_ptr, _size) in &walked_objects {
                // SAFETY: `walk_objects` yielded this as a valid object start.
                let h = unsafe { &*(obj_ptr as *const ObjectHeader) };
                if h.gc_flags() & GC_FLAG_MARKED != 0 {
                    continue;
                }
                let f = drop_flags_for(obj_ptr as usize);
                if f != 0 {
                    if f & OLD_FREED_FLAG_WATCHED != 0 {
                        n_watched += 1;
                    }
                    if f & OLD_FREED_FLAG_INTERIOR_ROOT != 0 {
                        n_interior += 1;
                    }
                    drop_flags.insert(obj_ptr as usize, f);
                }
            }
            if n_watched > 0 {
                COMPACT_DROPPED_WATCHED.fetch_add(n_watched as u64, Ordering::Relaxed);
            }
            if n_interior > 0 {
                // gengc-round1-core (2026-09-20): SAY THAT THIS IS ITS OWN
                // NEGATIVE CONTROL. The downgrade above is
                // `compact && !interior_pins.is_empty() && !no_interior_pins`,
                // so reaching the compacting arm with a non-empty
                // `interior_pins` IMPLIES `CRATONVM_GC_NO_OLD_INTERIOR_PINS`
                // is set. The in-place arm's twin counter already carries that
                // caveat in a comment; this one did not, and its wording ("the
                // compacting arm is supposed to run only with a precise root
                // set; this says otherwise") invites a future reader to treat
                // a flag-induced reading as independent evidence that the
                // downgrade failed. It is not.
                COMPACT_DROPPED_INTERIOR_ROOT.fetch_add(n_interior as u64, Ordering::Relaxed);
                let n = COMPACT_INTERIOR_REPORTS.fetch_add(1, Ordering::Relaxed);
                if n < 8 {
                    tracing::error!(
                        target: "cratonvm::gc::guard",
                        blocks = n_interior,
                        "old-gen mark-compact is dropping {n_interior} block(s) an INTERIOR \
                         conservative root points into. The compacting arm is supposed to run \
                         only with a precise, rewritable root set, and the interior-pin \
                         downgrade normally guarantees that — so this line is reachable ONLY \
                         under CRATONVM_GC_NO_OLD_INTERIOR_PINS, its own negative control. \
                         Read it as 'the downgrade would have been needed here', not as \
                         'the downgrade failed'. A mark would not be enough either — the \
                         block would have to be PINNED.",
                    );
                }
            }
        }
        // H2-CID0 (2026-08-02) — WHO still points at what this compaction is
        // about to drop.
        //
        // Every other instrument in this family reports a consequence: the ring
        // says what the block held, `reclaimed_hole_at` says the address is in a
        // free hole, `SWEEP_EDGES` says nothing at all. None of them names the
        // referrer, and the referrer is the whole question — `close_live_set`
        // structurally covers only referrers that are MARKED old-gen objects.
        //
        // So word-scan for the doomed bases: the whole old-gen backing store
        // (classifying each hit by the object grid and its mark bit), the young
        // SURVIVOR space (`young_from` is post-swap here), and the root slice.
        // A Bloom-style prefilter keeps the common case to one shift, one
        // multiply and one bit test per word.
        if doomed_referrers_dbg() {
            report_doomed_referrers("compact", &walked_objects, old_gen, young_from, roots);
        }
        // gen r4/oldgen (2026-09-23): compact over the grid derived at the top
        // of this function instead of letting `compact` walk the generation
        // again (item 3 of `gengc-core-old-gen-pause-path-redundant-walks`).
        // Nothing since the walk has allocated or freed in old gen; if that
        // ever stops being true the stamp catches it and `compact` re-walks.
        let fwd_index_before = old_gen.forward_index_compactions();
        let par_adjust_before = old_gen.par_adjust_compactions();
        let phase0_skips_before = old_gen.phase0_skips();
        let disagreements_before = old_gen.trust_mark_disagreements();
        // gcj j3/og3 (`CRATONVM_GC_OLD_COMPACT_TRUST_MARK`, opt-in; the Phase 0
        // drop's full design): the BFS vouches for the closure when it recorded
        // no mark fact (every reference of every object it scanned reached a
        // marked, pushed walked base, and no scan bailed) and nothing marked
        // without a push: no in-mark closure, no true-root seed (none runs under
        // `compact`), and no finalizer round that marked anything -- j3a rows
        // `og3_tm_on`: a round is due on every major with any registered
        // finalizable, so "no round" never held; a round that popped nothing
        // and ran no `close_live_set` (the seed's closure marks without
        // pushing; `CLOSE_LIVE_SET_CALLS` unchanged, a process-wide count, so
        // another heap's closure only makes this answer `None`) marked nothing.
        // Anything else hands `None`: Phase 0 runs as before.
        let slide_trusted = (trust_mark_requested
            && !marks_at_entry
            && true_young.is_none()
            && (!resurrection_draining || resurrection_pops == 0)
            && mark_closures == 0
            && crate::old_gen::CLOSE_LIVE_SET_CALLS.load(Ordering::Relaxed) == closures_before
            && old_gen.mark_gap_facts() == mark_facts_before)
            .then_some(false);
        // gcj j1/og: Phase 0's closure admits through this collection's
        // shared start set (built from this very grid) instead of building a
        // second bitmap of the same bases.
        let compact_map = old_gen.compact_with_walked_objects_trusting(
            walked_objects,
            walked_at_seq,
            &drop_flags,
            closure_starts,
            slide_trusted,
        );
        phase_timer.phase("compact");
        // gcj j3/og3: Phase 0's time and the drop's engagement.
        Self::note_compaction_phase0_lines(
            &phase_timer,
            &*old_gen,
            "compact",
            None,
            phase0_skips_before,
            disagreements_before,
        );
        // gcj j1/og (`CRATONVM_GC_OLD_COMPACT_FORWARD_INDEX`): the switch's
        // engagement, next to the `compact` phase it speeds up.
        let fwd_index_used = old_gen
            .forward_index_compactions()
            .wrapping_sub(fwd_index_before);
        if fwd_index_used > 0 {
            phase_timer.count("compact-forward-index", fwd_index_used as usize);
        }
        // gcj j2/og2 (`CRATONVM_GC_OLD_PAR_ADJUST`): the parallel Phase 2's
        // engagement (its rows' positive control).
        if old_gen.par_adjust_compactions() != par_adjust_before {
            phase_timer.count("compact-par-adjust", 1);
        }
        let t_reclaim = std::time::Instant::now();

        // ---- Cross-gen fixup ---- update young-gen refs into old gen ----

        if !compact_map.is_empty() {
            Self::fixup_young_old_refs(young_from, &compact_map, young_skips);

            // ---- Root fixup ---- update roots pointing into old gen ----
            for root in roots.iter_mut() {
                let old_addr = root.as_ptr() as usize;
                if let Some(&new_addr) = compact_map.get(&old_addr) {
                    // SAFETY: `new_addr` comes from the compaction map, pointing to a valid relocated object.
                    *root = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                }
            }
        }
        phase_timer.phase("fixup");
        store_old_gc_phase_split(t_entry, t_mark, t_reclaim);

        compact_map
    }

    /// Scan young from-space for references into old gen and mark them.
    ///
    /// gen r4w5/oldcompact5: `anomaly_pins`, when given, collects every old-gen
    /// object the CONSERVATIVE fallback below names (an exact base, or a word
    /// strictly inside a walked object). A pinned compaction must not move
    /// them: `fixup_young_old_refs` walks the same stretch the same way and
    /// can only rewrite a word that equals a moved object's old base, which a
    /// primitive in an unparseable stretch may do — and an interior word it
    /// cannot rewrite at all.
    ///
    /// gen r5w5/old9: generic over the admission oracle ([`OldMarkGrid`]): the
    /// walked grid everywhere by default, the block-offset oracle on the
    /// O(live) collection (`live_sweep`), which passes no `anomaly_pins`.
    pub fn mark_young_to_old_refs<G: OldMarkGrid + ?Sized>(
        young_from: &Arena,
        old_gen: &OldGen,
        walked: &G,
        young_skips: &[(usize, usize)],
        worklist: &mut Vec<*mut u8>,
        anomaly_pins: Option<&mut Vec<usize>>,
    ) {
        Self::mark_young_to_old_refs_deferring(
            young_from,
            old_gen,
            walked,
            young_skips,
            worklist,
            anomaly_pins,
            None,
        );
    }

}
