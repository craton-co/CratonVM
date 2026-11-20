#[allow(unused_imports)]
use super::*;

impl G1Collector {
    /// [`Self::record_collection_with_phases`] with LANE W4-B's drain-pass
    /// rule as a parameter.
    ///
    /// The `_within` shape exists here for the reason it exists everywhere
    /// else in this file: the flag is latched in a `OnceLock` for the life of
    /// the process, so a test that wants BOTH arms cannot get them by setting
    /// an environment variable twice. The A/B this change rests on is a
    /// comparison of what two controllers learn from the same record, and a
    /// test that could only ever see one arm would be pinning the current
    /// build rather than the rule.
    pub fn record_collection_with_phases_within(
        &self,
        collection_type: G1CollectionType,
        pause_us: u64,
        stats: &GcStats,
        phases: G1PausePhases,
        drain_not_a_pause: bool,
        evac_cost_real: bool,
    ) {
        // Relaxed ordering: these are statistics counters for monitoring /
        // logging only. They guard no data and a slightly stale read is
        // harmless.
        self.collection_count.fetch_add(1, Ordering::Relaxed);
        // gci i27/gr: arm (or disarm) the per-object referent verdict for this
        // collection count. Read back on the collecting thread, in the same
        // stop-the-world window, so the relaxed order above is enough.
        self.note_refproc_evacuation_record(collection_type, &phases);
        if collection_type == G1CollectionType::Mixed {
            self.mixed_collection_count.fetch_add(1, Ordering::Relaxed);
        }
        self.total_pause_us.fetch_add(pause_us, Ordering::Relaxed);
        // gci i9/fa: the run's phase sums (`[GC] g1 pause-phases:`).
        self.phase_totals.note(&phases, pause_us);

        {
            let mut hist = self.pause_history.lock();
            if hist.len() >= PAUSE_HISTORY_CAP {
                hist.pop_front();
                self.pause_history_dropped.fetch_add(1, Ordering::Relaxed);
            }
            hist.push_back(G1PauseRecord {
                collection_type,
                pause_us,
                objects_copied: stats.objects_copied,
                bytes_copied: stats.bytes_copied,
                bytes_freed: stats.bytes_freed,
                phases,
            });
        }

        // LANE W4-B — is this record allowed to TEACH the adaptive
        // controllers, or only to be reported?
        //
        // A drain pass (`retry_after_evacuation_failure` → this function, up
        // to eight times inside one pause that has already recorded itself) is
        // a stop-the-world interval and a real cost, but it is not a SAMPLE of
        // what the next ordinary pause will cost: whole-heap fix-up, a closure
        // that copies only the self-forwarded residue, and drawn exclusively
        // from the state closest to OOM. Every estimator below is an estimator
        // of the next ordinary pause. See [`g1_drain_not_a_pause`] for the
        // full argument, the three things a drain pass IS still allowed to
        // teach, and the measured effect.
        //
        // Computed once, here, rather than tested at each of the four sites,
        // so the four cannot drift into disagreeing about what a drain is.
        let teaches_controllers = !(phases.drain_pass && drain_not_a_pause);
        if !teaches_controllers {
            DRAIN_CONTROLLER_SAMPLES_REFUSED.fetch_add(1, Ordering::Relaxed);
        }

        // LANE W7-L — what did this mixed pause RECLAIM against what it
        // COPIED? The one funnel both mixed drivers (serial and
        // `CRATONVM_G1_PARALLEL_MIXED`) return through, so the breaker cannot
        // be blinded by a routing change.
        //
        // Under `teaches_controllers` for the same reason every estimator
        // below is: a drain pass copies one pause's self-forwarded residue
        // with no to-space left and frees almost nothing, so it reads futile
        // by construction. Charging it to the streak would trip the breaker
        // on the evacuation-failure path — which is the state where the mixed
        // sequence is this collector's full GC and the last thing that should
        // be refused. See `g1_drain_not_a_pause`.
        //
        // gci i18/mx -- nor an evacuation-failure pause (its `bytes_freed`
        // leaves out every region it kept) or a full / plan compaction (its
        // old half is not the mixed choice): neither samples old-to-old
        // copying, and both read futile by construction. Counted apart
        // (`mixed_futile_skipped_samples`) so a census still sees them.
        if teaches_controllers && matches!(collection_type, G1CollectionType::Mixed) {
            if mixed_choice::is_futile_breaker_sample(phases.evacuation_failed, phases.compaction) {
                self.note_mixed_productivity(stats.bytes_copied, stats.bytes_freed);
            } else {
                self.mixed_futile_skipped_samples
                    .fetch_add(1, Ordering::Relaxed);
            }
        }

        // G1AUD-9 — re-size the young generation from this pause's measured
        // cost against `max_gc_pause_ms`.
        // Item 10 — remember what the fix-up walk cost, so the next mixed
        // CSet's copy budget is the goal minus it (`old_cset_copy_budget_ns`).
        if teaches_controllers
            && matches!(
                collection_type,
                G1CollectionType::YoungOnly | G1CollectionType::Mixed
            )
        {
            let observed = phases.fixup_us.saturating_mul(1_000);
            let prev = self.fixup_ns_ema.load(Ordering::Relaxed);
            let next = if prev == 0 {
                observed
            } else {
                prev.saturating_mul(7).saturating_add(observed) / 8
            };
            self.fixup_ns_ema.store(next, Ordering::Relaxed);

            // LANE E wave 2 — and the DENOMINATOR, so the next mixed CSet can
            // price the walk PER REGION instead of treating its total as a
            // constant with respect to its own choice. Only from a pause that
            // actually walked something: a zero would make the per-region price
            // infinite on the next pause, and the fail-safe direction for a
            // budget is to keep the previous estimate rather than to refuse
            // every candidate.
            if phases.fixup_regions > 0 {
                let observed_regions = phases.fixup_regions as u64;
                let prev = self.fixup_regions_ema.load(Ordering::Relaxed);
                let next = if prev == 0 {
                    observed_regions
                } else {
                    prev.saturating_mul(7).saturating_add(observed_regions) / 8
                };
                self.fixup_regions_ema.store(next, Ordering::Relaxed);
            }
        }

        // LANE E wave 2 — what the YOUNG half of a collection set costs.
        //
        // Measured on young-only pauses, because those are the pauses in which
        // the young half IS the copying: on a mixed pause the closure covers
        // both halves and nothing separates them. `closure_us` rather than
        // `pause_us` so the estimate prices the copy and not the roots, the
        // rset walk or the fix-up — those are budgeted separately and charging
        // them twice would shrink the old CSet for no reason.
        //
        // Unconditional, like `fixup_ns_ema`: the estimate has to be warm
        // BEFORE `CRATONVM_G1_PAUSE_COST_MODEL` is armed, or the first mixed
        // pause after arming it budgets from a zero.
        //
        // LANE W4-B — and not from a drain pass, whose `closure_us` is the
        // cost of chasing one pause's self-forwarded residue with no to-space
        // left, not the cost of copying a young collection set. On the
        // reproducer that sample was 163 ms against a 249 ms real one from a
        // pause that copied twice as much: it does not even err in a
        // consistent direction, which is worse for an estimator than a bias.
        if teaches_controllers
            && collection_type == G1CollectionType::YoungOnly
            && phases.fused_closure_us() > 0
        {
            // gci i14/yt: the pre-split figure, so the parallel seed split is
            // reporting only (`G1PausePhases::fused_closure_us`).
            let observed = phases.fused_closure_us().saturating_mul(1_000);
            let prev = self.young_copy_ns_ema.load(Ordering::Relaxed);
            let next = if prev == 0 {
                observed
            } else {
                prev.saturating_mul(7).saturating_add(observed) / 8
            };
            self.young_copy_ns_ema.store(next, Ordering::Relaxed);        }

        // LANE W4-B — a drain pass does not re-size the young generation.
        //
        // This is the sharpest of the four, because `update_young_target` acts
        // on `pause_us` against the goal with no smoothing at all: one record
        // over the goal takes 20 % off the target immediately. On the
        // reproducer the drain pass's 232 ms against a 200 ms goal cut the
        // target from 76 regions to 60 — one fifth of the young generation,
        // removed by a record that is not a pause, in the one state where a
        // SMALLER young generation means more pauses and so more chances to
        // fail evacuation again. The containing young pause had already
        // applied its own (correct) adjustment a moment earlier.
        //
        // gci i8/gs: under `CRATONVM_G1_YOUNG_TARGET_NET_OF_HEAP_COSTS=1` the
        // loop is fed the pause net of the two phases no young size can
        // shorten (`verify_us`, `fixup_us`); see `young_target_sample_us`.
        //
        // gci i14/yt: the young census is a state fact, not a sample, so every
        // record that carries one writes it (drain passes too); under
        // `CRATONVM_G1_YOUNG_TARGET_HOTSPOT_MODEL=1` the target comes from the
        // predictors (`update_young_target_predicted`, fed the WHOLE pause --
        // the model separates the young-independent base itself, so
        // `NET_OF_HEAP_COSTS` does not apply on that arm).
        self.note_young_after_pause(collection_type, &phases);
        if teaches_controllers {
            if self.young_target_hotspot_model {
                self.update_young_target_predicted(collection_type, pause_us, stats, &phases);
            } else {
                let sample_us = young_target_sample_us(
                    pause_us,
                    &phases,
                    g1_young_target_net_of_heap_costs(),
                );
                self.update_young_target_base_aware(collection_type, pause_us, sample_us, stats, &phases);
            }
            self.note_young_pinned_kept(collection_type, &phases);
        }

        // LANE W5-A — what copying a YOUNG live byte costs on this host, for
        // this workload.
        //
        // Unconditional under the flag, and deliberately NOT gated on the
        // budget being armed, for the reason `young_copy_ns_ema` gives four
        // blocks up: an estimate that only starts warming when somebody arms
        // the thing that reads it is cold on the exact pause that first reads
        // it. The first mixed pause of a run is the largest one and it happens
        // before any mixed pause has measured anything — see
        // [`Self::old_evac_ns_per_byte`].
        //
        // `closure_us` is the numerator because that is the phase that copies.
        // `stats.bytes_copied` is the denominator because that is what the
        // closure moved. Mixed pauses do not come here: they take their sample
        // in the driver, through `update_evac_cost_for_mixed`, before
        // `phases.close` — folding the same pause in both places would give it
        // 2/8 of the weight instead of 1/8.
        //
        // LANE W4-B's `CRATONVM_G1_EVAC_COST_FROM_YOUNG` is the crude ancestor
        // of this block: it pointed a young pause's WHOLE-PAUSE ratio straight
        // at the OLD term. It is kept, unchanged, as the OFF arm's behaviour,
        // so a reader comparing wave 4's numbers against wave 5's is comparing
        // the same code wave 4 measured. With `CRATONVM_G1_EVAC_COST_REAL` on
        // it is inert by construction: a young pause writes the young term and
        // never the old one.
        if teaches_controllers && collection_type == G1CollectionType::YoungOnly {
            if evac_cost_real {
                self.fold_evac_cost(
                    &self.young_evac_ns_per_byte,
                    &self.young_evac_cost_samples,
                    phases.fused_closure_us().saturating_mul(1_000),
                    stats.bytes_copied,
                );
            } else if g1_evac_cost_from_young() {
                self.update_evac_cost(pause_us.saturating_mul(1_000), stats.bytes_copied);
            }
        }

        // F-18 — re-derive the tenuring threshold from the age histogram this
        // pause filled. Here rather than in each driver because every
        // evacuation path already funnels through this function, so a driver
        // added later cannot forget to do it — and a histogram that is filled
        // but never consumed grows without bound across pauses and makes every
        // threshold it does produce wrong.
        // LANE E: unconditional. The flag now gates only the THRESHOLD store
        // inside; the histogram must be consumed either way or it grows without
        // bound and `tenuring_state()` reports zeros forever. See the note in
        // `update_tenuring_threshold`.
        // gci i22/sz: a drain pass merges into its pause's table
        // (`update_tenuring_threshold_merging`).
        self.update_tenuring_threshold_merging(phases.drain_pass);

        // LANE E wave 2 — fold this pause's card-screen engagement into the
        // trend endpoints. Unconditional, and deliberately not behind any flag:
        // the question these answer ("does the screen's skip-rate decay over a
        // long run, and does `CRATONVM_G1_CARD_CLEAN` hold it up") is a
        // question ABOUT the flags, so an instrument that only exists when one
        // of them is set cannot answer it. Two relaxed loads and at most three
        // relaxed stores on a path that has already taken two locks.
        self.note_card_screen_engagement(&phases);

        // LANE E wave 2 — the wall-clock denominator, then the sizing policy.
        //
        // The order is load-bearing. `note_pause_wall_time` closes the interval
        // this pause ends, so the overhead figure `apply_heap_resize` reads
        // includes this pause; sizing on the PREVIOUS interval would act one
        // pause late, which on a collector that can take eighty pauses a second
        // is not a rounding error.
        //
        // Both are no-ops with `CRATONVM_G1_HEAP_RESIZE` unset except for the
        // `Instant::now()` and the one uncontended mutex in
        // `note_pause_wall_time`, which are kept unconditional so the overhead
        // figure is available to the `[GC-STAT]` line (and to anyone deciding
        // whether to arm the policy) without having armed it first.
        //
        // LANE W4-B — and a drain pass advances the clock without folding a
        // sample, for a reason that is arithmetic rather than judgement. The
        // overhead quantity is `pause / (pause + mutator)` over the interval
        // between two pause ENDS, and a drain pass ends inside the pause that
        // already closed the previous interval: there is no mutator time
        // between the two at all, so the ratio is ~1 000 000 ppm BY
        // CONSTRUCTION, whatever the workload does. On the reproducer the run
        // reports `gc_overhead_pct=77.03` with one drain pass in five records.
        // A number that a nested interval forces to its ceiling is not a
        // measurement of overhead.
        //
        // Advancing the clock and folding nothing is exactly the arm the first
        // pause of a process already takes — the interval is dropped from both
        // the numerator and the denominator, which leaves the ratio over the
        // remaining intervals undisturbed rather than tilting it either way.
        //
        // `apply_heap_resize` is deliberately NOT gated. It is not an
        // estimator of the next pause: it reads `phases`' own region census,
        // which a drain pass fills honestly (W3-C made it the fifth path to do
        // so), and the one reading that census admits under to-space
        // exhaustion is "occupancy is at its ceiling" — for which the policy's
        // answer is `Grow`, which is the answer this state wants. The shrink
        // arm cannot fire here: it needs a streak of pauses inside the LOW
        // occupancy band, and a drain pass resets that streak, which is also
        // correct.
        self.note_pause_wall_time(pause_us, teaches_controllers);
        let resized = self.apply_heap_resize(&phases);

        self.log_gc_event(collection_type, pause_us, stats, &phases);

        // Said at INFO, outside the `--verbose:gc` gate `log_gc_event` sits
        // behind, because a heap that changed size is a fact about the PROCESS
        // and not a GC statistic: an operator watching RSS move has to be able
        // to attribute it without having asked for per-pause logging in
        // advance. `tracing::debug!` would not do — `release_max_level_info`
        // compiles it out and no `RUST_LOG` value recovers it from a shipped
        // binary.
        if let Some((direction, target, released)) = resized {
            tracing::info!(
                "[GC] g1 heap-resize: {direction} target_regions={target} \
                 target_bytes={} committed={} released={released} gc_overhead_ppm={} \
                 grows={} shrinks={}",
                self.heap_target_bytes(),
                self.committed_bytes(),
                self.gc_overhead_ppm.load(Ordering::Relaxed),
                self.heap_grow_events.load(Ordering::Relaxed),
                self.heap_shrink_events.load(Ordering::Relaxed),
            );
        }
    }

    /// LANE E wave 2 — fold one pause's Phase-2 card-screen byte census into
    /// the first-window and rolling skip-rate figures the `[GC-STAT]` line
    /// renders.
    ///
    /// A pause that offered the screen no source regions carries no
    /// information about the screen and is skipped entirely, rather than being
    /// folded in as a 0 % skip-rate — `rset_regions_offered == 0` is "Phase 2
    /// had nothing to do", which is a different fact from "the screen refused
    /// every region", and averaging the two together is exactly how a decaying
    /// rate gets hidden behind a growing pile of empty pauses.
    pub fn note_card_screen_engagement(&self, phases: &G1PausePhases) {
        let scanned = phases.rset_bytes_scanned;
        let skipped = phases.rset_bytes_skipped;
        let offered = scanned.saturating_add(skipped);
        if phases.rset_regions_offered == 0 || offered == 0 {
            return;
        }
        let observed = skipped.saturating_mul(1_000_000) / offered;
        let prev = self.card_skip_rate_ppm.load(Ordering::Relaxed);
        let next = if prev == 0 {
            observed
        } else {
            prev.saturating_mul(7).saturating_add(observed) / 8        };
        self.card_skip_rate_ppm.store(next, Ordering::Relaxed);

        // The first window is frozen by the COLLECTION COUNT, not by a
        // dedicated counter, so it cannot drift from the pause numbering an
        // operator reads off the same log line.
        if self.collection_count.load(Ordering::Relaxed) <= G1_CARD_TREND_WINDOW {
            self.card_first_window_scanned
                .fetch_add(scanned, Ordering::Relaxed);
            self.card_first_window_skipped
                .fetch_add(skipped, Ordering::Relaxed);
        }
    }

    /// `(scanned, skipped, rate_ppm_overall, rate_ppm_first_window,
    /// rate_ppm_recent)` — the card screen's byte engagement, cumulative and as
    /// a trend. Diagnostics, `print_gc_summary` and tests.
    pub fn card_screen_skip_rates(&self) -> (u64, u64, u64, u64, u64) {
        let scanned = self.card_bytes_scanned.load(Ordering::Relaxed);
        let skipped = self.card_bytes_skipped.load(Ordering::Relaxed);
        let rate = |sc: u64, sk: u64| -> u64 {
            let total = sc.saturating_add(sk);
            if total == 0 {
                0
            } else {
                sk.saturating_mul(1_000_000) / total            }        };
        (
            scanned,
            skipped,
            rate(scanned, skipped),
            rate(
                self.card_first_window_scanned.load(Ordering::Relaxed),
                self.card_first_window_skipped.load(Ordering::Relaxed),
            ),
            self.card_skip_rate_ppm.load(Ordering::Relaxed),
        )
    }

    /// Log a GC event if logging is enabled.
    pub fn log_gc_event(
        &self,
        collection_type: G1CollectionType,
        pause_us: u64,
        stats: &GcStats,
        phases: &G1PausePhases,
    ) {
        if !self.gc_log_enabled.load(Ordering::Relaxed) {
            return;
        }
        // Two sinks:
        //  - `tracing::info!` keeps the legacy `[GC <Type>] ...` line for
        //    `RUST_LOG=cratonvm_gc=info` consumers and the G1-selection guard
        //    (which greps for `[GC YoungOnly]`); pause is now microseconds.
        //  - a structured `[GC-STAT]` line straight to stderr so the pause +
        //    bytes are visible with JUST `--verbose:gc` (no RUST_LOG needed)
        //    and trivially parseable by the gauntlet runner (§7 item 6).
        tracing::info!(
            "[GC {:?}] pause={pause_us}us copied={} bytes_copied={} freed={}",
            collection_type,
            stats.objects_copied,
            stats.bytes_copied,
            stats.bytes_freed,
        );
        // LANE E (G1-7, 2026-09-20) — the pause's own DECISIONS, beside its
        // costs.
        //
        // The phase breakdown says where a pause went and the region census
        // says what it bought, but neither says what the collector CHOSE, and
        // every G1 sizing complaint reduces to a choice: how big the young
        // generation was allowed to get, what pause goal that was measured
        // against, at what age survivors stopped being copied, and how much of
        // the goal was left for the old half of a mixed collection set after
        // the fix-up walk had spent its share. Those are four adaptive
        // controllers (`update_young_target`, `update_tenuring_threshold`,
        // `old_cset_copy_budget_ns`, `update_evac_cost`) whose state was
        // printed only at SHUTDOWN, and only for two of them — so a pause storm
        // in the middle of a run could not be attributed to the policy that
        // caused it, and a policy that never moved could not be told from one
        // that moved and moved back.
        //
        // Read AFTER `update_young_target` and `update_tenuring_threshold`
        // have run for this pause (see the call order in
        // `record_collection_with_phases`), so these are the values the NEXT
        // pause will be sized by — which is the actionable direction: the line
        // says what the collector concluded, not what it assumed.
        //
        // All atomic loads and pure config arithmetic. No lock is taken: every
        // production caller reaches this holding the regions guard as a WRITER,
        // which is the same reason the `try_read` census further down is
        // structurally dead.
        let (young_min, young_max) = self.young_region_bounds();
        let young_target = self.young_target_regions.load(Ordering::Relaxed);
        let decision_note = format!(
            // LANE W5-A — the four fields after `mark_threshold` are APPENDED
            // rather than woven in beside `evac_ns_per_byte`, so a scraper
            // written against the wave-4 line still parses this one.
            //
            // `evac_samples` is the field the whole finding turns on. `4` is a
            // plausible-looking calibration and it is what this collector
            // printed on every pause of every run for four waves while nothing
            // had ever written it; the sample count is the only thing on the
            // line that can tell "settled" from "never measured". `evac_price`
            // is what the next mixed collection set will ACTUALLY be charged
            // per live byte — which is `evac_ns_per_byte` until the young
            // bootstrap applies, and then is not.
            " goal_us={} young_target={young_target} young_bounds={young_min}/{young_max} young_now={} free_pool={} free_trigger_pct={} tenuring={}/{} survivor_target={} evac_ns_per_byte={} fixup_ns_ema={} old_budget_ns={} mixed_remaining={} old_gen_bytes={} mark_threshold={} evac_samples={}/{} young_evac_ns_per_byte={} evac_price_ns_per_byte={} evac_samples_refused={}",
            self.config.max_gc_pause_ms.saturating_mul(1_000),
            self.young_region_count.load(Ordering::Relaxed),
            self.free_region_count.load(Ordering::Relaxed),
            self.needs_gc_free_percent.load(Ordering::Relaxed),
            self.tenuring_threshold(),
            self.max_tenuring_threshold(),
            self.survivor_target_bytes(),
            self.evac_ns_per_byte.load(Ordering::Relaxed),
            self.fixup_ns_ema.load(Ordering::Relaxed),
            self.old_cset_copy_budget_ns(),
            self.mixed_gc_remaining.load(Ordering::Relaxed),
            self.old_gen_bytes.load(Ordering::Relaxed),
            self.marking_threshold_bytes.load(Ordering::Relaxed),
            self.evac_cost_samples.load(Ordering::Relaxed),
            self.young_evac_cost_samples.load(Ordering::Relaxed),
            self.young_evac_ns_per_byte.load(Ordering::Relaxed),
            self.old_evac_ns_per_byte(),
            self.evac_cost_samples_refused.load(Ordering::Relaxed),
        );
        // LANE E wave 2 — the CUMULATIVE card-screen engagement, and its trend.
        //
        // The per-pause `rset_scanned` / `rset_skipped` pair below is already
        // on this line, and it is not sufficient for the question that actually
        // hangs on these numbers. G1's card table is additive-only: it
        // saturates by construction, so the screen's skip-rate decays over a
        // run, and cleaning (`CRATONVM_G1_CARD_CLEAN`) is the only thing that
        // pushes back. Whether cleaning is worth its cost is therefore a
        // question about the SHAPE of that decay, which no single pause's delta
        // can show and which the run totals could not show either until they
        // were printed somewhere a process that ends in `System.exit` still
        // reaches. `first` is the rate over the run's first
        // `G1_CARD_TREND_WINDOW` instrumented pauses, `now` the 7/8-smoothed
        // recent rate: `now` well below `first` IS the saturation, happening.
        let (card_scanned, card_skipped, card_rate, card_rate_first, card_rate_now) =
            self.card_screen_skip_rates();
        let card_note = format!(
            " card_scanned_total={card_scanned} card_skipped_total={card_skipped} card_skip_rate_pct={:.2} card_skip_rate_first_pct={:.2} card_skip_rate_now_pct={:.2} card_clean={}",
            card_rate as f64 / 10_000.0,
            card_rate_first as f64 / 10_000.0,
            card_rate_now as f64 / 10_000.0,
            gc_flags().g1_card_clean,
        );
        // LANE E wave 2 — and what the sizing policy decided. Empty unless
        // `CRATONVM_G1_HEAP_RESIZE` is armed, so the flag-off line is byte for
        // byte what it was.
        // gci i13/ef: plus the two i13/ef opt-ins' run totals, empty unless
        // one of them is on (the flag-off line is unchanged).
        let resize_note = format!("{}{}", self.heap_resize_note(), self.gci_i13_ef_note());
        // G1AUD-10: the phase breakdown rides on the same line. `fixup` is
        // the one that scales with the LIVE HEAP rather than the collection
        // set, so its share is printed beside its own denominator (regions and
        // bytes walked) — a long fix-up on a big old generation and a long
        // fix-up on a small one are different problems.
        let phase_note = if *phases == G1PausePhases::default() {
            String::new()
        } else {
            format!(
                // LANE W3-C — `drain_pass` FIRST among the census fields,
                // because it changes how every number after it must be read:
                // a drain pass's `pause_us` is an interval nested inside a
                // young pause that already printed its own line, so a scraper
                // summing `[GC-STAT] pause_us` over a run double-counts unless
                // it can see this. See [`G1PausePhases::drain_pass`].
                " roots_us={} rset_us={} closure_us={} fixup_us={} free_us={} verify_us={} other_us={} fixup_regions={} fixup_bytes={} rset_regions={}/{} rset_scanned={} rset_skipped={} drain_pass={} free_regions={} eden_regions={} surv_regions={} old_regions={} hum_regions={} cset_regions={} jni_pinned_out={} jit_pinned_out={}",
                phases.roots_us,
                phases.rset_us,
                phases.closure_us,
                phases.fixup_us,
                phases.free_us,
                phases.verify_us,
                phases.other_us,
                phases.fixup_regions,
                phases.fixup_bytes,
                // F-05 engagement: skipped-of-offered regions, then the bytes
                // the source walk visited vs. the bytes the card screen let it
                // step over. Both halves are printed so a zero is readable:
                // `rset_skipped=0` with a non-zero `rset_scanned` is "the
                // screen ran and refused", not "the screen is off".
                phases.rset_regions_skipped,
                phases.rset_regions_offered,
                phases.rset_bytes_scanned,
                phases.rset_bytes_skipped,
                phases.drain_pass,
                phases.free_regions,
                phases.eden_regions,
                phases.surv_regions,
                phases.old_regions,
                phases.hum_regions,
                phases.cset_regions,
                phases.jni_pinned_out,
                phases.jit_pinned_out,
            )
            .replace("                 ", "")
        };
        // LANE E wave 2, small finding 4 — the `try_read` region census that
        // used to hang off this call is GONE.
        //
        // It was structurally dead and its own comment said so: every
        // collection path reaches this emitter holding the regions guard as a
        // WRITER, `try_read` therefore always returned `None`, and the only
        // caller of this function is a collection. What it printed instead was
        // the `old_bytes` half, which `decision_note` above already carries
        // unconditionally — so the flag arm rendered a strict subset of the
        // default line. The six numbers it was armed to print are on
        // `phase_note` (`G1PausePhases::free_regions` and friends), taken
        // inside the pause where the table is already borrowed. An instrument
        // armed where it cannot fire is worse than no instrument, because a
        // zero from it reads as a measurement.
        eprintln!(
            "[GC-STAT] type={:?} pause_us={pause_us} objects_copied={} bytes_copied={} bytes_freed={}{phase_note}{decision_note}{card_note}{resize_note}",
            collection_type,
            stats.objects_copied,
            stats.bytes_copied,
            stats.bytes_freed,
        );
    }

    /// Snapshot the bounded pause ring (most-recent window) for offline
    /// percentile/throughput analysis.
    pub fn pause_history_snapshot(&self) -> Vec<G1PauseRecord> {
        self.pause_history.lock().iter().copied().collect()
    }

    /// Reduce the recorded pauses to per-type p50/p99/max (§5 acceptance
    /// metric). Returns `None` when nothing has been collected yet.
    pub fn pause_summary(&self) -> Option<G1PauseSummary> {
        let hist = self.pause_history.lock();
        if hist.is_empty() {
            return None;
        }
        let reduce = |kind: G1CollectionType| -> G1PausePercentiles {
            let mut samples: Vec<u64> = hist
                .iter()
                // LANE W3-C — a drain pass is not a pause. It is recorded as
                // `YoungOnly` by `drain_kept_self_forwards`, and
                // `retry_after_evacuation_failure` calls that up to eight
                // times INSIDE a young pause which has already recorded
                // itself, so including them made this a population of nine
                // "pauses" per one stop of the world, eight of them nested
                // intervals of the first. They are reported separately below.
                // See [`G1PausePhases::drain_pass`].
                .filter(|r| !r.phases.drain_pass)
                .filter(|r| r.collection_type == kind)
                .map(|r| r.pause_us)
                .collect();
            if samples.is_empty() {
                return G1PausePercentiles::default();
            }
            samples.sort_unstable();
            let count = samples.len();
            let total_us: u64 = samples.iter().sum();
            // Nearest-rank percentile: index ceil(p/100 * N) - 1, clamped.
            let pct = |p: usize| -> u64 {
                let rank = ((p * count) + 99) / 100; // ceil(p*N/100)
                let idx = rank.saturating_sub(1).min(count - 1);
                samples[idx]
            };
            G1PausePercentiles {
                count: count as u64,
                total_us,
                p50_us: pct(50),
                p99_us: pct(99),
                max_us: *samples.last().unwrap(),
            }
        };
        let drain_passes = hist.iter().filter(|r| r.phases.drain_pass).count() as u64;
        let drain_pass_us: u64 = hist
            .iter()
            .filter(|r| r.phases.drain_pass)
            .map(|r| r.pause_us)
            .sum();
        Some(G1PauseSummary {
            young: reduce(G1CollectionType::YoungOnly),
            mixed: reduce(G1CollectionType::Mixed),
            dropped: self.pause_history_dropped.load(Ordering::Relaxed),
            drain_passes,
            drain_pass_us,
        })
    }

    /// Print the aggregate pause summary to stderr (the §5 p50/p99 table). A
    /// no-op when no collection has run. Emitted at VM shutdown when GC stats
    /// are requested — see `VmHeap::print_gc_summary`.
    pub fn print_gc_summary(&self) {
        // THE PROCESS-STATIC GUARD COUNTERS ARE NOT HERE ANY MORE.
        //
        // `evac_ref_rejected`, `non_object_roots_skipped`, the
        // `young evacuation: parallel=/serial=` pair,
        // `implausible_legacy_headers`, `copy_shape_drift`,
        // `flat_walk_refused_array` and `kept_seed_rejected` moved to
        // `gc_metrics::collector_decision_report`, which is emitted on BOTH
        // shutdown arms. They used to be printed here with comments saying
        // they were "unconditional ... before the early return" so that a zero
        // could be cited -- and on every JUnit workload in the suites the zero
        // was never printed at all, because this function is reached only from
        // `vm-cli`'s normal-return teardown and `JUnitCore` ends in
        // `System.exit`, which never unwinds Rust frames. See the note above
        // `maybe_dump_shutdown_reports` in `vm-cli/src/main.rs`.
        //
        // What is left below needs `&self`, so it stays -- and the SAME hook
        // now reaches it through a `Weak<SharedVm>` stashed at VM
        // construction, guarded so the normal arm cannot print it twice.
        // F-15 / F-16 / F-18 — the state the three adaptive policies ended the
        // run in. Unconditional and before the `pause_summary` early return: a
        // policy whose state nothing prints cannot be cited, and each of these
        // is a decision the collector made on its own that an operator would
        // otherwise have to infer from the outcome.
        // g1gen/c6: this collector's own evacuation pool, the generational
        // heap's `[GC] evac_pool:` fields plus the dispatch-while-busy
        // fail-safe (a MUST-BE-ZERO; it was `pool_dispatch_while_busy=` on the
        // process-wide `[GC] g1 evac-workers:` line, read from a process
        // static every heap's pool fed).
        {
            let census = self.evac_pool_census();
            let line = cratonvm_gc_core::evac_pool::evac_pool_census_line(census);
            eprintln!(
                "[GC] g1 evac-pool:{} pool_dispatch_while_busy={}",
                line.strip_prefix("[GC] evac_pool:").unwrap_or(&line),
                census.map_or(0, |c| c.dispatch_while_busy),
            );
        }
        eprintln!(
            "[GC] g1 heap: reserved={} committed={} backing={}",
            self.reserved_bytes(),
            self.committed_bytes(),
            if self.heap_is_reserved() {
                "reserved-on-demand"
            } else {
                "fully-committed"
            },
        );
        // LANE W2-C touched this line only to keep it compiling against the two
        // fields `ihop_model_state` grew (disengaged cycles, back-off
        // suppressions). Both belong here for the same reason the rest of the
        // row does: an operator watching G1 fail to reclaim old gen has to be
        // able to tell "the model is asking for this threshold" from "the model
        // folded nothing and the static number is what you are getting".
        let (mark_ms, alloc_kib_per_ms, headroom_pct, late, disengaged, backoff) =
            self.ihop_model_state();
        // LANE W3-C touched this line for three things, all of them
        // §7.1-of-`orchestrator-wave-1-measurements.md` problems:
        //
        //  * `backoff_suppressions` is renamed `backoff_declined_polls`, with
        //    `ihop_polls` beside it, because it counts CONSULTS declined and
        //    not cycles declined — see [`IHOP_POLLS`]. W2-C's own A/B recipe
        //    reads it as cycles.
        //  * `untimed_cycles` is the second disengagement route
        //    `disengaged_cycles` structurally could not see (see
        //    [`IHOP_UNTIMED_CYCLES`]).
        //  * The two flags that govern the numbers on this line are now
        //    PRINTED. `backoff_declined_polls=0` on a build with the back-off
        //    off is a tautology, and until this it was indistinguishable from
        //    "the back-off is armed and never had to fire" — which is exactly
        //    the reading §7.1 is about. The rule that came out of §7.1 is that
        //    a measurement reports what the switch it moved resolved to.
        let (ihop_polls, untimed) = ihop_poll_census();
        // LANE W5-C adds two tokens, both for the §7.3 reason the wave-3 ones
        // were added: `backoff_deadline_releases` is how many times the new
        // fail-safe actually fired, and `backoff_deadline` is what its switch
        // resolved to — without the second, a zero in the first is
        // indistinguishable from a flag that was never set.
        let deadline_releases = self.mark_backoff_deadline_releases();
        eprintln!(
            "[GC] g1 ihop: threshold={} ceiling={} mark_ms={mark_ms} old_growth_kib_per_ms={alloc_kib_per_ms} headroom_pct={headroom_pct} to_space_exhausted={late} disengaged_cycles={disengaged} untimed_cycles={untimed} ihop_polls={ihop_polls} backoff_declined_polls={backoff} backoff_deadline_releases={deadline_releases} adaptive={} gross_growth={} backoff_enabled={} backoff_deadline={}",
            self.marking_threshold_bytes(),
            self.ihop_static_ceiling(),
            gc_flags().g1_adaptive_ihop,
            gc_flags().g1_ihop_gross_growth,
            gc_flags().g1_ihop_backoff,
            gc_flags().g1_mark_backoff_deadline,
        );
        // LANE W6-B — a line of its own, because it answers a different
        // question from the one above: that line says how often the gate was
        // consulted, this one says what a consultation COST. `fast_declines`
        // is the engagement count — zero with the flag on means the fix never
        // ran and the arm's wall clock says nothing — and `residue_per_thread`
        // is the exactness the batching trades away, printed rather than left
        // to be discovered in a doc comment.
        eprintln!(
            "[GC] g1 ihop-poll-gate: poll_gate={} fast_declines={} flush_batch={} residue_per_thread<={}",
            gc_flags().g1_ihop_poll_gate,
            self.backoff_fast_declines(),
            IHOP_POLL_FLUSH_BATCH,
            IHOP_POLL_FLUSH_BATCH - 1,
        );
        // LANE E wave 2 — the sizing policy's run totals, beside the heap line
        // they explain. A grow count and a shrink count that are both large and
        // nearly equal is the hysteresis being too weak, and that is the
        // failure mode no single pause's line can show.
        let (grows, shrinks, uncommitted, overhead_ppm) = self.heap_resize_counts();
        eprintln!(
            "[GC] g1 heap-resize: enabled={} target_regions={} target_bytes={} floor_regions={} grows={grows} shrinks={shrinks} uncommitted_bytes={uncommitted} gc_overhead_pct={:.2}",
            g1_heap_resize_enabled(),
            self.heap_target_regions(),
            self.heap_target_bytes(),
            self.heap_floor_regions(),
            overhead_ppm as f64 / 10_000.0,
        );
        let (fc, fp, fw, cc, cp, cw) = free_region_scan_counts();
        eprintln!(
            "[GC] g1 free-scan: single(calls={fc} probed={fp} worst={fw}) contiguous(calls={cc} probed={cp} worst={cw}) regions={}",
            self.region_table_len(),
        );
        // LANE E wave 2 (proposal P6) — humongous allocation outcomes. The
        // failure SHAPE is the point: a non-zero `failures` with a comfortable
        // `free_at_failure` and a `longest_run_at_failure` shorter than the
        // span requested is FRAGMENTATION, which no amount of extra collection
        // can fix because this collector has no compaction pass. Before these
        // counters that case was indistinguishable, in every log the VM emits,
        // from plain exhaustion.
        let (hum_req, hum_fail, hum_free, hum_run) = self.humongous_alloc_counts();
        eprintln!(
            "[GC] g1 humongous: requests={hum_req} failures={hum_fail} free_at_failure={hum_free} longest_run_at_failure={hum_run} best_fit={} run_guard={}",
            true,
            true,
        );
        // gci nd48: small fallible requests the TLAB reserve served on a
        // collection's reclaim credit, and the ones it refused.
        let (credit_claims, credit_refusals) = self.reserve_credit_counts();
        eprintln!(
            "[GC] g1 reserve-credit: claims={credit_claims} refusals={credit_refusals} enabled={} last_chance_claims={}",
            true,
            self.reserve_last_chance_claims(),
        );
        // Card cleaning — the engagement census. `cleaned` near zero across a
        // run means the cards are saturating exactly as they did before this
        // existed, and the byte skip-rate beside it is the consequence.
        let cleaned = self.card_cards_cleaned.load(Ordering::Relaxed);
        let kept = self.card_cards_kept.load(Ordering::Relaxed);
        let scanned = self.card_bytes_scanned.load(Ordering::Relaxed);
        let skipped = self.card_bytes_skipped.load(Ordering::Relaxed);
        eprintln!(
            "[GC] g1 card-clean: enabled={} cards_cleaned={cleaned} cards_kept={kept} \
             bytes_scanned={scanned} bytes_skipped={skipped} skip_rate={:.2}%",
            gc_flags().g1_card_clean,
            if scanned + skipped == 0 {
                0.0
            } else {
                skipped as f64 * 100.0 / (scanned + skipped) as f64
            },
        );
        // W4-A — the block-offset table's engagement census.
        //
        // Its own line rather than four more fields on the one above, because
        // it answers a different question. `card-clean`'s skip-rate is about
        // objects the walk SIZED and then stepped over; these are bytes it
        // never looked at, and mixing them would inflate the skip-rate that
        // `CRATONVM_G1_CARD_CLEAN`'s default is decided on with work a
        // different feature did.
        //
        // `enabled=` is printed beside the numbers on purpose. §7.1 of
        // `orchestrator-wave-1-measurements.md` retracted a whole measurement
        // because a flag resolved to the opposite of what the arm's name said
        // and nothing on the output surface could have revealed it.
        let bo_jumps = self.block_jumps_taken.load(Ordering::Relaxed);
        let bo_bytes = self.block_bytes_jumped.load(Ordering::Relaxed);
        let bo_refused = self.block_entries_refused.load(Ordering::Relaxed);
        let bo_tails = self.block_tail_breaks.load(Ordering::Relaxed);
        let bo_tail_bytes = self.block_tail_bytes.load(Ordering::Relaxed);
        // W5-B — the ENFORCEMENT census, on the same line as the engagement it
        // qualifies.
        //
        // `unvouched` counts producers that named an address whose sixteen
        // bytes do not carry `GC_FLAG_HEADER`; `disarmed` says whether one of
        // them has turned the jump off for the rest of the process. A run with
        // `jumps=0 disarmed=true` and a run with `jumps=0` because the heap had
        // nothing to skip are completely different facts and a bare jump count
        // cannot tell them apart.
        //
        // `audited` and `audit_violations` are the sampled reader audit
        // (`CRATONVM_G1_BLOCK_OFFSET_AUDIT=N`, one jump in N), and BOTH are
        // printed for the reason `SWEEP_NO_HEADER_FLAG` gives for its own:
        // zero violations out of zero audited objects means the audit never
        // ran, which is not the same claim as the audit running clean.
        // `audit_period=0` says which of the two it is without arithmetic.
        let (bo_vouches, bo_unvouched, bo_disarmed) =
            crate::g1_cards::block_offset_enforcement_census();
        let (bo_spans, bo_audited, bo_bad) = block_offset_audit_census();
        eprintln!(
            "[GC] g1 block-offsets: enabled={} jumps={bo_jumps} bytes_jumped={bo_bytes} \
             tail_breaks={bo_tails} tail_bytes={bo_tail_bytes} \
             entries_refused={bo_refused} bytes_per_jump={:.0} \
             vouch_checks={bo_vouches} unvouched={bo_unvouched} disarmed={bo_disarmed} \
             audit_period={} audit_spans={bo_spans} audited={bo_audited} audit_violations={bo_bad}",
            lane_a_block_offsets(),
            if bo_jumps == 0 {
                0.0
            } else {
                bo_bytes as f64 / bo_jumps as f64
            },
            lane_a_block_offset_audit(),
        );
        // LANE E wave 2 — the card screen's skip-rate as a TREND, not a total.
        //
        // The line above has carried the cumulative rate since F-05 and it
        // cannot answer the question that decides `CRATONVM_G1_CARD_CLEAN`'s
        // default. G1's card table is additive-only, so it saturates by
        // construction and the screen decays toward useless over a long run;
        // cleaning is what pushes back. "Does the rate decay, and does cleaning
        // hold it up" is a question about the SHAPE of the decay, and a run
        // total averages the shape away. `first` is the rate over the run's
        // first `G1_CARD_TREND_WINDOW` instrumented pauses; `now` is the
        // 7/8-smoothed recent rate, so `now` well below `first` IS the
        // saturation, happening.
        //
        // Both numbers are also on every `[GC-STAT]` line, which is the sink
        // that matters: this function is reached only from `vm-cli`'s
        // normal-return teardown, and a workload that ends in `System.exit`
        // never unwinds to it.
        let (_, _, rate_all, rate_first, rate_now) = self.card_screen_skip_rates();
        eprintln!(
            "[GC] g1 card-screen trend: window={G1_CARD_TREND_WINDOW} skip_rate_all={:.2}% skip_rate_first={:.2}% skip_rate_now={:.2}% decayed={}",
            rate_all as f64 / 10_000.0,
            rate_first as f64 / 10_000.0,
            rate_now as f64 / 10_000.0,
            rate_first > 0 && rate_now < rate_first,
        );
        // LANE W6-A — the card TABLE's state, beside the card SCREEN's rate.
        //
        // The line above is about the walk: what fraction of the bytes it was
        // asked to scan it managed to skip. This one is about the structure the
        // walk consults: how much of the card table is still capable of
        // screening anything, and how much of the block-offset table exists at
        // all. They read as the same subject and they are not, which is why a
        // 99.91% skip rate could be published for four waves beside a card
        // table whose occupancy nothing had ever printed.
        //
        // See `card_table_census_line` for the argument per field.
        eprintln!("{}", self.card_table_census_line());
        let (tenuring, hist) = self.tenuring_state();
        eprintln!(
            "[GC] g1 tenuring: threshold={tenuring} effective={} configured={} survivor_target={} ages={:?}",
            self.tenuring_threshold(),
            self.max_tenuring_threshold(),
            self.survivor_target_bytes(),
            hist,
        );
        // LANE E (2026-09-20) — the young sizing controller's end state.
        //
        // `update_young_target` is the policy that decides how big a young
        // generation a pause is allowed to collect, and it is also half of
        // `needs_gc`. Nothing printed it: an operator watching a pause storm
        // could see the IHOP state and the tenuring state at shutdown and had
        // to INFER the young target from pause outcomes. `target == max` is the
        // ceiling, which is also the state in which the young half of the
        // trigger is deliberately inert (see `needs_gc`), so the distinction
        // between "the goal was never missed" and "the goal was missed and the
        // target came back" is exactly what this line carries.
        let (ymin, ymax) = self.young_region_bounds();
        eprintln!(
            // LANE W4-B — `young_copy_ns_ema` joins its two siblings, and the
            // drain-pass decision is reported beside the numbers it governs.
            //
            // The budget `old_budget_ns` is the goal minus BOTH EMAs under
            // `CRATONVM_G1_PAUSE_COST_MODEL`, so printing one of the two
            // subtrahends and hiding the other made the line unable to explain
            // its own last field. And `drain_samples_refused` is the
            // denominator that says whether this run exercised the drain
            // exclusion at all: a zero means both arms of the A/B would have
            // produced this same target, which is a different statement from
            // "the flag did nothing".
            "[GC] g1 young-sizing: target={} bounds={ymin}/{ymax} at_ceiling={} now={} goal_us={} evac_ns_per_byte={} fixup_ns_ema={} young_copy_ns_ema={} old_budget_ns={} drain_samples_refused={} drain_not_a_pause={} evac_cost_from_young={} enabled={}",
            self.young_target_regions(),
            self.young_target_regions() >= ymax,
            self.young_region_count(),
            self.config.max_gc_pause_ms.saturating_mul(1_000),
            self.evac_ns_per_byte.load(Ordering::Relaxed),
            self.fixup_ns_ema.load(Ordering::Relaxed),
            self.young_copy_ns_ema.load(Ordering::Relaxed),
            self.old_cset_copy_budget_ns(),
            drain_controller_samples_refused(),
            true,
            // LANE W4-B — `evac_ns_per_byte` sits two fields to the left on
            // this same line, and until this switch exists a reader has no way
            // to tell "the calibration settled at 4" from "the calibration was
            // never written". On a young-only run with the switch off, it is
            // always the second.
            g1_evac_cost_from_young(),
            gc_flags().g1_young_pause_target,
        );
        // gci i14/yt — the HotSpot-shaped young model's predictors, only when
        // it sized this run (`CRATONVM_G1_YOUNG_TARGET_HOTSPOT_MODEL=1`).
        if self.young_target_hotspot_model {
            let (base_us, ps_per_byte, surv_per_region, surv_equiv, after_pause) =
                self.young_model_state();
            // gci i16/pl: the survivor split's state appended (finding 12).
            let (surv_permille, _, _, surv_last, split_samples) = self.young_model_split_state();
            eprintln!(
                "[GC] g1 young-model: hotspot=on base_us={base_us} work_ps_per_byte={ps_per_byte} surv_bytes_per_region={surv_per_region} region_ns={} survivor_equiv={surv_equiv} young_after_pause={after_pause} surv_survival_permille={surv_permille} surv_term_us={} split_samples={split_samples}",
                young_region_cost_ns(surv_per_region, ps_per_byte),
                survivor_copy_us(surv_last, surv_permille, ps_per_byte),
            );
        }
        // gci i14/yt — the Survivor PLAB census (`CRATONVM_G1_SURVIVOR_PLAB`).
        let (plab_regions, plab_chunks, plab_tail) = self.survivor_plab_census();
        if plab_chunks > 0 || true {
            // gci i16/pl: direct allocations appended (finding 4).
            let (direct_objects, direct_bytes) = self.survivor_plab_direct_census();
            eprintln!(
                "[GC] g1 survivor-plab: enabled={} regions={plab_regions} chunks={plab_chunks} tail_filler_bytes={plab_tail} plab_bytes={SURVIVOR_PLAB_BYTES} direct_objects={direct_objects} direct_bytes={direct_bytes} switch_waits={} copy_end_elide={}",
                true,
                self.survivor_plab_switch_waits(),
                true,
            );
        }
        // gci i9/fa — where the pauses went, summed by phase, beside the two
        // decaying estimates above; and, under `CRATONVM_G1_DBG_PINS`, which
        // publisher held the pinned regions out. See `G1PhaseTotals`.
        eprintln!("{}", self.phase_totals.line());
        // g1sz: the pauses' thread CPU (the collecting thread plus the
        // evacuation pool's workers, `gc_thread_cpu_ns`), which a loaded host
        // inflates far less than the phases' wall clocks.
        {
            let (parallel, conc) = self.gc_thread_cpu_ns();
            let pool = self.evac_pool.get().map_or(0, cratonvm_gc_core::evac_pool::EvacPool::worker_cpu_ns);
            eprintln!(
                "[GC] g1 gc-cpu: parallel_us={} pool_workers_us={} conc_mark_us={}",
                parallel / 1000,
                pool / 1000,
                conc / 1000,
            );
        }
        if let Some(line) = self.pin_attribution.line() {
            eprintln!("{line}");
        }
        // LANE W5-A — the copy-cost calibration's own line, because the line
        // above cannot carry it honestly.
        //
        // W4-B's comment three fields up is the whole finding: `evac_ns_per_byte`
        // printed alone cannot say whether it was ever written. W4-B answered
        // that by printing the SWITCH beside it, which tells a reader what the
        // build would have done; this prints the SAMPLE COUNTS, which tell a
        // reader what the run actually did. `old=…/0` is "the mixed copy budget
        // priced every candidate region with a compile-time guess", and it is
        // the reading on every young-only run — that is, on most runs.
        //
        // `price` is the number the next mixed collection set will really be
        // charged per live byte. It differs from `old` exactly when the young
        // bootstrap is standing in, so `price != old` is itself the readout for
        // "this run has not yet measured an old evacuation".
        //
        // `refused` is the fail-safe's counter: samples dropped because the
        // pause copied less than `G1_EVAC_COST_MIN_SAMPLE_BYTES` and so carried
        // no information about cost per byte. A run with `young=0/0` and a
        // large `refused` did not fail to instrument anything — it genuinely
        // never copied enough in one pause to measure, which for a humongous
        // workload is the truth and not a bug.
        let (evac_old, evac_old_n, evac_young, evac_young_n, evac_refused, evac_price) =
            self.evac_cost_state();
        eprintln!(
            "[GC] g1 evac-cost: price_ns_per_byte={evac_price} old={evac_old}/{evac_old_n} young={evac_young}/{evac_young_n} refused={evac_refused} min_sample_bytes={} default={} real={}",
            G1_EVAC_COST_MIN_SAMPLE_BYTES,
            G1_EVAC_NS_PER_BYTE_DEFAULT,
            g1_evac_cost_real(),
        );
        // LANE E — TLAB refills whose request was clamped to `region_size / 2`
        // rather than refused. A non-zero count is the adaptive TLAB sizer
        // having climbed past half a region; it used to be the point at which
        // that thread silently lost its TLAB for good. See `refill_tlab`.
        //
        // LANE W6-A — `tail_sinks=` and `sink_tail_bytes=`, and why they are
        // HERE rather than only on `[GC] tlab-waste:`.
        //
        // `tlab::tlab_census_lines()` already carries `sink_tail=`, and W6-A's
        // first attempt put the sink COUNT beside it there. That line is
        // emitted from `VmHeap::print_gc_summary`'s **`Generational` arm
        // only** — deliberately, so "a G1 or ZGC summary is unchanged byte for
        // byte" — so on the collector this whole round is about, the TLAB
        // census does not print at all and a number wired to it is wired to a
        // report a G1 run never reaches. That is the P7 shape one level up, and
        // catching it is the reason this lane's brief says to run the binary
        // and quote the line rather than assert that it exists.
        //
        // So the pair lands on G1's own TLAB line, which G1 does print. It is
        // a PAIR on purpose: `reclaim_tail_via_sinks` opens with
        // `if TAIL_SINK_COUNT == 0 { return false }`, so with no sink
        // registered `sink_tail_bytes` is zero by construction and no retire
        // ever reaches a sink to decline. "Nothing offered to take tails back"
        // and "the sinks declined every tail" are the same printed zero and
        // different facts. `tlab_tail_sink_count` existed to separate them and
        // had no caller anywhere in the workspace.
        //
        // The fact that the process-wide TLAB census is reported nowhere on a
        // G1 or ZGC run is larger than these two fields and is NOT fixed here:
        // `VmHeap::print_gc_summary` is not this lane's printer. Filed in
        // `w6a-twenty-nine-accessors.md` §2.2.
        eprintln!(
            "[GC] g1 tlab: oversize_clamped={} clamp_enabled={} cap={} \
             tail_sinks={} sink_tail_bytes={}",
            TLAB_OVERSIZE_CLAMPED.load(Ordering::Relaxed),
            true,
            self.config.region_size / 2,
            cratonvm_gc_core::tlab::tlab_tail_sink_count(),
            cratonvm_gc_core::tlab::tlab_waste_stats().sink_tail_bytes,
        );
        // LANE W3-C — the concurrent marker's PER-WORKER engagement.
        //
        // `dbg_mark_worker_scans_public` and `dbg_mark_steals_public` are the
        // real scan counts. They are what `lane-c-small-residue.md` §5 pointed
        // `work_units_done`'s consumers at when it deleted that counter for
        // being `steps × 256` rather than work done — and then nothing on any
        // production path read them either, so the replacement for a lying
        // counter was a silent one. Their only callers were `gc/tests/*.rs`.
        //
        // Printed as the per-worker VECTOR, not a sum, for the reason lane D's
        // evacuation census gives about its own rows: a total of 200k scans is
        // equally consistent with "eight workers shared the cycle" and with
        // "one worker did all of it and seven spun", and only the second is a
        // bug. `steals` beside it is how much of the balancing the work-stealing
        // path actually did.
        //
        // HERE rather than in `collector_decision_report` because these need
        // `&self` — they are per-collector state, not process statics. That is
        // the split this round settled on; see
        // `w3c-print-gc-summary-and-the-system-exit-arm.md`.
        let scans = self.dbg_mark_worker_scans_public();
        let scan_total: usize = scans.iter().sum();
        eprintln!(
            "[GC] g1 mark-workers: workers={} scans_total={scan_total} steals={} per_worker={:?}",
            scans.len(),
            self.dbg_mark_steals_public(),
            scans,
        );

        // LANE W6-M — THE MIXED-PAUSE REFUSAL CENSUS, on its own line.
        //
        // `[GC-SUMMARY] mixed count=0` says a mixed pause did not happen. It
        // does not say WHY, and the four candidate answers want four different
        // fixes. This line names the one that applied:
        //
        //   `no_cycle` ≈ `polls`  — the gate was never armed; no mark cycle
        //       completed, so `needs_mixed_gc()` was false every pause. Nothing
        //       in the selector is involved and tuning it cannot help.
        //   `no_work` > 0         — the gate was armed and `mixed_phase_has_work`
        //       refused; read `cand_no_data` / `cand_too_live` / `cand_pinned`
        //       for which screen did it, and `refused_waste` for the
        //       `heap_waste_percent` floor.
        //   `entered` > 0 with `old_regions` small — the pause happened and the
        //       OLD half was truncated; `capped_percent` is
        //       `old_cset_region_threshold_percent`, `capped_budget` is Step 7.
        //
        // The `cand_*` tallies are per POLL, not per region: a region refused
        // on ten polls counts ten times. See the field docs.
        let (
            polls,
            no_cycle,
            no_work,
            entered,
            cand_no_data,
            cand_too_live,
            cand_pinned,
            refused_waste,
            capped_percent,
            capped_budget,
            old_regions,
        ) = self.mixed_refusal_census();
        eprintln!(
            "[GC] g1 mixed-census: polls={polls} no_cycle={no_cycle} no_work={no_work} \
             entered={entered} old_regions={old_regions} cand_no_data={cand_no_data} \
             cand_too_live={cand_too_live} cand_pinned={cand_pinned} \
             refused_waste={refused_waste} capped_percent={capped_percent} \
             capped_budget={capped_budget} count_target={} live_threshold_pct={} \
             waste_pct={} old_cset_pct={}",
            self.config.mixed_gc_count_target,
            self.config.mixed_gc_live_threshold_percent,
            self.config.heap_waste_percent,
            self.config.old_cset_region_threshold_percent,
        );
        // gci i10/rh finding 1 -- the producer of the stale live estimate, both arms.
        eprintln!(
            "[GC] g1 mixed-live: promoted_into_marked_bytes={} promoted_into_marked_copies={} \
             hotspot_choice={} min_old={} ended_pinned={}",
            self.promoted_into_marked_bytes.load(Ordering::Relaxed),
            self.promoted_into_marked_copies.load(Ordering::Relaxed),
            self.mixed_hotspot_choice,
            self.mixed_min_old.load(Ordering::Relaxed),
            self.mixed_phase_ended_pinned.load(Ordering::Relaxed),
        );
        // gci i11/gm findings 3, 6 and 8 -- engagement of the old-CSet cost
        // model's marginal fix-up arm, the compaction-pause mixed-count switch
        // and the during-mark eager humongous reclaim. Both arms of each.
        eprintln!(
            "[GC] g1 gm-census: cost_model={} fixup_charged_pauses={} \
             count_skips_compaction={} compaction_in_mixed_phase={} \
             eager_mark={} eager_mark_spans={} eager_mark_bytes={} \
             eager_mark_gray_dropped={} eager_mark_declined={}",
            g1_pause_cost_model(),
            self.mixed_fixup_charged_pauses.load(Ordering::Relaxed),
            self.mixed_count_skips_compaction,
            self.mixed_count_compaction_pauses.load(Ordering::Relaxed),
            self.eager_reclaim_during_mark,
            self.eager_mark_spans.load(Ordering::Relaxed),
            self.eager_mark_bytes.load(Ordering::Relaxed),
            self.eager_mark_gray_dropped.load(Ordering::Relaxed),
            self.eager_mark_declined.load(Ordering::Relaxed),
        );

        // LANE W7-L — THE FUTILE OLD-TO-OLD COPY CENSUS, on its own line and
        // printed in BOTH arms of `CRATONVM_G1_MIXED_FUTILE_GUARD`.
        //
        // How to read it:
        //
        //   `futile` ≈ `futile + productive`  — every mixed pause this process
        //       took reclaimed less than 1.5x what it copied. That is W6-M
        //       §5's defect and it does not need the guard armed to be visible;
        //       this line is what makes an UNARMED run a null arm rather than
        //       a silence.
        //   `streak` large at exit  — the run ENDED inside the loop. A run
        //       that merely passed through it exits with a small streak.
        //   `breaker_polls` > 0 with `refused` = 0 and `probes` = 0 — the
        //       breaker tripped and the guard was not armed: the engagement
        //       count for the fix, measured on the control arm.
        //   `refused` > 0  — the guard is armed and refusing; `probes` is how
        //       many pauses the tripped breaker let through anyway, and
        //       `refused + probes` is `breaker_polls` from the moment it
        //       tripped.
        //   `waived_tight`  — polls whose verdict the `tight` waiver carried
        //       ALONE, with the `heap_waste_percent` floor refusing. W6-M
        //       named the waiver as the screen that should have caught this;
        //       `waived_tight` ≈ 0 beside a large `entered` says the floor
        //       was passing on its own and the waiver was never the screen in
        //       question.
        let (
            futile_pauses,
            productive_pauses,
            futile_streak,
            breaker_polls,
            refused_futile,
            futile_probes,
            waived_tight,
        ) = self.mixed_futile_census();
        eprintln!(
            "[GC] g1 mixed-futile: futile={futile_pauses} productive={productive_pauses} \
             streak={futile_streak} breaker_polls={breaker_polls} refused={refused_futile} \
             probes={futile_probes} waived_tight={waived_tight} guard={} ratio_pct={} \
             trip={} probe_every={} live_ratio_pct={}",
            true,
            MIXED_PRODUCTIVE_RATIO_PCT,
            MIXED_FUTILE_STREAK_TRIP,
            MIXED_FUTILE_PROBE_EVERY,
            // gci i17/mx: the second productive bar (`freed/copied` must exceed it).
            mixed_choice::live_threshold_ratio_pct(self.config.mixed_gc_live_threshold_percent),
        );
        // gci i18/mx: its own line, so the i17 line keeps its shape.
        let (skipped_samples, cycle_resets) = self.mixed_futile_i18_census();
        eprintln!(
            "[GC] g1 mixed-futile-i18: skipped_samples={skipped_samples} \
             cycle_resets={cycle_resets} reset_per_cycle={} young_to_reserve={} \
             needs_gc_free_percent={}",
            self.mixed_futile_reset_per_cycle,
            self.young_sizes_to_reserve,
            self.needs_gc_free_percent.load(Ordering::Relaxed),
        );

        let Some(s) = self.pause_summary() else {
            return;
        };
        let line = |label: &str, p: &G1PausePercentiles| {
            if p.count == 0 {
                return;
            }
            eprintln!(
                "[GC-SUMMARY] {label} count={} total_us={} p50_us={} p99_us={} max_us={} avg_us={}",
                p.count,
                p.total_us,
                p.p50_us,
                p.p99_us,
                p.max_us,
                p.total_us / p.count.max(1),
            );
        };
        line("young", &s.young);
        line("mixed", &s.mixed);
        // LANE W3-C — the drain passes the two lines above no longer contain.
        //
        // UNCONDITIONAL, including the zero, and that is the point: a run with
        // no evacuation failure must SAY it had none, or a reader cannot tell
        // this build (which excludes them) from the previous one (which folded
        // them into `young` and therefore always reported some). Without the
        // zero the correction would itself be invisible, which is the shape of
        // every defect this line was written to answer.
        eprintln!(
            "[GC-SUMMARY] drain_passes count={} total_us={} (evacuation-failure recovery passes recorded INSIDE a young pause; excluded from the young percentiles above)",
            s.drain_passes, s.drain_pass_us,
        );
        if s.dropped > 0 {
            eprintln!(
                "[GC-SUMMARY] note: {} oldest record(s) evicted from the {}-entry ring (percentiles cover the most-recent window)",
                s.dropped, PAUSE_HISTORY_CAP,
            );
        }
    }

    /// Enable GC event logging.
    pub fn enable_gc_logging(&self) {
        self.gc_log_enabled.store(true, Ordering::Relaxed);
    }

    /// Disable GC event logging.
    pub fn disable_gc_logging(&self) {
        self.gc_log_enabled.store(false, Ordering::Relaxed);
    }

    /// Get the total number of collections performed.
    pub fn collection_count(&self) -> u64 {
        self.collection_count.load(Ordering::Relaxed)
    }

    /// gci i5g/g1o (`gch-h4g1-g1-young-only-pauses-stamp-the-old-pools-collection-usage`):
    /// the collections recorded as [`G1CollectionType::Mixed`], i.e. the
    /// pauses that collected Old regions. The GC event plumbing samples it on
    /// both edges of a pause: a pause that did not move it was young-only, and
    /// HotSpot's `G1YoungGCMonitoringScope` then leaves `G1 Old Gen`'s
    /// collection usage and collection threshold alone. A drain pass inside a
    /// mixed pause records `YoungOnly` and does not move it, so the pause still
    /// reads mixed. One relaxed load.
    pub fn mixed_collection_count(&self) -> u64 {
        self.mixed_collection_count.load(Ordering::Relaxed)
    }

    /// zt t31/x10: when the last collection pause of this heap ended (the
    /// GC-overhead clock's edge), `None` before the first. G1's periodic
    /// collection measures HotSpot's `time_since_last_collection` from it.
    pub fn last_pause_end(&self) -> Option<std::time::Instant> {
        *self.last_pause_end.lock()
    }

    /// gci i11/go (i10rh finding 9): the pauses that ran as HotSpot's `Prepare
    /// Mixed` (only under `CRATONVM_G1_PREPARE_MIXED`). Sampled on both edges
    /// of a pause by the GC event plumbing, as [`Self::mixed_collection_count`]
    /// is. One relaxed load.
    pub fn prepare_mixed_pause_count(&self) -> u64 {
        self.prepare_mixed_pauses.load(Ordering::Relaxed)
    }

    /// gci i11/go: the region size in bytes (`-XX:G1HeapRegionSize` after
    /// ergonomics). An object is humongous when it is larger than half of it
    /// (HotSpot's `is_humongous`; [`Self::is_humongous_request`]).
    pub fn region_size(&self) -> usize {
        self.config.region_size
    }

    /// gci i11/go (i10rh finding 4): would an allocation of `bytes` be a
    /// humongous request (`bytes > region_size / 2`, the threshold
    /// `alloc_in_region_locked` applies)? The VM labels the collection such a
    /// request asks for `G1 Humongous Allocation`, as HotSpot's
    /// `attempt_allocation_humongous` does. Pure.
    pub fn is_humongous_request(&self, bytes: usize) -> bool {
        bytes > self.config.region_size / 2
    }

    /// gci i11/go (i10rh finding 4): would the mark-cycle trigger open a
    /// concurrent cycle NOW -- [`Self::check_ihop`]'s decision (with the level
    /// test `VmHeap::g1_should_start_marking` puts in front of it), read
    /// WITHOUT its side effects (no poll census, no deadline release, no gate
    /// publish). The GC event plumbing asks it inside an evacuation pause, as
    /// HotSpot's `G1Policy::decide_on_concurrent_start_pause` decides at the
    /// pause, to label that pause `Pause Young (Concurrent Start)` when the
    /// pause's mark-cycle drive is about to open a cycle. The answer can
    /// differ from the drive's only by a race (mutator promotion between the
    /// release and the drive, or the initial mark losing its STW race).
    ///
    /// gci i22/sz: a pending humongous concurrent-start request
    /// ([`Self::humongous_conc_start_requested`], opt-in
    /// `CRATONVM_G1_HUMONGOUS_IHOP_START`) answers yes, as the gate it mirrors
    /// does with hunk SZ-H1: HotSpot labels that pause `Pause Young
    /// (Concurrent Start) (G1 Humongous Allocation)`.
    pub fn would_start_marking(&self) -> bool {
        if self.humongous_conc_start_requested() {
            return true;
        }
        let threshold = self.marking_threshold_bytes.load(Ordering::Relaxed);
        let old_bytes = self.old_gen_bytes.load(Ordering::Relaxed);
        if threshold == 0 || old_bytes <= threshold {
            return false;
        }
        if !gc_flags().g1_ihop_backoff && !self.breached_reserve_backs_off() {
            return true;
        }
        if self.last_cycle_reclaimed_bytes.load(Ordering::Relaxed) > 0 {
            return true;
        }
        let last_end_old = self.last_cycle_end_old_bytes.load(Ordering::Relaxed);
        if last_end_old == 0 {
            return true;
        }
        let required = self
            .mark_backoff_regions
            .load(Ordering::Relaxed)
            .max(1)
            .saturating_mul(self.config.region_size);
        if old_bytes >= last_end_old.saturating_add(required) {
            return true;
        }
        if gc_flags().g1_mark_backoff_deadline {
            let last = self.last_cycle_end_collections.load(Ordering::Relaxed);
            let now = self.collection_count.load(Ordering::Relaxed);
            return last != u64::MAX
                && now.saturating_sub(last) >= MARK_BACKOFF_DEADLINE_COLLECTIONS;
        }
        false
    }

    /// gci i20/ys (`gci-i20ys-g1-humongous-allocation-never-asks-for-a-concurrent-start-FIXED-20261016`,
    /// default-ON `CRATONVM_G1_HUMONGOUS_IHOP_START`): should a humongous request
    /// of `bytes` (header included) ask for a collection BEFORE it allocates,
    /// as HotSpot 25's `G1CollectedHeap::attempt_allocation_humongous` does
    /// (`policy()->need_to_start_conc_mark("concurrent humongous allocation",
    /// word_size)` -> `collect(GCCause::_g1_humongous_allocation)`, a young
    /// pause that is the concurrent start)? True only when the request is
    /// humongous, no marking cycle is open, the young-only phase is on (no
    /// mixed phase pending: HotSpot's `in_young_only_phase()`), and the old
    /// occupancy plus the request rounded up to whole regions exceeds the IHOP
    /// threshold ([`humongous_request_crosses_ihop`]).
    ///
    /// Without it a G1 humongous allocation that succeeds never collects, so
    /// `G1 Humongous Allocation` is reported only when one FAILS (the i19/oo
    /// label doors), which a heap with a humongous-churning mutator and room to
    /// spare never reaches: vi19-oo N saw `[G1 Evacuation Pause]` alone 6/6
    /// where HotSpot reports `G1 Humongous Allocation`. Read per call (the
    /// humongous slow path, never the TLAB path). The VM half (the array
    /// ladders asking this before they allocate) is the page's hunk.
    pub fn humongous_alloc_wants_concurrent_start(&self, bytes: usize) -> bool {
        if !self.is_humongous_request(bytes) {
            return false;
        }
        if self.gc_state.is_marking_active() || self.needs_mixed_gc() {
            return false;
        }
        // gci i26/hu (vi22-sz HC1 / HC3 FAIL, `humongous-cause-seen false`):
        // HotSpot's level test is in REGIONS (`non_young_capacity_bytes`, a
        // humongous String of 800 000 bytes holds a whole 1 MiB region) and
        // against its 45 % initial IHOP; ours read `old_gen_bytes` (800 000
        // per String) against our 70 % default, so at -Xmx64m the
        // Free-fraction young pause (~35 MiB used) always came first and the
        // request was never raised.
        let wants = humongous_request_crosses_ihop(
            humongous_request_occupancy(
                self.region_table_len(),
                self.free_region_count.load(Ordering::Relaxed),
                self.young_region_count.load(Ordering::Relaxed),
                self.config.region_size,
                self.old_gen_bytes.load(Ordering::Relaxed),
            ),
            bytes,
            self.config.region_size,
            humongous_request_ihop_threshold(
                self.marking_threshold_bytes.load(Ordering::Relaxed),
                self.heap_grid_bytes(),
                self.config.ihop_percent,
                G1CollectorConfig::default().ihop_percent,
            ),
        );
        // gci i22/sz (the page's residual 3): every caller answers `true` by
        // running the `G1 Humongous Allocation` pause; record that the pause
        // is a concurrent-start request, so the mark drive after it opens the
        // cycle (HotSpot's `should_do_concurrent_full_gc(_g1_humongous_allocation)`)
        // instead of re-testing IHOP without the request's bytes.
        if wants {
            self.humongous_conc_start_requested
                .store(true, Ordering::Release);
        }
        wants
    }

    /// gci i22/sz -- is a humongous concurrent-start request pending
    /// ([`Self::humongous_alloc_wants_concurrent_start`])? The VM's mark-start
    /// gate (`VmHeap::g1_should_start_marking`) answers yes while it is. A
    /// READ, not a take: the JIT mark driver asks the gate once to decide
    /// whether to stop, and `g1_drive_mark_cycle` asks it again before it
    /// opens the cycle, so a take in the first read would refuse the second.
    /// The request is cleared where it is answered, at the cycle start
    /// ([`Self::note_mark_cycle_start`]); one that loses its initial-mark
    /// race stays pending for the next drive. It is only ever set while no
    /// cycle is open and no mixed phase pending (the predicate's gates).
    /// Always `false` with `CRATONVM_G1_HUMONGOUS_IHOP_START` off (nothing
    /// sets it): one relaxed load on the gate, which the JIT mark driver
    /// polls at allocation rate.
    pub fn humongous_conc_start_requested(&self) -> bool {
        self.humongous_conc_start_requested.load(Ordering::Acquire)
    }

    /// gch h4/g1 (`r15w4-mh2-address-memo-epoch-misses-g1-cleanup-patch`, step
    /// 1): a counter that moves whenever an address may come to name a
    /// different object -- every evacuation pause (it books a collection and
    /// bumps `rset_cache_epoch`) AND `cleanup`'s in-place reclaim, which frees
    /// whole Old regions and dead humongous spans without booking a
    /// collection. For address-keyed memos (the MethodHandle direct lane, the
    /// `DecimalFormat` plan stamp), which key on "nothing moves or is freed
    /// within one epoch"; [`Self::collection_count`] does not move at cleanup,
    /// so on G1 it is not that epoch. Not a statistic: only equality across two
    /// reads means anything.
    pub fn address_epoch(&self) -> u64 {
        self.collection_count
            .load(Ordering::Relaxed)
            .wrapping_add(self.rset_cache_epoch.load(Ordering::Acquire))
    }

    /// Get the total pause time across all collections.
    pub fn total_pause_ms(&self) -> u64 {
        self.total_pause_us.load(Ordering::Relaxed) / 1000
    }

    /// Total pause time across all collections in microseconds (the precise
    /// accumulator; `total_pause_ms()` rounds this to milliseconds).
    pub fn total_pause_us(&self) -> u64 {
        self.total_pause_us.load(Ordering::Relaxed)
    }

    /// Get the current GC phase.
    pub fn gc_phase(&self) -> ConcurrentGcPhase {
        self.gc_state.phase()
    }

    /// Get old-gen byte count.
    pub fn old_gen_bytes(&self) -> usize {
        self.old_gen_bytes.load(Ordering::Relaxed)
    }

    /// Publish the occupancy denominators the normalized `gc_metrics` view
    /// divides by (audit §9 item 5).
    ///
    /// Item 5 says the rset question is decided on `rset_bytes_per_live_byte`
    /// and that "nothing has measured it yet". The reason is narrower than
    /// "nobody ran the workload": `record_heap_occupancy` had exactly one
    /// caller in the tree, `GenerationalHeap`, so under `-XX:+UseG1GC` the
    /// DENOMINATOR was never published and every per-live-byte ratio was
    /// structurally `0.000000` no matter what ran. The number could not be
    /// measured, not merely had not been.
    ///
    /// `live` here is the post-pause occupancy of the surviving regions — the
    /// sum of their bump cursors — which is an over-estimate of live bytes by
    /// whatever garbage survived this pause uncollected. That is the honest
    /// denominator available at a young pause: true liveness is only known
    /// after a mark cycle, and a ratio that appears only after marking would
    /// leave the young-only runs exactly as unmeasured as they are now. It is
    /// an over-estimate, so `rset_bytes_per_live_byte` reads LOW, which is the
    /// fail-safe direction for a number used to argue the set is small enough.
    pub fn publish_occupancy_metrics(&self, regions: &[G1Region]) {
        let live: usize = regions
            .iter()
            .filter(|r| r.region_type() != RegionType::Free)
            .map(|r| r.cursor())
            .sum();
        // G1 keeps no allocated-object/byte totals of its own, so those two
        // stay at whatever they were rather than being clobbered with a zero
        // that would read as "nothing was allocated".
        let raw = cratonvm_gc_core::gc_metrics::gc_metrics_raw();
        cratonvm_gc_core::gc_metrics::record_heap_occupancy(
            raw.allocated_objects,
            raw.allocated_bytes,
            live as u64,
        );
    }

    /// Recompute the IHOP occupancy statistic (`old_gen_bytes`) from the
    /// region table: bytes in `Old` regions PLUS humongous spans.
    ///
    /// Humongous objects are logically part of the old generation — HotSpot's
    /// IHOP compares old occupancy *including* humongous regions to the
    /// threshold. Counting only `Old` regions here meant a humongous-heavy
    /// workload (large arrays churned faster than they promote ordinary
    /// objects) never crossed IHOP, so concurrent marking — the ONLY path
    /// that reclaims dead humongous spans (`cleanup` →
    /// `reclaim_dead_humongous_spans_locked`) — never started and the heap
    /// filled with unreclaimable dead spans until OOM, while young pauses
    /// spun freeing nothing.
    ///
    /// A `HumongousStart` region's `cursor` is the FULL object size (its
    /// continuations carry `cursor = 0`), so summing both types counts each
    /// humongous object exactly once.
    pub fn recompute_old_gen_bytes(&self, regions: &[G1Region]) {
        // OCCUPANCY IS REGIONS CONSUMED, NOT BYTES LIVE IN THEM.
        //
        // IHOP is a percentage of the HEAP (`ihop_percent`, 70 by default, so
        // 179 MB of a 256 MB heap), and the heap is a fixed set of REGIONS. An
        // Old region is unavailable to the allocator whether it is 100% full or
        // 5% full, so summing each region's `cursor()` measures something the
        // threshold is not about -- and under fragmentation the two diverge
        // until the trigger can never fire.
        //
        // Measured on `org.h2.test.store.TestMVStoreTool` at -Xmx256m,
        // 2026-09-05: 211 Old regions holding 44.7 MB between them -- 21% full
        // on average. Against a 179 MB threshold that reads as 25% occupancy,
        // so IHOP never fires, no concurrent mark starts, `marking_complete` is
        // never set, `needs_mixed_gc()` is never true, and NO MIXED COLLECTION
        // EVER RUNS. Meanwhile the region supply is gone: `free=0` with 245 of
        // 256 regions Old, 18-54 to-space exhaustions per run and 17 evacuation
        // failures, each of which also declines eager humongous reclaim.
        //
        // Counted in region-size units so it stays comparable with a threshold
        // derived from `heap_size`. A humongous span contributes one unit per
        // region it occupies (start plus continuations), which is exactly what
        // it costs the allocator.
        let old_bytes: usize = if gc_flags().g1_ihop_counts_regions {
            regions
                .iter()
                .filter(|r| {
                    matches!(
                        r.region_type(),
                        RegionType::Old
                            | RegionType::HumongousStart
                            | RegionType::HumongousContinuation
                    )
                })
                .count()
                .saturating_mul(self.config.region_size)
        } else {
            regions
                .iter()
                .filter(|r| {
                    matches!(
                        r.region_type(),
                        RegionType::Old
                            | RegionType::HumongousStart
                            | RegionType::HumongousContinuation
                    )
                })
                .map(|r| r.cursor())
                .sum()
        };
        self.old_gen_bytes.store(old_bytes, Ordering::Relaxed);
    }

    /// Get the IHOP marking threshold.
    pub fn marking_threshold_bytes(&self) -> usize {
        self.marking_threshold_bytes.load(Ordering::Relaxed)
    }

    /// Return the total heap capacity in bytes.
    ///
    /// LANE E wave 2, small finding 6 — this used to return `config.heap_size`
    /// (i.e. `-Xmx`), which is wrong twice over. `num_regions = heap_size /
    /// region_size` TRUNCATES, so the real region grid is up to
    /// `region_size - 1` bytes smaller than `-Xmx`; and `-Xmx` is a
    /// RESERVATION since F-16, not a capacity. `VmHeap` builds
    /// `Runtime.freeMemory()` out of this (`gc/src/vm_heap.rs`), so the old
    /// answer described address space the process may never charge itself for.
    ///
    /// The capacity reported is the SOFT one — the region grid by default, and
    /// the adaptive target once `CRATONVM_G1_HEAP_RESIZE` is armed, which is
    /// the HotSpot correspondence: `Runtime.totalMemory()` reports the heap the
    /// VM is currently willing to use, and `maxMemory()` reports `-Xmx`.
    /// [`Self::reserved_bytes`] is still the `-Xmx` grid and
    /// [`Self::committed_bytes`] still what is actually charged.
    pub fn heap_capacity(&self) -> usize {
        self.heap_target_bytes()
    }

    /// What the Old generation can still absorb, in bytes: the Free regions
    /// within the soft capacity ([`Self::heap_target_regions`], as `needs_gc`
    /// counts them), less the to-space reserve the evacuation-headroom
    /// trigger keeps for the next pause ([`to_space_reserve_regions`]). One
    /// relaxed load of the maintained Free count -- exact between pauses,
    /// re-derived at every pause's end -- and no table walk.
    ///
    /// gcd d10/o retirement (2026-10-04): this is G1's
    /// `VmHeap::old_gen_headroom`, the GC-overhead limit's "is the old
    /// generation wedged" yardstick. It was `capacity - allocated bytes`, which
    /// on a G1 heap at its `OutOfMemoryError` still read 12 % of the heap
    /// (`Gcd10oG1LatchProbe -Xmx128m`: 16 MB): the to-space reserve, which the
    /// mutator never gets, plus the unused tails of occupied regions. The
    /// streak's 2 % wedge test could therefore never pass on G1, and its
    /// latched exit was unreachable under any limit.
    pub fn old_absorbable_bytes(&self) -> usize {
        let table = self.region_table_len();
        let total = self.heap_target_regions().min(table.max(1));
        let free = self
            .free_region_count
            .load(Ordering::Relaxed)
            .saturating_sub(table.saturating_sub(total));
        free.saturating_sub(to_space_reserve_regions(total))
            .saturating_mul(self.config.region_size)
    }

    /// The configured initial heap size (`-Xms`, else the default), never
    /// above the capacity: `G1 Old Gen`'s JMX `init` (gcd d10/o retirement).
    pub fn initial_heap_bytes(&self) -> usize {
        self.config.initial_heap_size.min(self.heap_capacity())
    }

    /// LANE E wave 2, small finding 7 — the ONE answer to "how many regions
    /// does this heap have".
    ///
    /// `needs_gc` derived it as `heap_size / region_size`, while
    /// `note_region_consumed_locked` and `mixed_phase_has_work` used
    /// `regions.len()`. They agree today only because `new` builds the table
    /// from exactly that expression and `needs_gc`'s comment says so — a latent
    /// divergence rather than a bug, and exactly the kind that survives a
    /// change to how the table is built. Takes no lock, for the same reason
    /// `needs_gc` did not: the table is built once in `new` and never resized.
    #[inline]
    pub fn region_table_len(&self) -> usize {
        self.config.heap_size / self.config.region_size.max(1)
    }

    /// Return (used, capacity) for Eden regions.
    pub fn eden_stats(&self) -> (usize, usize) {
        let regions = self.regions.read();
        let mut used = 0usize;
        let mut count = 0usize;
        for r in regions.iter() {
            if r.region_type() == RegionType::Eden {
                used += r.cursor();
                count += 1;
            }
        }
        (used, count * self.config.region_size)
    }

    /// Return (used, capacity) for Old regions.
    pub fn old_gen_stats(&self) -> (usize, usize) {
        let regions = self.regions.read();
        let mut used = 0usize;
        let mut count = 0usize;
        for r in regions.iter() {
            if r.region_type() == RegionType::Old {
                used += r.cursor();
                count += 1;
            }
        }
        (used, count * self.config.region_size)
    }

    /// Count regions of a given type.
    pub fn count_regions(&self, region_type: RegionType) -> usize {
        let regions = self.regions.read();
        regions
            .iter()
            .filter(|r| r.region_type() == region_type)
            .count()
    }

    /// Per-family `(used, regions)` in one region-table walk — see
    /// [`G1RegionCensus`]. The reader of HotSpot's three G1 pools (Eden,
    /// Survivor, Old-including-humongous): `VmHeap::young_gen_stats` /
    /// `old_gen_stats`. gc-common w4-f.
    /// t1 (T1W13BigAfterChurn diagnosis): the region table as one character
    /// per region, lowest address first -- `.` Free, `E` Eden, `S` Survivor,
    /// `O` Old, `H` humongous start, `h` continuation -- so the OOME ladder
    /// (`CRATONVM_DBG_OOME_LADDER`) shows which regions split a humongous
    /// request's run. Diagnostic only; one read of the table.
    pub fn region_shape_map(&self) -> String {
        self.regions
            .read()
            .iter()
            .map(|r| match r.region_type() {
                RegionType::Free => '.',
                RegionType::Eden => 'E',
                RegionType::Survivor => 'S',
                RegionType::Old => 'O',
                RegionType::HumongousStart => 'H',
                RegionType::HumongousContinuation => 'h',
            })
            .collect()
    }

    /// JIT round 23 wave 1b, lane L7 (R23W1L7-2): how many regions the
    /// published JIT pin words name right now, as the pauses count them
    /// (`jit_frames_live_for_pins` first, then each word's region), and how
    /// many words there are. A region a pin names stays out of every
    /// collection set, the full compaction's candidates included, so this
    /// is the figure the OOME ladder line (`CRATONVM_DBG_OOME_LADDER`) lacked
    /// for the JIT-on 1/10 against `--nojit` 0/10 split of `R22FlipGcRows`.
    /// Diagnostic only: it takes no region lock (a word's region is
    /// arithmetic on the arena bounds) and only the pin registry's own
    /// mutexes, briefly.
    pub fn published_jit_pin_census(&self) -> (usize, usize) {
        if !jit_frames_live_for_pins() {
            return (0, 0);
        }
        let words = cratonvm_gc_core::gc_quiescence::pinned_jit_roots_snapshot();
        let mut regions: RegionSet = RegionSet::new();
        for &addr in &words {
            if let Some(i) = self.lookup_region_for_addr(addr) {
                regions.insert(i);
            }
        }
        (regions.len(), words.len())
    }

    pub fn region_census(&self) -> G1RegionCensus {
        let regions = self.regions.read();
        let mut c = G1RegionCensus {
            region_size: self.config.region_size,
            ..G1RegionCensus::default()
        };
        for r in regions.iter() {
            let slot = match r.region_type() {
                RegionType::Eden => &mut c.eden,
                RegionType::Survivor => &mut c.survivor,
                RegionType::Old => &mut c.old,
                RegionType::HumongousStart | RegionType::HumongousContinuation => &mut c.humongous,
                RegionType::Free => {
                    c.free += 1;
                    continue;
                }
            };
            slot.0 += r.cursor();
            slot.1 += 1;
        }
        c
    }

    /// LANE W6-A — the `[GC] g1 card-table:` line, formatted rather than
    /// printed so a test can assert on it without capturing stderr. This is the
    /// same split `tlab::tlab_census_lines` and `g1_evac_worker_census_report`
    /// already use, and it is what lets
    /// `gc/tests/g1_w3c_report_reachability.rs` pin this line the way it pins
    /// the W3-C block.
    ///
    /// # The four accessors this is the reader for
    ///
    /// All four had no caller outside `#[cfg(test)]` and `gc/tests/*.rs`.
    /// `G1CardTable::arena_base` and `arena_len` had none at all, through two
    /// sweeps.
    ///
    /// * `cards` / `dirty` — [`G1CardTable::card_count`] and
    ///   [`G1CardTable::dirty_card_total`]. `dirty_card_total`'s own doc names
    ///   the experiment that would falsify this collector's "a card goes clean
    ///   only at region reset" choice — *a measurement showing an old
    ///   generation whose cards saturate* — and calls itself that experiment's
    ///   numerator. Nothing read either half, so the experiment its doc
    ///   describes could not be run on a shipped binary. `saturation` is the
    ///   ratio, printed beside BOTH of its terms rather than alone, because a
    ///   percentage with no denominator is what rule 5 of this round's method
    ///   exists to refuse.
    /// * `block_entries` — [`G1CardTable::block_entries_in`] over the whole
    ///   arena. `[GC] g1 block-offsets:` prints `jumps=` and `entries_refused=`
    ///   but never how many entries the table HOLDS, so `jumps=0` has two
    ///   readings — "the heap had nothing to skip" and "the table was empty" —
    ///   and `CRATONVM_G1_BLOCK_OFFSETS` ships OFF pending exactly that
    ///   distinction (README §3).
    ///
    /// # Cost, and why it is not on a pause path
    ///
    /// Two O(cards) sweeps, one byte load each: on a 256 MB heap at 512 B per
    /// card that is 2 × 524 288 relaxed loads, once, at shutdown, after the
    /// last pause. `dirty_card_total`'s doc already says it "belongs on a
    /// diagnostic path and not in a pause"; this is that path.
    ///
    /// # Why `block_offsets=` is on the line
    ///
    /// §7.1 of `orchestrator-wave-1-measurements.md`. A coverage count is not
    /// readable without the resolved value of the switch that fills the table,
    /// and `gc/tests/g1_w3c_report_reachability.rs` asserts every census line
    /// carries one. `card_clean=` is the switch for the dirty count's other
    /// half — cleaning is the only thing that pushes saturation back down.
    pub fn card_table_census_line(&self) -> String {
        let cards = self.cards.card_count();
        let dirty = self.cards.dirty_card_total();
        let block_entries = self
            .cards
            .block_entries_in(self.cards.arena_base(), self.cards.arena_len());
        format!(
            "[GC] g1 card-table: cards={cards} dirty={dirty} saturation={:.2}% \
             block_entries={block_entries} block_coverage={:.2}% \
             card_clean={} block_offsets={}",
            if cards == 0 {
                0.0
            } else {
                dirty as f64 * 100.0 / cards as f64
            },
            if cards == 0 {
                0.0
            } else {
                block_entries as f64 * 100.0 / cards as f64
            },
            gc_flags().g1_card_clean,
            lane_a_block_offsets(),
        )
    }

    /// W4-A — the block-offset table's engagement, as
    /// `(jumps, bytes_jumped, regions_whose_entry_was_refused)`.
    ///
    /// The same three numbers the `[GC] g1 block-offsets:` census line prints,
    /// reachable from a test. A test that arms
    /// `CRATONVM_G1_BLOCK_OFFSETS` and asserts only that the heap survived is
    /// indistinguishable from a test whose lever never engaged — which is the
    /// failure §7.3 of `orchestrator-wave-1-measurements.md` says cost this
    /// round three retracted measurements — so the tests in
    /// `gc/tests/g1_w4a_*.rs` assert on this instead of hoping.
    ///
    /// `refused` counts REGIONS in which a candidate entry failed its header
    /// screen, not individual refusals: the walk latches the refusal per region
    /// and finishes it linearly, so the second candidate in a region is never
    /// asked. It must be zero on a healthy heap.
    pub fn block_offset_census(&self) -> (u64, u64, u64) {
        (
            self.block_jumps_taken.load(Ordering::Relaxed),
            self.block_bytes_jumped.load(Ordering::Relaxed),
            self.block_entries_refused.load(Ordering::Relaxed),
        )
    }

    /// Check if mixed GC is needed (marking is complete and cycles remain).
    pub fn needs_mixed_gc(&self) -> bool {
        // Round-9 HIGH-1: Acquire pairs with the Release publish in
        // `finish_mark_cycle` so this reader observes the fully-drained
        // gray set and deactivated SATB queue before acting on the flag.
        self.marking_complete.load(Ordering::Acquire)
            && self.mixed_gc_remaining.load(Ordering::Relaxed) > 0
    }

    /// gci i13/vg (`CRATONVM_GC_VT_PARKED_GATHER_FILTER`): G1's answer to
    /// `VmHeap::parked_old_only_young_ranges` -- the ranges of every region
    /// that is NOT `Old` (eden, survivor, humongous start / continuation,
    /// free), or `None` unless the next `collect_garbage` is provably a plain
    /// young pause with no mark open.
    ///
    /// Why the split is sound only then: a young pause's collection set is the
    /// eden and survivor regions, and its old -> young edges come from the
    /// remembered sets, so a root whose every word is in an `Old` region (or
    /// outside the heap) is never followed, moved or freed by it. The one
    /// young-pause reader of roots outside the CSet is the eager humongous
    /// reclaim (a span no root names may go), which is why humongous regions
    /// count as "young" here. Every other pause shape reads old regions from
    /// the roots: a mixed pause and a full compaction evacuate old regions
    /// (`collect_garbage`'s `needs_mixed_gc` / `full_compaction_armed` /
    /// `pending_humongous_compaction` arms), and an open mark (phase not
    /// `Idle`, SATB active, gray work left) is not argued here. The checks
    /// are taken inside the pause, before the gather, and nothing between the
    /// gather and `collect_garbage` can change them (cleanup, which publishes
    /// `marking_complete`, is itself a pause). A future pause shape that marks
    /// from the root slice (a concurrent-start young pause) must answer `None`
    /// here for that pause.
    pub fn parked_old_only_young_ranges(&self) -> Option<Vec<(usize, usize)>> {
        if !self.next_pause_provably_young_only() {
            return None;
        }
        let regions = self.regions.read();
        let mut out: Vec<(usize, usize)> = Vec::new();
        for r in regions.iter() {
            if r.region_type() == RegionType::Old {
                continue;
            }
            let base = r.data.addr();
            let end = base + r.data.len();
            // Coalesce neighbours (regions are laid out in address order).
            match out.last_mut() {
                Some(last) if last.1 == base => last.1 = end,
                _ => out.push((base, end)),
            }
        }
        Some(out)
    }

    /// The gate of [`Self::parked_old_only_young_ranges`]: no mark is open and
    /// the next `collect_garbage` is provably a plain young pause.
    pub fn next_pause_provably_young_only(&self) -> bool {
        self.gray_set_is_empty() && self.next_pause_young_only_no_mark()
    }

    /// [`Self::next_pause_provably_young_only`] without the gray-set test: no
    /// concurrent phase and no SATB log, no armed full or humongous
    /// compaction, no mixed phase pending. The gray set is non-empty at idle
    /// pauses (42 of 44 in `WeakRowPopulationPause`; why was not traced), which
    /// would keep this gate shut on every pause, and a young pause is not
    /// affected by it: only phase and SATB say a mark is open.
    pub fn next_pause_young_only_no_mark(&self) -> bool {
        self.gc_state.phase() == ConcurrentGcPhase::Idle
            && !self.satb_queue.is_active()
            && !self.full_compaction_armed.load(Ordering::Acquire)
            && self.pending_humongous_compaction.lock().is_none()
            && !self.needs_mixed_gc()
    }

    /// P3 (`p3-collection-object-hot-paths-review-RETIRED-20261107`, hypothesis 1): the `Old`
    /// regions' address ranges when the next pause is provably young-only, else
    /// `None`. A young pause neither moves nor frees anything inside them, so a
    /// weak row lying wholly in them is settled for that pause (the reference
    /// processor sets such rows aside: `ReferenceProcessor::stash_settled_rows`).
    /// Humongous regions are left out, as in [`Self::parked_old_only_young_ranges`]
    /// (eager reclaim can free one).
    pub fn young_pause_old_ranges(&self) -> Option<Vec<(usize, usize)>> {
        self.next_pause_young_only_no_mark()
            .then(|| self.parked_mirror_keyed_view(true).1)
    }

    /// gci i15/vh (`CRATONVM_GC_VT_PARKED_MIRROR_KEYED` on G1, item 6 of
    /// `gci-i11vt-virtual-thread-continuation-roots-review-findings-FIXED-20261016`):
    /// what the two G1 marking root scans need to key parked virtual threads
    /// by their mirrors. Returns `(last_mark_trusted, old_ranges)`:
    ///
    /// * `last_mark_trusted` -- whether the last `cleanup` trusted its closure
    ///   (no implausible gray entry, gray set drained; `retain_all` false).
    ///   Read by the remark door right AFTER `g1_final_remark_and_cleanup`
    ///   returned: only then may a parked thread whose mirror the mark left
    ///   unmarked be retired. An untrusted cycle freed nothing, and its
    ///   unmarked mirror is not proven dead, so the thread stays.
    /// * `old_ranges` (empty unless `with_ranges`) -- the address ranges of
    ///   every `Old` region, coalesced, in address order: the gather keys an
    ///   entry only when its mirror and every snapshot word lie in them.
    ///   Humongous, eden, survivor and free regions are left out (an entry with
    ///   such a word stays rooted). Read inside the marking pause, before the
    ///   gather; nothing reclassifies a region inside it.
    pub fn parked_mirror_keyed_view(&self, with_ranges: bool) -> (bool, Vec<(usize, usize)>) {
        let trusted = self.last_mark_trusted.load(Ordering::Acquire);
        if !with_ranges {
            return (trusted, Vec::new());
        }
        let regions = self.regions.read();
        let mut out: Vec<(usize, usize)> = Vec::new();
        for r in regions.iter() {
            if r.region_type() != RegionType::Old {
                continue;
            }
            let base = r.data.addr();
            let end = base + r.data.len();
            match out.last_mut() {
                Some(last) if last.1 == base => last.1 = end,
                _ => out.push((base, end)),
            }
        }
        (trusted, out)
    }

    /// Ten-findings items 2 and 3 — is there an Old region worth a mixed pause?
    ///
    /// A candidate is unpinned, has mark data, and is below the live threshold
    /// (`mixed_gc_live_threshold_percent`, HotSpot's
    /// `G1MixedGCLiveThresholdPercent`): a region that is 98% live costs almost
    /// a full copy to reclaim 2%, and the old selectors used to take it on the
    /// pause budget alone. The phase has work only if some candidate exists AND
    /// the garbage the candidates hold together is at least
    /// `heap_waste_percent` of the heap (`G1HeapWastePercent`) — below that the
    /// mixed pauses are all copy and no reclaim, and the phase ends early. The
    /// JIT-pinned set is not known here; a pause whose every candidate left is
    /// pinned ends the phase itself (g1ep7,
    /// [`Self::end_mixed_phase_if_only_pinned_left`]).
    pub fn mixed_phase_has_work(&self) -> bool {
        self.mixed_phase_has_work_within(true)
    }

    /// [`Self::mixed_phase_has_work`] with LANE W7-L's breaker as a PARAMETER.
    ///
    /// The `_within` shape exists here for the reason it exists everywhere
    /// else in this file: `gc_flags()` is a process-wide snapshot parsed once
    /// at first use, so a test that wants both arms cannot get them by setting
    /// an environment variable twice. The whole claim this change rests on is
    /// a comparison of what the SAME heap state decides with the breaker armed
    /// and unarmed, and a test that could only ever reach one arm would be
    /// pinning the build rather than the rule.
    pub fn mixed_phase_has_work_within(&self, futile_guard: bool) -> bool {
        let regions = self.regions.read();
        let region_size = self.config.region_size;
        let threshold = self.config.mixed_gc_live_threshold_percent as usize;
        let mut candidates = 0usize;
        let mut reclaimable = 0usize;
        // LANE W6-M — separate the three ways an Old region fails to be a
        // candidate, because "candidates=0" on its own does not say whether the
        // marker never gave the region data, the region is too live to be worth
        // copying, or a pin excluded it. Those three want three different
        // fixes and four waves could not tell them apart.
        let (mut no_data, mut too_live, mut pinned_out) = (0u64, 0u64, 0u64);
        for r in regions.iter() {
            if r.region_type() != RegionType::Old {
                continue;
            }
            if r.pinned {
                pinned_out += 1;
                continue;
            }
            if r.live_bytes == 0 {
                no_data += 1;
                continue;
            }
            if r.mixed_live(self.mixed_hotspot_choice) * 100 >= region_size * threshold {
                too_live += 1;
                continue;
            }
            candidates += 1;
            reclaimable += r.mixed_reclaimable(self.mixed_hotspot_choice);
        }
        self.mixed_cand_no_mark_data
            .fetch_add(no_data, Ordering::Relaxed);
        self.mixed_cand_too_live
            .fetch_add(too_live, Ordering::Relaxed);
        self.mixed_cand_pinned
            .fetch_add(pinned_out, Ordering::Relaxed);
        if candidates == 0 {
            if gc_flags().g1_dbg_reach {
                eprintln!(
                    "[g1][MIXWORK] old={} live_gt0={} candidates=0 verdict=false",
                    regions
                        .iter()
                        .filter(|r| r.region_type() == RegionType::Old)
                        .count(),
                    regions
                        .iter()
                        .filter(|r| r.region_type() == RegionType::Old && r.live_bytes > 0)
                        .count(),
                );
            }
            return false;
        }
        // The waste floor is a HEADROOM policy: with plenty of Free regions,
        // a mixed pause that copies a lot to reclaim a little is not worth
        // taking. It must not refuse the work when the heap is tight — measured
        // on `G1CardChurn 11 60` at `-Xmx24m`: with the floor applied
        // unconditionally, a pause storm of ~886 young pauses (against ~40)
        // ran while the free pool sat under the trigger and the only regions
        // that could relieve it were refused as "not worth it". HotSpot has
        // the same floor and a full GC behind it; this collector's full GC IS
        // the mixed sequence, so the floor is waived once the Free pool is
        // within twice the free-percent trigger.
        let total = regions.len().max(1);
        let free = self.free_region_count.load(Ordering::Relaxed);
        let trigger_pct = self.needs_gc_free_percent.load(Ordering::Relaxed).max(1);
        // gci i23/tn (i20/ys review finding 6, opt-in
        // `CRATONVM_G1_MIXED_WAIVER_BASE_TRIGGER`): against twice the BASE
        // trigger, so a trigger ratcheted to its 50 % cap by evacuation
        // failures no longer reads `tight` at every occupancy. Default: the
        // live trigger, byte for byte as before (`g1_mixed_waiver_tight`).
        let tight = g1_mixed_waiver_tight(free, total, trigger_pct, g1_mixed_waiver_base_trigger());
        // gci i10/rh finding 10: HotSpot's floor is strict and against capacity.
        let floor_ok = if self.mixed_hotspot_choice {
            mixed_choice::waste_allows_mixed(
                reclaimable,
                self.heap_capacity(),
                self.config.heap_waste_percent,
            )
        } else {
            // gci i13/ef finding 11: of the region grid, not `-Xmx`.
            reclaimable * 100 >= self.heap_grid_bytes() * self.config.heap_waste_percent as usize
        };
        let mut verdict = tight || floor_ok;
        // LANE W7-L — was the WAIVER load-bearing, or did the floor pass on its
        // own? W6-M named the waiver as the screen that should have caught the
        // futile loop, from the mechanism, and the mechanism admits both
        // readings: `reclaimable` is `cursor - live_bytes` summed over the
        // candidates, so the same stale-low `live_bytes` that makes a region
        // look collectable also makes the floor pass without the waiver's
        // help. One add settles which. See `mixed_work_waived_tight`.
        if verdict && !floor_ok {
            self.mixed_work_waived_tight.fetch_add(1, Ordering::Relaxed);
        }
        // LANE W7-L — and the one screen on this path that does not consult
        // `live_bytes`: what the last few mixed pauses MEASURED. Applied to
        // the whole verdict rather than only to the waiver, because both of
        // the terms above are estimates derived from the number the defect
        // corrupts, and a measurement of four consecutive pauses beats an
        // estimate of what a fifth would do. Unarmed it counts and refuses
        // nothing. See `mixed_futile_breaker_allows_within`.
        //
        // The order matters for the CENSUS, not for the verdict: W6-M's
        // `refused_waste` is incremented first and keeps meaning exactly what
        // it meant — "candidates existed and the waste floor turned them
        // down". A breaker refusal is a different finding with a different
        // lever behind it and is counted under `refused_futile` alone, so the
        // two arms of this round's A/B produce comparable `refused_waste`
        // columns.
        //
        // LANE W6-M — a refusal HERE means candidates existed and the waste
        // floor turned them down, which is a different finding from "no Old
        // region qualified" and wants a different lever.
        if !verdict {
            self.mixed_work_refused_waste
                .fetch_add(1, Ordering::Relaxed);
        }
        if verdict && !self.mixed_futile_breaker_allows_within(futile_guard) {
            verdict = false;
        }
        if gc_flags().g1_dbg_reach {
            eprintln!(
                "[g1][MIXWORK] old={} live_gt0={} candidates={candidates} reclaimable={reclaimable} free={free} tight={tight} verdict={verdict}",
                regions
                    .iter()
                    .filter(|r| r.region_type() == RegionType::Old)
                    .count(),
                regions
                    .iter()
                    .filter(|r| r.region_type() == RegionType::Old && r.live_bytes > 0)
                    .count(),
            );
        }
        verdict
    }

    /// LANE W7-L — record what a mixed pause actually did, at the one funnel
    /// every mixed pause returns through.
    ///
    /// # Why the ratio and not the old-generation delta
    ///
    /// The sharpest possible statement of the defect is "the old generation
    /// did not shrink", and that is measurable here: snapshot Old occupancy
    /// under the pause's own write guard before and after Phase 5. It was
    /// rejected, and the reason is promotion. A mixed pause collects the young
    /// generation too, so survivors aging out land in Old during the same
    /// pause; on a promoting workload a mixed pause that reclaimed a great
    /// deal of old data can still leave Old occupancy flat or higher. That
    /// screen would refuse exactly the workloads that need mixed pauses most,
    /// which is the failure direction a collector cannot afford.
    ///
    /// So the measurement is the whole pause's `bytes_freed / bytes_copied`,
    /// which is what W6-M's signature is stated in and what the healthy
    /// baseline has 257 samples of. Its known weakness is the same young half:
    /// a LARGE young generation dilutes the ratio upward, so a collector whose
    /// old half is futile but whose young half is productive reads as healthy.
    /// That is a false NEGATIVE — the breaker declines to fire — and it is the
    /// safe direction for a screen that can refuse work. It also means this
    /// screen cannot be the whole answer to W6-M §6.2 ("the old half of a
    /// mixed pause's copying is not separated"), and the page says so.
    ///
    /// # What is deliberately not a sample
    ///
    /// A pause that copied NOTHING is not evidence about copying: `0/0` has no
    /// ratio, and the case it describes — every candidate turned out to be
    /// JIT-pinned, so the pause was mixed in name only — is a different defect
    /// with a different counter (`cand_pinned`). It leaves the streak alone
    /// rather than resetting it, because resetting would let an alternating
    /// futile/empty sequence hold the breaker open forever.
    ///
    /// A drain pass is not a sample either; the caller applies W4-B's rule.
    ///
    /// gci i17/mx: the bar is [`mixed_choice::mixed_pause_productive`] --
    /// the legacy [`MIXED_PRODUCTIVE_RATIO_PCT`] OR "freed more than the live
    /// threshold allows a candidate-only pause to free"
    /// (`freed * 85 > copied * 100` at the default). A pause over HotSpot-legitimate candidates
    /// (67-85 % live) used to read futile here, trip the breaker after four
    /// pauses and lose the mixed phase for ~32 marking cycles (page
    /// `gci-i15orch-g1-ooms-on-the-mixed-names-probe-at-96m-where-hotspot-does-not-FIXED-20261017`).
    pub fn note_mixed_productivity(&self, bytes_copied: usize, bytes_freed: usize) {
        if bytes_copied == 0 {
            return;
        }
        if mixed_choice::mixed_pause_productive(
            bytes_copied,
            bytes_freed,
            MIXED_PRODUCTIVE_RATIO_PCT,
            self.config.mixed_gc_live_threshold_percent,
        ) {
            self.mixed_productive_pauses.fetch_add(1, Ordering::Relaxed);
            let was = self.mixed_futile_streak.swap(0, Ordering::Relaxed);
            if was >= MIXED_FUTILE_STREAK_TRIP {
                self.report_futile_transition("closed", bytes_copied, bytes_freed);
            }
        } else {
            self.mixed_futile_pauses.fetch_add(1, Ordering::Relaxed);
            let now = self.mixed_futile_streak.fetch_add(1, Ordering::Relaxed) + 1;
            if now == MIXED_FUTILE_STREAK_TRIP {
                self.report_futile_transition("tripped", bytes_copied, bytes_freed);
            }
        }
    }

    /// LANE W7-L — one line per breaker TRANSITION, on `--verbose:gc`.
    ///
    /// # Why this is not the summary line
    ///
    /// `print_gc_summary` prints the whole census, and it is reached only from
    /// `vm-cli`'s normal-return teardown — so a run that is KILLED never
    /// prints it. The runs this instrument exists for are exactly the ones
    /// that get killed: W6-M's reproducer spun for 24 minutes and was cut at a
    /// `timeout`, and a census only a terminating run emits cannot describe a
    /// failure to terminate. This is the same defect class the wave-6 sweep
    /// found in `degradation_exit_summary`, which returns early in precisely
    /// the state its counter certifies.
    ///
    /// It is bounded by CONSTRUCTION rather than rate-limited, which is the
    /// other half of that lesson: a trip happens once per episode and a probe
    /// is one poll in [`MIXED_FUTILE_PROBE_EVERY`], so the 5 397-pause loop
    /// would emit about 170 lines over 24 minutes. Each line carries the
    /// cumulative counters, so the LAST one before the kill is the census.
    pub fn report_futile_transition(&self, what: &str, bytes_copied: usize, bytes_freed: usize) {
        if !self.gc_log_enabled.load(Ordering::Relaxed) {
            return;
        }
        let (futile, productive, streak, polls, refused, probes, _waived) =
            self.mixed_futile_census();
        eprintln!(
            "[GC] g1 mixed-futile {what}: copied={bytes_copied} freed={bytes_freed} \
             futile={futile} productive={productive} streak={streak} \
             breaker_polls={polls} refused={refused} probes={probes} guard={}",
            true,
        );
    }

    /// LANE W7-L — may the mixed phase proceed, given what the last few mixed
    /// pauses measured?
    ///
    /// `true` for every caller until [`MIXED_FUTILE_STREAK_TRIP`] consecutive
    /// mixed pauses have each reclaimed less than
    /// [`MIXED_PRODUCTIVE_RATIO_PCT`] of what they copied. After that the
    /// breaker is tripped and refuses all but one poll in
    /// [`MIXED_FUTILE_PROBE_EVERY`]; the pause that probe admits either
    /// reclaims — which resets the streak and closes the breaker — or does
    /// not, and nothing changes.
    ///
    /// # It counts in both arms
    ///
    /// `mixed_futile_breaker_polls` is incremented before `futile_guard` is
    /// consulted, so the ENGAGEMENT COUNT for this fix is printed by an
    /// unarmed run as well as an armed one. Round rule 3 asks how many times
    /// the thing being measured happened; this is the one number that answers
    /// it for a lever whose whole effect is a refusal, and taking it only in
    /// the treatment arm would leave the control arm unable to say whether it
    /// was a control of anything. The cost in the unarmed arm is one relaxed
    /// add per poll, on a path that has already stopped the world.
    ///
    /// # No assertion
    ///
    /// The condition this detects is a collector doing measurable, bounded,
    /// pointless work — not a corruption — and it is detected ON A PAUSE PATH.
    /// A `debug_assert!` here would abort a debug build in the one state the
    /// screen exists to survive, and would be compiled out of exactly the
    /// release builds where the loop was observed. It is counted and reported
    /// instead: `[GC] g1 mixed-futile:`.
    pub fn mixed_futile_breaker_allows_within(&self, futile_guard: bool) -> bool {
        // gci i18/mx -- fresh mark data is a fresh trial on the switch arm
        // (`CRATONVM_G1_MIXED_FUTILE_RESET_PER_CYCLE`): the first poll of a
        // phase restarts the streak. The flag is kept in both arms so the
        // phase bookkeeping is the same; see `mixed_choice::futile_streak_restarts`.
        let first_poll_of_phase = !self.mixed_futile_phase_polled.swap(true, Ordering::Relaxed);
        if mixed_choice::futile_streak_restarts(self.mixed_futile_reset_per_cycle, first_poll_of_phase)
        {
            let was = self.mixed_futile_streak.swap(0, Ordering::Relaxed);
            if was >= MIXED_FUTILE_STREAK_TRIP {
                self.mixed_futile_cycle_resets
                    .fetch_add(1, Ordering::Relaxed);
                self.report_futile_transition("cycle-reset", 0, 0);
            }
        }
        if self.mixed_futile_streak.load(Ordering::Relaxed) < MIXED_FUTILE_STREAK_TRIP {
            return true;
        }
        let polls = self
            .mixed_futile_breaker_polls
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        // One cadence, both arms: the unarmed arm emits a line wherever the
        // armed arm would emit its probe, so the two runs' logs can be read
        // against each other line for line instead of one of them being a
        // silence. See `report_futile_transition` on why a killed run needs
        // this at all.
        let at_probe = MIXED_FUTILE_PROBE_EVERY == 0 || polls % MIXED_FUTILE_PROBE_EVERY == 0;
        if !futile_guard {
            if at_probe {
                self.report_futile_transition("unarmed", 0, 0);
            }
            return true;
        }
        if at_probe {
            self.mixed_futile_probes.fetch_add(1, Ordering::Relaxed);
            self.report_futile_transition("probe", 0, 0);
            return true;
        }
        self.mixed_work_refused_futile
            .fetch_add(1, Ordering::Relaxed);
        false
    }

    /// LANE W7-L — the futile-copy census, as
    /// `(futile_pauses, productive_pauses, streak, breaker_polls,
    /// refused_futile, probes, waived_tight)`.
    ///
    /// A separate accessor rather than four more fields on
    /// [`Self::mixed_refusal_census`], so the `polls == no_cycle + no_work +
    /// entered` invariant that eleven-tuple exists to let a test assert keeps
    /// its shape.
    ///
    /// Per-collector, not process-global, so a test may read absolutes.
    pub fn mixed_futile_census(&self) -> (u64, u64, u64, u64, u64, u64, u64) {
        (
            self.mixed_futile_pauses.load(Ordering::Relaxed),
            self.mixed_productive_pauses.load(Ordering::Relaxed),
            self.mixed_futile_streak.load(Ordering::Relaxed),
            self.mixed_futile_breaker_polls.load(Ordering::Relaxed),
            self.mixed_work_refused_futile.load(Ordering::Relaxed),
            self.mixed_futile_probes.load(Ordering::Relaxed),
            self.mixed_work_waived_tight.load(Ordering::Relaxed),
        )
    }

    /// gci i18/mx -- `(skipped_samples, cycle_resets)`: mixed-driver pauses
    /// left out of the breaker's streak (evacuation failure / compaction), and
    /// tripped streaks the `CRATONVM_G1_MIXED_FUTILE_RESET_PER_CYCLE` arm
    /// restarted at a phase start. Per collector.
    pub fn mixed_futile_i18_census(&self) -> (u64, u64) {
        (
            self.mixed_futile_skipped_samples.load(Ordering::Relaxed),
            self.mixed_futile_cycle_resets.load(Ordering::Relaxed),
        )
    }

    /// g1ep7: a mixed pause whose old half came out empty because every
    /// candidate left was in this pause's pinned set ends the mixed phase.
    /// `mixed_phase_has_work` cannot see the pinned set, so with the
    /// tight-heap waiver it kept answering yes for the same pinned candidates
    /// pause after pause: `ZipFsFinalizerChurn 60 -Xmx512m` ran 50+ such
    /// "mixed" pauses (`candidates=2` every time) while no new mark could
    /// start, and threw `OutOfMemoryError` in 3 of 3 runs on rr6. HotSpot
    /// drops a candidate it cannot collect and ends the phase when none is
    /// left. Returns whether it ended the phase.
    pub fn end_mixed_phase_if_only_pinned_left(&self, choice: &OldCsetChoice) -> bool {
        if !choice.picked.is_empty() || choice.excluded == 0 || !self.needs_mixed_gc() {
            return false;
        }
        self.mixed_phase_ended_pinned.fetch_add(1, Ordering::Relaxed);
        self.end_mixed_phase("every candidate left is pinned");
        true
    }

    /// Item 2 — close the mixed phase before its pause budget is spent.
    pub fn end_mixed_phase(&self, why: &str) {
        tracing::debug!("g1: ending the mixed phase early — {why}");
        self.mixed_gc_remaining.store(0, Ordering::Relaxed);
        self.marking_complete.store(false, Ordering::Relaxed);
        // gci i11/go (finding 9).
        self.prepare_mixed_pending.store(false, Ordering::Relaxed);
        // gci i18/mx: the next phase's first breaker poll is a fresh trial.
        self.mixed_futile_phase_polled.store(false, Ordering::Relaxed);
    }

    /// LANE W6-M — the mixed-pause refusal census, as
    /// `(polls, no_cycle, no_work, entered, cand_no_data, cand_too_live,
    /// cand_pinned, refused_waste, capped_percent, capped_budget,
    /// old_regions)`.
    ///
    /// The same eleven numbers the `[GC] g1 mixed-census:` line prints,
    /// reachable from a test — for the reason `block_offset_census` gives
    /// about its own three: a test that drives a workload and asserts only
    /// that a mixed pause happened is indistinguishable from a test whose
    /// gate never armed, and this round has retracted three measurements to
    /// exactly that shape.
    ///
    /// `polls == no_cycle + no_work + entered` is an invariant of the gate in
    /// `collect_garbage`; a test may assert it, and a future refactor that
    /// adds a fourth arm without a counter will break it there rather than
    /// silently.
    #[allow(clippy::type_complexity)]
    pub fn mixed_refusal_census(&self) -> (u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) {
        (
            self.mixed_gate_polls.load(Ordering::Relaxed),
            self.mixed_gate_no_cycle.load(Ordering::Relaxed),
            self.mixed_gate_no_work.load(Ordering::Relaxed),
            self.mixed_pauses_entered.load(Ordering::Relaxed),
            self.mixed_cand_no_mark_data.load(Ordering::Relaxed),
            self.mixed_cand_too_live.load(Ordering::Relaxed),
            self.mixed_cand_pinned.load(Ordering::Relaxed),
            self.mixed_work_refused_waste.load(Ordering::Relaxed),
            self.mixed_cset_capped_percent.load(Ordering::Relaxed),
            self.mixed_cset_capped_budget.load(Ordering::Relaxed),
            self.mixed_old_regions_selected.load(Ordering::Relaxed),
        )
    }

    // -----------------------------------------------------------------------
    // Fallible allocation (Phase 89)
    // -----------------------------------------------------------------------

    /// Try to allocate a Java object. Returns `None` when Eden is exhausted
    /// (caller should trigger GC and retry).
    ///
    /// Leaves the last [`Self::tlab_reserve_regions`] Free regions to the
    /// callers that cannot report failure; see
    /// Try to allocate a Java object. Returns `None` when Eden is exhausted
    /// (leaves the reserve for infallible callers).
    pub fn try_alloc_object(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        // gc-common w3-c: plan the COMPACT body exactly as this collector's
        // own infallible `GarbageCollector::alloc_object` does.
        let compact_body = cratonvm_types::compact_object_total_size(
            self.layout_domain(),
            class_id.as_u32(),
            num_fields,
        );
        // M6 (round-12 gc): checked `+ HEADER_SIZE` to match `try_alloc_array`
        // and gen_heap; a near-`usize::MAX` field count must not wrap.
        let total_size = match compact_body {
            Some(total) => total,
            None => HEADER_SIZE.checked_add(num_fields.checked_mul(SLOT_SIZE)?)?,
        };
        let num_slots = u32::try_from(num_fields).ok()?;
        let init = |ptr: *mut u8| {
            let mut header = ObjectHeader::new(
                class_id,
                ObjectKind::Object,
                ArrayElementType::Reference,
                0,
                num_slots,
            );
            if let Some(body) = compact_body {
                header.set_compact_shape(num_slots, body);
            }
            // SAFETY: `ptr` is the base of a span this allocation has just
            // reserved and not yet published; it is 8-aligned and at least
            // `HEADER_SIZE` bytes.
            unsafe { std::ptr::write(ptr as *mut ObjectHeader, header) };
        };
        let (ptr, _region) = self.alloc_fallible_initialized(total_size, init)?;
        // SAFETY: the initializer above wrote a well-formed header at `ptr`.
        unsafe { Some(ObjectRef::from_raw(ptr)) }
    }

    /// Allocate and pre-initialize primitive-typed slots based on JVM
    /// field descriptor bytes. See
    /// [`cratonvm_gc_core::heap::default_value_for_descriptor`] for the mapping.
    pub fn alloc_object_with_descriptors(
        &self,
        class_id: ClassId,
        num_fields: usize,
        descriptor_bytes: &[u8],
    ) -> ObjectRef {
        // Use the GarbageCollector trait's alloc_object to get the raw
        // object, then populate primitive slots.
        let obj = GarbageCollector::alloc_object(self, class_id, num_fields);
        for i in 0..num_fields {
            let default = descriptor_bytes
                .get(i)
                .and_then(|desc| cratonvm_gc_core::heap::default_value_for_descriptor(*desc))
                .unwrap_or(Value::Object(None));
            GarbageCollector::set_field(self, obj, i, default);
        }
        obj
    }

    /// Fallible variant of [`Self::alloc_object_with_descriptors`].
    pub fn try_alloc_object_with_descriptors(
        &self,
        class_id: ClassId,
        num_fields: usize,
        descriptor_bytes: &[u8],
    ) -> Option<ObjectRef> {
        let obj = self.try_alloc_object(class_id, num_fields)?;
        for i in 0..num_fields {
            let default = descriptor_bytes
                .get(i)
                .and_then(|desc| cratonvm_gc_core::heap::default_value_for_descriptor(*desc))
                .unwrap_or(Value::Object(None));
            GarbageCollector::set_field(self, obj, i, default);
        }
        Some(obj)
    }

    /// Try to allocate a Java array. Returns `None` when Eden is exhausted.
    pub fn try_alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        let data_size = array_data_size(length, element_type).ok()?;
        let total_size = ARRAY_DATA_OFFSET.checked_add(data_size)?;
        let length_u32 = u32::try_from(length).ok()?;
        let init = |ptr: *mut u8| {
            // Mirror `length` into BOTH `array_length` and `num_slots`.
            // SAFETY: as in `try_alloc_object`.
            unsafe {
                std::ptr::write(
                    ptr as *mut ObjectHeader,
                    ObjectHeader::new(
                        class_id,
                        ObjectKind::Array,
                        element_type,
                        length_u32,
                        length_u32,
                    ),
                );
            }
        };
        let (ptr, _region) = self.alloc_fallible_initialized(total_size, init)?;
        // SAFETY: the initializer above wrote a well-formed header at `ptr`.
        unsafe { Some(ObjectRef::from_raw(ptr)) }
    }

    /// The allocation body of the FALLIBLE entries ([`Self::try_alloc_object`],
    /// [`Self::try_alloc_array`]): [`Self::alloc_in_region_initialized`], except
    /// that it never takes the last [`Self::tlab_reserve_regions`] Free regions.
    ///
    /// # Whose reserve it is (gc-common w3-c)
    ///
    /// `refill_tlab` has always stopped at the reserve, so that the last Free
    /// regions serve the callers that cannot be told "no": the infallible
    /// `GarbageCollector::alloc_object` / `alloc_array` behind class mirrors,
    /// non-unwinding native contexts and JNI object minting, which `abort()`
    /// the process when they find nothing. But these per-object fallible
    /// entries took the reserve like any other allocation, and they are what
    /// every VM allocation ladder retries through after its forced
    /// collections. So on a heap filling with LIVE data the ladder's last
    /// retries ate the reserve, the ladder then threw its catchable
    /// `OutOfMemoryError` a little later, and the first infallible caller after
    /// it -- the `catch` block's string concatenation, whose `invokedynamic`
    /// bootstrap mints class mirrors -- found no region and aborted
    /// (`GcOverheadProbe -XX:+UseG1GC`, rc=127; see
    /// `docs/feature-designs/concurrent-gc-maturation.md`).
    ///
    /// A fallible caller that is refused here either collects and retries (the
    /// interpreter and JIT ladders) or raises `OutOfMemoryError`; either way it
    /// no longer spends the headroom the aborting callers depend on. At the
    /// reserve it may still bump inside its stripe's CURRENT Eden region --
    /// that claims no Free region -- and a humongous request is refused when it
    /// would leave fewer than the reserve behind. The latch is armed on every
    /// refusal, exactly as `refill_tlab`'s is.
    ///
    /// The pool is read without the regions lock (the maintained count), so two
    /// racing claimers can each see one region above the reserve and together
    /// take one of it: the reserve is a bound up to the number of threads that
    /// race for the last region, not an exact one. A heap too small to hold a
    /// reserve (fewer than 8 regions) is unaffected.
    ///
    /// Behaviour change, G1 only and only at the very edge of exhaustion: a
    /// fallible allocation fails about `reserve` regions (2 of 128 at
    /// `-Xmx128m`, ~1.6 %) earlier. A native allocating inside a callback
    /// cannot collect first, so it now reports `OutOfMemoryError` at that edge
    /// rather than at the very last region.
    ///
    /// gci nd48: a small request at the reserve may still claim a Free region
    /// that a collection has given back since the reserve last refused one
    /// ([`Self::alloc_from_reclaim_credit`]), so a heap churning garbage at the
    /// reserve is not refused where HotSpot (which has no reserve) allocates.
    pub fn alloc_fallible_initialized(
        &self,
        size: usize,
        init: impl Fn(*mut u8),
    ) -> Option<(*mut u8, usize)> {
        let region_size = self.config.region_size;
        // `regions.len()` without the lock: the table is sized once, at
        // construction, to exactly this.
        let reserve = self.tlab_reserve_regions(self.config.heap_size / region_size.max(1));
        let humongous = size > region_size / 2;
        let claims = if humongous {
            size.div_ceil(region_size.max(1)).max(1)
        } else {
            1
        };
        let free = self.free_region_count.load(Ordering::Relaxed);
        if reserve == 0 || free >= claims.saturating_add(reserve) {
            // gci nd48: the pool is out of the reserve, so a refusal recorded
            // there no longer describes it.
            if self.reserve_credit_used.load(Ordering::Relaxed) != RESERVE_CREDIT_UNARMED {
                self.reserve_credit_used.store(RESERVE_CREDIT_UNARMED, Ordering::Relaxed);
            }
            return self.alloc_in_region_initialized(size, init);
        }
        // At (or into) the reserve.
        self.native_alloc_pressure.store(true, Ordering::Relaxed);
        if humongous {
            // gci i13/oa finding 1 (opt-in `CRATONVM_G1_HUMONGOUS_RESERVE_ONE`,
            // `gci-i13oa-g1-allocation-slow-path-and-oome-review-findings-RETIRED-UNFIXED-20261018`):
            // HotSpot G1 has no reserve, so a humongous array of up to every
            // Free region is served there, and here it was refused `reserve`
            // regions early (2 of 16 at -Xmx16m, 16 of 1024 at -Xmx1g): an
            // `OutOfMemoryError` after the whole ladder where HotSpot allocates.
            // Under the switch a humongous claim keeps ONE region back for the
            // infallible callers (a class mirror, a JNI handle's object: small
            // objects that need one Eden region), not the whole reserve. The
            // pressure latch is armed above, as for every use of the reserve.
            if humongous_may_take_reserve(free, claims, reserve) && g1_humongous_reserve_one() {
                return self.alloc_in_region_initialized(size, init);
            }
            // Counted like any other failed humongous request, so the P6
            // request/failure census still adds up; the free/longest pair it
            // records says this was not fragmentation.
            self.note_humongous_request();
            // gci i47w hr47: a compaction reservation kept for THIS request
            // size can no longer be consumed by it -- the retry was refused
            // here, before the claim that releases it -- so it must not outlive
            // the refusal and steer (or, before hr47, starve) every later
            // region claim. A later arming installs a fresh one.
            {
                let mut reservation = self.humongous_compaction_avoid.lock();
                if reservation
                    .as_ref()
                    .is_some_and(|r| r.requested_regions == claims)
                {
                    *reservation = None;
                }
            }
            let (free_now, longest) = free_region_shape(&self.regions.read()[..]);
            self.note_humongous_failure(free_now, longest, claims);
            tracing::debug!(
                "g1: fallible humongous allocation of {size} bytes ({claims} regions) refused: \
                 {free_now} Free region(s) and a reserve of {reserve} for infallible callers"
            );
            return None;
        }
        if let Some(hit) = self.bump_current_eden_initialized(size, &init) {
            return Some(hit);
        }
        self.alloc_from_reclaim_credit(size, free, &init)
    }

    /// gci nd48 (`gci-i48-g1-native-door-oome-after-ladder-RETIRED-UNFIXED-20261018`): the last
    /// arm of [`Self::alloc_fallible_initialized`] for a small request at the
    /// reserve whose stripe has no current Eden room. Claim a Free region when
    /// collections have given back at least half a region of bytes, per region
    /// this arm has claimed, since the reserve last refused a request, and the
    /// claim leaves one Free region ([`reserve_credit_admits`]). Otherwise
    /// refuse, and record today's occupancy as the new refusal point.
    ///
    /// Why: every allocation ladder (the interpreter's `new`, the native doors,
    /// the JIT helpers) retries through this entry after its collections, and
    /// the reserve refused those retries until the pool held `1 + reserve` Free
    /// regions. On `Zr8NrNativeDoors allocinst -Xmx64m` (reserve 2) live data
    /// leaves the pool at the reserve: a young pause that empties the garbage
    /// Eden and claims a Survivor region leaves 2 Free regions, and the retry
    /// after a full collection and two compaction pauses was still refused
    /// (`door_oome=12`; the `=0` twin threw out of its `println`).
    /// HotSpot G1 has no reserve and allocates there.
    ///
    /// The reserve still does its job (gc-common w3-c): on a heap filling with
    /// LIVE data a collection gives back no bytes, so no credit is earned and
    /// the reserve stays for the infallible callers; and a credited claim
    /// never takes the last Free region. `CRATONVM_G1_RESERVE_RECLAIM_CREDIT=0`
    /// restores the plain refusal. The two cells are read and written without
    /// the regions lock, so racing threads may each spend one credit: a bound
    /// up to the number of racers, as the reserve itself is.
    pub fn alloc_from_reclaim_credit(
        &self,
        size: usize,
        free: usize,
        init: &dyn Fn(*mut u8),
    ) -> Option<(*mut u8, usize)> {
        // The G1 near-full proxy-define page: the ladder's last chance (see
        // `arm_reserve_last_chance`). Never the last Free region.
        if free >= 2 && self.reserve_last_chance.swap(false, Ordering::AcqRel) {
            if let Some(hit) = self.alloc_in_region_initialized(size, init) {
                self.reserve_last_chance_claims.fetch_add(1, Ordering::Relaxed);
                return Some(hit);
            }
        }
        let used = GarbageCollector::allocated_bytes(self);
        let armed_at = self.reserve_credit_used.load(Ordering::Acquire);
        let spent = self.reserve_credit_spent.load(Ordering::Relaxed);
        if armed_at != RESERVE_CREDIT_UNARMED
            && reserve_credit_admits_at(
                free,
                armed_at.saturating_sub(used),
                spent,
                reserve_credit_per_claim(self.config.region_size, true),
            )
        {
            if let Some(hit) = self.alloc_in_region_initialized(size, init) {
                self.reserve_credit_spent.fetch_add(1, Ordering::Relaxed);
                self.reserve_credit_claims.fetch_add(1, Ordering::Relaxed);
                return Some(hit);
            }
        }
        self.reserve_credit_refusals.fetch_add(1, Ordering::Relaxed);
        self.reserve_credit_spent.store(0, Ordering::Relaxed);
        self.reserve_credit_used.store(used, Ordering::Release);
        None
    }

    /// r23 w6: one self-forward made by either evacuator
    /// (`self_forward_tracking`).
    pub fn note_self_forward(&self, addr: usize) {
        self.self_forward_count.fetch_add(1, Ordering::AcqRel);
        self.self_forwarded_addrs.lock().push(addr);
    }

    /// g1pin: the address span `[lo, hi)` of region `idx`.
    pub fn region_span(&self, idx: usize) -> (usize, usize) {
        let lo = self.arena_base + (idx << self.region_shift);
        (lo, lo + self.config.region_size)
    }

    /// g1pin: `f(key, value)` for every forward of `map` whose key lies in one
    /// of `regions`. The sorted form visits only those regions' key ranges
    /// ([`cratonvm_types::PointerMap::range`]); the hashed form filters one
    /// full iteration.
    pub fn for_each_forward_in_regions(
        &self,
        map: &cratonvm_types::PointerMap,
        regions: &RegionSet,
        mut f: impl FnMut(usize, usize),
    ) {
        if !map.is_sorted_form() {
            for (&k, &v) in map.iter() {
                if self.lookup_region_for_addr(k).is_some_and(|i| regions.contains(&i)) {
                    f(k, v);
                }
            }
            return;
        }
        for idx in regions.iter() {
            let (lo, hi) = self.region_span(idx);
            map.range(lo, hi).for_each(|(&k, &v)| f(k, v));
        }
    }

    /// g1pin: `f(key, value)` for every forward of `map` whose key lies in
    /// none of `excluded` (a key outside the arena included). The sorted form
    /// visits only the key ranges between the excluded regions; the hashed
    /// form filters one full iteration.
    pub fn for_each_forward_outside_regions(
        &self,
        map: &cratonvm_types::PointerMap,
        excluded: &RegionSet,
        mut f: impl FnMut(usize, usize),
    ) {
        if !map.is_sorted_form() {
            for (&k, &v) in map.iter() {
                if !self.lookup_region_for_addr(k).is_some_and(|i| excluded.contains(&i)) {
                    f(k, v);
                }
            }
            return;
        }
        let mut from = 0usize;
        for idx in excluded.iter() {
            let (lo, hi) = self.region_span(idx);
            map.range(from, lo).for_each(|(&k, &v)| f(k, v));
            from = hi;
        }
        map.range(from, usize::MAX).for_each(|(&k, &v)| f(k, v));
    }

    /// g1pin: the identity keys of `pointer_map`, one pass's map. Outside a
    /// tracked collection, the map is scanned; inside one, they are the
    /// addresses the pass self-forwarded ([`Self::note_self_forward`]) whose
    /// entry is an identity, found without visiting the rest of the map.
    pub fn identity_keys(&self, pointer_map: &cratonvm_types::PointerMap) -> Vec<usize> {
        if self.no_self_forward_this_pause() {
            return Vec::new();
        }
        if !self.self_forward_tracking.load(Ordering::Acquire) {
            return pointer_map.iter().filter(|(k, v)| k == v).map(|(&k, _)| k).collect();
        }
        let mut keys: Vec<usize> = self
            .self_forwarded_addrs
            .lock()
            .iter()
            .copied()
            .filter(|a| pointer_map.get(a) == Some(a))
            .collect();
        keys.sort_unstable();
        keys.dedup();
        keys
    }

    /// r23 w6: true when this pause is tracked and made no self-forward, so
    /// the forwarding map holds no identity entry and an identity scan of it
    /// would find nothing.
    pub fn no_self_forward_this_pause(&self) -> bool {
        self.self_forward_tracking.load(Ordering::Acquire)
            && self.self_forward_count.load(Ordering::Acquire) == 0
    }

    /// The G1 near-full proxy-define page: the VM's allocation ladder has run
    /// its two full collections (`last_ditch_reclaim`) and is about to make
    /// its final attempt, after which it throws `OutOfMemoryError`. That one
    /// attempt may take a reserve region, as long as one Free region stays for
    /// the infallible callers: HotSpot G1, which keeps no reserve, allocates
    /// there. Measured on `Zs3Nd3UpcallMints proxy 4096 -Xmx64m`: every
    /// thrown door had 2 Free regions (the reserve) after both full
    /// collections, with `old=` unchanged across the whole ladder.
    ///
    /// `CRATONVM_G1_RESERVE_LAST_CHANCE=0` leaves it unarmed.
    pub fn arm_reserve_last_chance(&self) {
        {
            self.reserve_last_chance.store(true, Ordering::Release);
        }
    }

    /// Claims [`Self::arm_reserve_last_chance`] granted (census).
    pub fn reserve_last_chance_claims(&self) -> u64 {
        self.reserve_last_chance_claims.load(Ordering::Relaxed)
    }

    /// gci nd48: `(claims, refusals)` of [`Self::alloc_from_reclaim_credit`].
    pub fn reserve_credit_counts(&self) -> (u64, u64) {
        (
            self.reserve_credit_claims.load(Ordering::Relaxed),
            self.reserve_credit_refusals.load(Ordering::Relaxed),
        )
    }

    /// Bump `size` bytes inside this thread's stripe's CURRENT Eden region,
    /// never claiming a Free one: the arm of [`Self::alloc_fallible_initialized`]
    /// that runs once the pool is down to the reserve. The same probe as
    /// `alloc_in_region_locked`'s fast path (and, with
    /// `CRATONVM_G1_SHARED_ALLOC=0`, under the exclusive guard as that path
    /// then is).
    pub fn bump_current_eden_initialized(
        &self,
        size: usize,
        init: &dyn Fn(*mut u8),
    ) -> Option<(*mut u8, usize)> {
        let slot = self.eden_slot();
        let probe = |regions: &[G1Region]| {
            let cur = self.eden_slots[slot].load(Ordering::Relaxed);
            if cur < regions.len()
                && regions[cur].region_type() == RegionType::Eden
                && self.commit_through_region(cur)
            {
                return regions[cur]
                    .bump_alloc_initialized(size, 8, "obj:cur-eden", init)
                    .map(|claimed| {
                        self.note_eden_bytes(size);
                        (claimed.0, cur)
                    });
            }
            None
        };
        {
            probe(&self.regions.read()[..])
        }
    }

    /// Carve a TLAB from the current Eden region.
    ///
    /// Returns `Some((ptr, actual_size))` on success. The TLAB is carved from
    /// the Eden region's bump pointer, exactly like how `refill_tlab` works in
    /// the generational collector but backed by G1 regions.
    pub fn refill_tlab(&self, requested_size: usize) -> Option<(*mut u8, usize)> {
        self.refill_tlab_at_least(requested_size, 0)
    }

    /// gch h4/g1 (`gengc-r5w5-sizer9-refill-can-carve-less-than-the-object`,
    /// the G1 remainder): [`Self::refill_tlab`] with a floor -- the chunk is at
    /// least `min_size` bytes, or there is none. `min_size == 0` is
    /// `refill_tlab` byte for byte.
    ///
    /// The VM asks for a refill because an object of `min_size` (its footprint
    /// on the 8-byte grid) missed its buffer. Before, a current Eden region
    /// with less than that left still handed out its short remainder: the
    /// object failed in the fresh buffer, the refill read as "no TLAB", and
    /// the caller took the slow path with a useless buffer installed. Now a
    /// region that cannot hold the floor is passed over and a fresh one is
    /// claimed, HotSpot's `G1Allocator::attempt_allocation(min_word_size,
    /// desired_word_size, ...)`: the mutator region is retired when the
    /// minimum does not fit (its tail stays unallocated above the cursor, which
    /// every region walker already stops at). A floor above the TLAB bound
    /// (`region_size / 2`, the humongous threshold) has no chunk at all.
    pub fn refill_tlab_at_least(
        &self,
        requested_size: usize,
        min_size: usize,
    ) -> Option<(*mut u8, usize)> {
        let region_size = self.config.region_size;
        if min_size > region_size / 2 {
            return None;
        }
        // The request never asks for less than the floor.
        let requested_size = requested_size.max(min_size);

        // Don't serve TLABs for requests larger than half a region.
        //
        // LANE E (2026-09-20) — CLAMP, do not refuse. The bound itself is
        // load-bearing and unchanged: `may_be_humongous` is exactly
        // `size > region_size / 2`, so every object that fits in a chunk of
        // `region_size / 2` is provably non-humongous and the accessors' lock-
        // free routing stays exact.
        //
        // What was wrong was the REFUSAL. `tlab::TlabPressureTracker` doubles a
        // thread's request on fast fill up to `MAX_TLAB_SIZE` (1 MiB), and the
        // G1 region-size ergonomic gives 1 MiB regions for any heap up to
        // 2 GiB — so the ladder is 256 KiB, 512 KiB, 1 MiB, and the last rung
        // is over the bound. At that point this returned `None` for every
        // subsequent refill, the thread fell back to per-object allocation
        // through the exclusive guard, and it could never climb back out: the
        // tracker only re-sizes when a refill actually happens, and the shrink
        // condition is an IDLE thread, which a thread allocating hard enough to
        // have grown here is not. A hot allocator lost its TLAB permanently for
        // having been hot.
        //
        // The return contract already carries the answer — `(ptr, actual_size)`
        // has always been allowed to be smaller than the request, because
        // `tlab_carve_size` caps by what the region has left — so a caller
        // cannot tell a clamp from an ordinary short carve.
        //
        // `CRATONVM_G1_TLAB_CLAMP=0` restores the refusal on the same binary.
        let tlab_cap = region_size / 2;
        let requested_size = if requested_size > tlab_cap {
            TLAB_OVERSIZE_CLAMPED.fetch_add(1, Ordering::Relaxed);
            tlab_cap
        } else {
            requested_size
        };

        // F-11 fast path: carve out of the current Eden under a SHARED guard.
        //
        // This is the site the finding is about. At the 256 KiB default TLAB
        // (`tlab::DEFAULT_TLAB_SIZE`) against 1 MiB regions, four refills
        // exhaust a region, so every allocating thread came through here
        // constantly and every one of them took the collector's single
        // exclusive lock to do it. The carve is now an atomic claim; only the
        // fourth-refill region change still needs exclusion.
        let slot = self.eden_slot();
        {
            let regions = self.regions.read();
            if let Some(carve) = self.carve_tlab_from_eden_slot(
                &regions,
                slot,
                requested_size,
                min_size,
                "tlab:cur-eden",
            ) {
                self.tlab_shared_carves.fetch_add(1, Ordering::Relaxed);
                return Some(carve);
            }
        }

        // Find a new free region for Eden and carve TLAB from it.
        let mut regions = self.regions.write();
        self.tlab_exclusive_carves.fetch_add(1, Ordering::Relaxed);

        // Re-probe the current Eden under the write guard: another thread may
        // have installed a fresh region while we queued, and with the shared
        // fast path off this is the ONLY probe (the pre-F-11 behaviour).
        if let Some(carve) = self.carve_tlab_from_eden_slot(
            &regions,
            slot,
            requested_size,
            min_size,
            "tlab:cur-eden",
        ) {
            return Some(carve);
        }

        // Emergency reserve: a TLAB refill is a *speculative bulk* claim (the
        // thread may retire the chunk with most of it unused), so once the
        // Free pool is down to the reserve, stop serving refills and leave
        // those regions to the per-object allocator — which, unlike this one,
        // has callers that cannot be told "no" (`GarbageCollector::
        // alloc_object` aborts the process). See `tlab_reserve_regions`.
        // Item 6: the maintained count, not a table scan.
        let free_count = self.free_region_count.load(Ordering::Relaxed);
        if free_count <= self.tlab_reserve_regions(regions.len()) {
            // Latch the pressure signal on the way out: the workload is
            // allocating hard enough to exhaust the pool and something must
            // ask for a collection.
            self.native_alloc_pressure.store(true, Ordering::Relaxed);
            return None;
        }
        if let Some(idx) = self.claim_free_region_young(&regions) {
            debug_free_region_cursor(&regions[idx], idx, "refill_tlab");
            regions[idx].set_region_type(RegionType::Eden);
            self.eden_slots[slot].store(idx, Ordering::Relaxed);
            self.note_region_consumed_locked(&regions);
            // Item 7: the carve zeroes its whole extent (up to a full region),
            // and it used to do so under the exclusive guard, stalling every
            // other allocating thread for the memset. Downgrade first: the
            // region is typed and published in its stripe slot, so the carve
            // below is the same shared-guard `bump_alloc` any other thread
            // would do against it.
            let regions = parking_lot::RwLockWriteGuard::downgrade(regions);
            if let Some(carve) = self.carve_tlab_from_eden_slot(
                &regions,
                slot,
                requested_size,
                min_size,
                "tlab:fresh-eden",
            ) {
                return Some(carve);
            }
        }

        None
    }

    /// Carve a TLAB out of whatever region Eden stripe `slot` currently names.
    ///
    /// Split out of [`Self::refill_tlab`] by F-11 so the identical carve can be
    /// attempted under the shared guard and then, if it failed, under the
    /// exclusive one — the two arms have to agree exactly or the kill switch
    /// would be selecting different behaviour rather than a different lock.
    ///
    /// The retry loop is what the atomic cursor costs. `remaining()` is a
    /// SNAPSHOT: another thread can claim part of it between the read and this
    /// thread's compare-exchange, in which case `bump_alloc` refuses and the
    /// size has to be recomputed against what is actually left. It terminates
    /// because a region's cursor only ever moves up, so each retry either fits
    /// or falls under the 256-byte floor.
    ///
    /// TLAB contract: the returned chunk is fully zeroed. Inline compiled
    /// allocation relies on that for JVM default field values and zero-valued
    /// header words, and Eden regions are recycled WITHOUT clearing their bytes
    /// (`G1Region::reset`), so it has to be established somewhere —
    /// `bump_alloc` establishes it, zeroing exactly the range it claimed. There
    /// is deliberately no second `write_bytes` here; that was memsetting every
    /// TLAB twice. `refill_tlab_zeroes_dirty_eden_bytes` is the oracle.
    pub fn carve_tlab_from_eden_slot(
        &self,
        regions: &[G1Region],
        slot: usize,
        requested_size: usize,
        min_size: usize,
        site: &'static str,
    ) -> Option<(*mut u8, usize)> {
        let cur = self.eden_slots[slot].load(Ordering::Relaxed);
        if cur >= regions.len() || regions[cur].region_type() != RegionType::Eden {
            return None;
        }
        // gch h4/g1: `refill_tlab_at_least`'s floor. A region with less than
        // `min_size` left is not carved (the caller claims a fresh one).
        let floor = min_size.max(256);
        // F-16: no byte above the committed prefix may be touched, and
        // `bump_alloc` zeroes what it hands out. An Eden region was committed
        // when it was claimed, so this is an atomic load and a compare — but it
        // is HERE rather than assumed, because the rule is the thing that has
        // to hold and an assumption is not a rule. Both of `refill_tlab`'s
        // probes (shared and exclusive) funnel through this helper, which is
        // why the check is in it rather than at either call site.
        if !self.commit_through_region(cur) {
            return None;
        }
        loop {
            let remaining = regions[cur].remaining();
            if remaining < floor {
                return None;
            }
            let actual = tlab_carve_size(requested_size, remaining);
            if actual < min_size {
                // Only the 8-byte rounding of an unaligned floor can get here.
                return None;
            }
            if let Some((ptr, _off)) = regions[cur].bump_alloc(actual, 8, site) {
                self.note_eden_bytes(actual);
                return Some((ptr, actual));
            }
        }
    }

    /// Get a reference to the global SATB queue.
    pub fn satb_queue(&self) -> &Arc<SatbQueue> {
        &self.satb_queue
    }

    /// Record an old reference value in the SATB queue (for concurrent marking).
    /// Only records when SATB is active (during concurrent mark phase).
    ///
    /// Routes through the per-thread SATB buffer
    /// ([`cratonvm_gc_core::satb::satb_thread_local_log`]) so the hot write-barrier
    /// path takes no shared lock in the common case; the buffer auto-flushes
    /// into the global queue every ~256 entries.
    ///
    /// Also bumps a per-thread "SATB pre-barrier observed" epoch counter so
    /// the debug-only assertion in `write_barrier` can detect callers that
    /// invoke the post-store barrier without first logging the old slot
    /// value (see `write_barrier` for the SATB protocol contract).
    pub fn satb_pre_barrier(&self, old_ref: usize) {
        bump_satb_pre_barrier_epoch();
        if old_ref == 0 {
            return;
        }
        if self.satb_queue.is_active()
            && cratonvm_gc_core::satb::satb_thread_local_log(&self.satb_queue, old_ref)
        {
            // Item 9b — a thread-local buffer just spilled to the shared queue:
            // that is gray work, so wake a parked marker for it.
            self.wake_marker();
        }
    }

    /// Is the SATB pre-barrier obligation in force for a store happening now?
    ///
    /// gc-common w5-e (`common-g-proposal-one-satb-gate`, finished): this is
    /// `satb_queue.is_active()`, the one gate that actually retains a value.
    /// The phase (`gc_state`) is the marker's business, not the barrier's.
    ///
    /// It used to be the PHASE, plus a `debug_assert!` that the queue was
    /// active whenever the phase said "marking". A store read its old slot on
    /// the phase and retained it on the queue, so a window with the phase open
    /// and the queue closed would read every old value and throw it away — a
    /// silently lost snapshot edge (G1AUD-2). That window was kept shut by the
    /// ORDER of the state changes in [`Self::start_concurrent_mark`],
    /// [`Self::cleanup`] and [`Self::abort_concurrent_mark`]. With one gate it
    /// cannot be expressed: the orders are still right, but no longer
    /// load-bearing for the barrier. The queue also owns the compiled-code
    /// pre-gate (`satb.rs`, `SatbQueue::jit_gate_armed`), so the JIT gate is a
    /// superset of this one by construction.
    ///
    /// The queue's window covers every marking-active phase (activated while
    /// the phase is still `InitialMark`, closed after the phase is `Idle`), so
    /// this admits MORE stores than the phase did, never fewer; the extra ones
    /// fall inside the initial-mark and cleanup pauses.
    #[inline]
    pub fn satb_pre_barrier_required(&self) -> bool {
        self.satb_queue.is_active()
    }

    /// JIT r23 wave 3c, lane L5 (`CRATONVM_G1_GEN_RAW_REF_ARRAYCOPY`, the
    /// caller's switch): may a caller copy reference slots between two arrays
    /// of these TOTAL object sizes word by word and then call
    /// [`Self::post_write_barrier_rset`] once per stored non-null reference?
    /// Exactly when [`G1Collector`]'s own `set_array_element` would take its
    /// lock-free arm with no SATB read for every one of those stores: no
    /// marking window open (`satb_pre_barrier_required`; it opens only inside
    /// a pause, which a caller holding raw references across the copy
    /// excludes) and neither array possibly humongous (`may_be_humongous`: a
    /// humongous payload is reached through `humongous_span`, not as one flat
    /// range). Read-only; no lock.
    pub fn raw_ref_array_copy_admitted(&self, src_total_size: usize, dst_total_size: usize) -> bool {
        !self.satb_pre_barrier_required()
            && !self.may_be_humongous(src_total_size)
            && !self.may_be_humongous(dst_total_size)
    }

    /// G1AUD-5 (defect G1-8) — the current remembered-set *generation*.
    ///
    /// This is [`Self::rset_cache_epoch`], reused as a monotone reclassification
    /// clock: it is bumped (Release, under the regions lock) at the start of
    /// every phase that can recycle or re-type a region, so within one
    /// generation no region is reset. Every rset entry is stamped with the
    /// generation it was recorded in and every recycled region records the
    /// generation it was reset in (`G1Region::recycled_in_generation`); an entry
    /// is dead exactly when `stamp < source.recycled_in_generation()`.
    ///
    /// `Acquire` pairs with the `Release` bump so a mutator that observes a new
    /// generation also observes the reclassification that caused it.
    #[inline]
    pub fn rset_generation(&self) -> u64 {
        self.rset_cache_epoch.load(Ordering::Acquire)
    }

    /// G1AUD-5 — is this remembered-set entry dead?
    ///
    /// True when the source region was recycled *after* the edge was recorded:
    /// [`G1Region::reset`] zero-filled the region and cleared its own rset, so
    /// nothing that could have held the edge survived. An out-of-range source
    /// index is also dead (it can never be walked).
    ///
    /// Deliberately a strict `<`: an edge recorded in the SAME generation that
    /// later reset the source (the Phase-4 rebuild runs before Phase 5's frees)
    /// is retained. That over-retains for one cycle and is the fail-safe
    /// direction — the scan side independently refuses `Free` sources, so a
    /// retained-but-dead entry costs a lookup, while a dropped-but-live one is
    /// a use-after-free.
    pub fn rset_entry_is_stale(regions: &[G1Region], source: usize, recorded_generation: u64) -> bool {
        match regions.get(source) {
            Some(r) => recorded_generation < r.recycled_in_generation(),
            None => true,
        }
    }

    /// G1AUD-5 — the remembered-set sources of `cset` that are still live,
    /// deduplicated.
    ///
    /// Drops entries whose source was recycled since the edge was recorded
    /// (defect G1-8: such a source, once re-typed into a live region, was
    /// otherwise re-walked *wholesale* every pause on behalf of an object that
    /// no longer exists, resurrecting its referents). `Free` sources are left
    /// to `scan_source_region_for_cset_refs`'s own early return, which already
    /// handles them.
    pub fn live_rset_sources(regions: &[G1Region], cset: &[usize]) -> RegionSet {
        let mut set = RegionSet::new();
        // G1AUD-9: the coarsened arm below asks "is region `i` in the CSet?"
        // once per region, and `cset` is a SLICE — so that arm was
        // `O(num_regions x |cset|)`, and a young CSet is every young region in
        // the heap. Hash it once, outside the loop; a coarsened rset is exactly
        // the case where the arm runs.
        let cset_lookup: RegionSet = cset.iter().copied().collect();
        let prune = lane_a_rset_prune_on_scan();
        for &cset_idx in cset {
            // COARSENED (audit §9 item 5): this rset stopped naming individual
            // sources when it hit `rset_source_cap`, so it now asserts only
            // "some region points into me". The sound reading is every region
            // that could hold such an edge — which is what makes coarsening a
            // memory/scan-time trade and not a correctness one. Getting this
            // wrong in the cheap direction (treating a coarsened rset as empty)
            // would drop exactly the live objects the set exists to find.
            if regions[cset_idx].rset.is_coarsened() {
                for (i, r) in regions.iter().enumerate() {
                    if r.region_type() != RegionType::Free && !cset_lookup.contains(&i) {
                        set.insert(i);
                    }
                }
                // Nothing another CSet region can add to a whole-heap set.
                return set;
            }
            // LANE A — prune while we are here, not merely skip.
            //
            // The additive remembered set is the compounding half of the O(heap)
            // young pause (`g1-maturation.md` §1.2/§1.3): the only thing that
            // ever removed an entry was `cleanup`'s pass at the end of a
            // concurrent-mark cycle, so on a workload that never triggers a mark
            // cycle a dead entry is re-hashed and re-judged on every pause for
            // the rest of the run. The judgement cannot change — see
            // `lane_a_rset_prune_on_scan` for why `rset_entry_is_stale` is
            // monotone — so paying for it twice is pure waste, and paying for it
            // forever is the defect.
            //
            // `retain_and_collect_sources` does the prune and the snapshot in
            // one acquisition of the per-rset mutex, and returns only the
            // `usize` half the caller wants, instead of allocating a
            // `Vec<(usize, u64)>` of every entry including the dead ones.
            if prune {
                for source in
                    regions[cset_idx]
                        .rset
                        .retain_and_collect_sources(|source, generation| {
                            !Self::rset_entry_is_stale(regions, source, generation)
                        })
                {
                    set.insert(source);
                }
            } else {
                // `sources_with_generations()` snapshots under the per-rset
                // mutex and returns owned pairs, so the lock is not held across
                // the body.
                for (source, generation) in regions[cset_idx].rset.sources_with_generations() {
                    if !Self::rset_entry_is_stale(regions, source, generation) {
                        set.insert(source);
                    }
                }
            }
        }
        set
    }

    /// May Phase 2 apply the card screen to this source region?
    ///
    /// One place, because the answer is a policy about what the write barrier
    /// covers and it has to be the same on the serial and parallel arms.
    ///
    /// A source the remembered set named is screened, always: the entry and the
    /// card have the same three producers, so a region with no dirty card holds
    /// no recorded cross-region edge. A JIT-PINNED source is the question this
    /// answers — it is added to the source list unconditionally, as
    /// defence-in-depth against compiled stores that might have skipped the
    /// barrier, and screening it would trust exactly what that walk exists to
    /// double-check.
    ///
    /// Under G1 there are no such stores any more. Every compiled
    /// reference-store path reaches `post_write_barrier_rset`; the enumeration,
    /// with the gate that forces each one, is on
    /// [`cratonvm_types::GcFlags::g1_card_screen_jit_pinned`], and
    /// `CRATONVM_G1_CARD_SCREEN_JIT_PINNED=0` restores the wholesale walk.
    ///
    /// This is the lever the §12 measurement pointed at: the screen was not
    /// weak, it was switched off for the regions a warm JIT makes most of.
    #[inline]
    pub fn card_screen_for_source(&self, wholesale: &RegionSet, src_idx: usize) -> bool {
        !wholesale.contains(&src_idx) || gc_flags().g1_card_screen_jit_pinned
    }

    /// F-05 — the four card-screen counters as of now, for a pause to diff
    /// against after Phase 2.
    ///
    /// The counters are process-lifetime monotone atomics (Phase 2's walkers
    /// hold `&self`, and the parallel evacuator's seeding runs off a
    /// `SharedEvac` that has no `&mut` to anything the pause owns), so a pause
    /// reads its own contribution as a difference rather than by resetting
    /// them. Resetting would race with a concurrent-mark step that happens to
    /// be scanning at the same moment.
    pub fn card_scan_snapshot(&self) -> [u64; 4] {
        [
            self.card_regions_offered.load(Ordering::Relaxed),
            self.card_regions_skipped.load(Ordering::Relaxed),
            self.card_bytes_scanned.load(Ordering::Relaxed),
            self.card_bytes_skipped.load(Ordering::Relaxed),
        ]
    }

    /// F-05 — publish `now - before` into the pause's phase table.
    pub fn fill_card_scan_phases(&self, phases: &mut G1PausePhases, before: [u64; 4]) {
        let now = self.card_scan_snapshot();
        phases.rset_regions_offered = now[0].saturating_sub(before[0]) as u32;
        phases.rset_regions_skipped = now[1].saturating_sub(before[1]) as u32;
        phases.rset_bytes_scanned = now[2].saturating_sub(before[2]);
        phases.rset_bytes_skipped = now[3].saturating_sub(before[3]);
    }

    /// F-05 — the arena card table, for the tests and for the F-08 barrier
    /// table publication.
    pub fn cards(&self) -> &std::sync::Arc<crate::g1_cards::G1CardTable> {
        &self.cards
    }

    /// The reclassification-epoch bump every recycle/retype phase makes first
    /// (under the regions write lock), and -- JIT round 21, lane vhcae
    /// (VHCAE21-1) -- the compiled edge filter's mirror of it
    /// ([`G1JitEdgeFilter`]), stored right after, before any reset.
    pub fn bump_rset_cache_epoch(&self) {
        let epoch = self
            .rset_cache_epoch
            .fetch_add(1, Ordering::Release)
            .wrapping_add(1);
        self.jit_edge_filter.publish_epoch(epoch);
    }

    /// JIT round 21, lane vhcae (VHCAE21-1): the address of this collector's
    /// [`G1JitEdgeFilter`], the answer to the JIT's
    /// `WELL_KNOWN_VH_CONST_G1_EDGE_FILTER_TAG` query. Stable for the
    /// collector's life.
    pub fn jit_edge_filter_addr(&self) -> usize {
        self.jit_edge_filter.addr()
    }

    /// gci i18/nr (gci i17/rv finding 11): this heap's freed-target scrub
    /// counters (they were process statics before).
    pub fn cleanup_scrub_counters(&self) -> &CleanupScrubCounters {
        &self.scrub_counters
    }

    /// Post-write barrier: track cross-region references in remembered sets.
    ///
    /// Round-9 gc CRIT-8 — hot path: a per-thread cache of the
    /// last-touched destination region pointer avoids re-acquiring the
    /// heap-wide `regions` mutex on every reference store. Same-region
    /// successive stores (the dominant pattern: tight loops populating
    /// one array, append-to-list, etc.) hit the cache and take only the
    /// per-RSet inner mutex.
    ///
    /// SAFETY invariant: the `Vec<G1Region>` backing `self.regions` is
    /// created once in `G1Collector::new` with all `region_count`
    /// entries and is **never resized** thereafter (`grep -n
    /// 'regions.\\(push\\|resize\\|extend\\)' gc/src/g1.rs` returns no
    /// matches). Therefore `&regions[i]` is address-stable for the
    /// entire `G1Collector` lifetime, and a raw `*const G1Region`
    /// captured under one lock acquisition remains valid for later
    /// dereference even after the lock is released, as long as the
    /// collector itself is still alive. The cached pointer is keyed by
    /// both region index AND collector identity so a stale cache from a
    /// previous collector instance never resurrects a dangling pointer.
    /// Identity is the minted [`Self::instance_id`], NOT the collector's
    /// address: `self as *const Self` ABAs when a later collector is
    /// constructed at a dropped one's address (stack-slot or allocator
    /// block reuse) with a matching epoch history, which let this fast
    /// path write through a pointer into the dropped collector's freed
    /// regions storage (intermittent 0xC0000374 under parallel gc tests).
    pub fn post_write_barrier_rset(&self, src_obj: ObjectRef, stored_ref: ObjectRef) {
        let src_addr = src_obj.as_ptr() as usize;
        let dst_addr = stored_ref.as_ptr() as usize;

        // O(log R) lookups via the cached `region_lookup` table; these do
        // NOT take any lock (the lookup table is immutable post-`new`).
        let src_region = self.lookup_region_for_addr(src_addr);
        let dst_region = self.lookup_region_for_addr(dst_addr);

        // Only record cross-region references.
        let (src_idx, dst_idx) = match (src_region, dst_region) {
            (Some(s), Some(d)) if s != d => (s, d),
            _ => {
                if gc_flags().g1_dbg_accessor {
                    self.rset_edge_same_region.fetch_add(1, Ordering::Relaxed);
                }
                return;
            }
        };

        // JIT round 23 wave 2d, lane L4 (`CRATONVM_G1_POST_BARRIER_LAST_EDGE`,
        // default ON, `=0` kill): a per-thread ONE-entry memo of the last
        // completed (holder, destination region) store. A repeat of exactly
        // that store -- `ring[i & 1023] = new ...`, the same holder into the
        // same young destination region for a whole TLAB -- has nothing left
        // to do: the holder's start card (the one card this function
        // dirties) is still dirty, and the (source, destination) rset edge is
        // still recorded, unless a cleaner or a recycle ran in between. Both
        // are keyed: `clean_epoch` moves before any card is made cleaner or
        // the block-offset table is reset (`G1CardTable`), and
        // `rset_cache_epoch` moves before any rset is cleared or region
        // retyped. Both only ever grow, a key is read BEFORE the work it
        // vouches for, and the memo is set only on the paths that finished
        // the work (never on the Free-destination decline), so a stale key
        // can only miss. Holder address equality is region equality for the
        // epoch's lifetime (objects move only in a pause, which bumps it).
        let last_edge_key: Option<G1LastEdgeKey> = true.then(|| {
            (
                self.instance_id,
                src_addr,
                dst_idx,
                self.rset_cache_epoch.load(Ordering::Acquire),
                self.cards.clean_epoch(),
            )
        });
        thread_local! {
            static G1_LAST_EDGE: std::cell::Cell<Option<G1LastEdgeKey>>
                = const { std::cell::Cell::new(None) };
        }
        if let Some(key) = last_edge_key {
            if g1_last_edge_memo_hit(G1_LAST_EDGE.with(|c| c.get()), key) {
                if gc_flags().g1_dbg_accessor {
                    self.rset_edge_memo_hit.fetch_add(1, Ordering::Relaxed);
                }
                // JIT round 21, lane vhcae (VHCAE21-1): the key's epoch is the
                // one the edge was recorded in, the compiled filter's claim.
                self.jit_edge_filter.note_edge(dst_idx, src_idx, key.3);
                return;
            }
        }
        let note_last_edge = || {
            if let Some(key) = last_edge_key {
                G1_LAST_EDGE.with(|c| c.set(Some(key)));
            }
        };

        // F-05 — dirty the SOURCE card, and do it here: before the per-thread
        // edge memo below, before the region-pointer cache, before the Free
        // gate on the destination.
        //
        // WHY THIS EXACT PLACE. The memo below skips the rset insert for an
        // edge this thread already recorded in this epoch. That is sound for
        // the rset — a set insert is idempotent — but a card is per SOURCE
        // ADDRESS, not per (source region, target region) pair: the same edge
        // key covers stores into two different objects 400 KiB apart in the
        // same source region, i.e. two different cards. Dirtying after the memo
        // check would leave the second one clean, and a clean card holding a
        // live cross-region reference is exactly the use-after-free this whole
        // structure exists to prevent. So the card store is unconditional on
        // every cross-region store; it is one `mov` into a byte the cache line
        // for is almost always already hot.
        //
        // WHY THE CROSS-REGION TEST IS THE RIGHT GATE. Card and rset entry are
        // written under identical conditions, which is what lets Phase 2 skip a
        // source region with no dirty card: a same-region store from a non-CSet
        // region reaches a referent in that same non-CSet region, so it can
        // never be an edge into the collection set, and neither structure needs
        // to know about it.
        //
        // NOT gated on `g1_card_rset`. That flag disables the READ side (the
        // screen in Phase 2); the write side stays on so the flag is a pure
        // read-side A/B rather than a half-disabled feature whose card table is
        // then wrong for the rest of the run.
        //
        // W5-B — the holder arrives as an `ObjectRef`, whose type contract is
        // "points at a published object header", and the witness is minted
        // from it rather than from `src_addr`. The barrier is the one producer
        // where this matters most: it is the path a new caller is likeliest to
        // be added to, and the path on which naming a SLOT instead of the
        // object would look harmless (the whole-range card screen is correct
        // either way — it is the block-offset jump that is not). See
        // `ObjectStart` in `gc/src/g1_cards.rs`.
        self.cards
            .dirty_addr_at_object_start(crate::g1_cards::ObjectStart::of_object(src_obj));

        let collector_id = self.instance_id;

        // SECURITY FIX (V7a): snapshot the current reclassification epoch.
        // Pairs (`Acquire`) with the `Release` bump performed under the
        // regions lock at the start of every recycle/retype phase
        // (`young_collection`/`mixed_collection`/`cleanup`).
        let cur_epoch = self.rset_cache_epoch.load(Ordering::Acquire);

        thread_local! {
            // SECURITY FIX (V7a): now (collector instance_id, region_idx,
            // *const G1Region, epoch). `Cell` is sufficient — the
            // pointer is `Copy` and never escapes the with() block other
            // than as a deref-then-call.
            static LAST_RSET_TARGET: std::cell::Cell<Option<(u64, usize, *const G1Region, u64)>>
                = const { std::cell::Cell::new(None) };
            /// Edges this thread has already recorded, as PACKED
            /// `(dst_idx << 32) | src_idx` keys — `0` means empty.
            ///
            /// Four slots, not one. One slot was the first shape and it thrashes
            /// on the workload that matters: a loop storing references into one
            /// array reaches objects spread over SEVERAL source regions, so a
            /// single-entry memo alternates between two or three live edges and
            /// misses every time. Packing the pair into one `u64` is what makes
            /// four slots affordable — the whole memo is 32 bytes to copy out of
            /// the `Cell`, against 160 for an array of
            /// `Option<(u64, usize, usize, u64)>`, which is the cost that made
            /// the first attempt at four slots a wash.
            static RSET_EDGE_MEMO: std::cell::Cell<[u64; RSET_EDGE_MEMO_SLOTS]>
                = const { std::cell::Cell::new([0; RSET_EDGE_MEMO_SLOTS]) };
            /// The `(collector_id, epoch)` the memo above was built under. Any
            /// change invalidates all of it at once, which is what keeps the
            /// per-slot compare down to a single `u64`.
            static RSET_EDGE_MEMO_TAG: std::cell::Cell<(u64, u64)>
                = const { std::cell::Cell::new((u64::MAX, u64::MAX)) };
        }

        // An rset is a SET of source region indices, so re-recording an edge
        // this thread already recorded in this epoch is a no-op — and taking a
        // per-destination `Mutex<FxHashMap>` to discover that is what every
        // reference store into a hot region was paying. Mutators writing into
        // the same current Eden region all contend on that one mutex, so the
        // cost is a convoy rather than an uncontended lock: the same total
        // number of retained-object stores measured 1.0 s on one thread and
        // 6.8 s on eight (ZGC flat at ~1.1 s), with ZERO collections in either
        // run — the pauses were never involved.
        //
        // `epoch` is what makes skipping sound, and it is the same epoch the
        // region-pointer cache below already relies on: a recycle/retype phase
        // bumps it under the regions lock, and `G1Region::reset` (which clears
        // the rset) only happens inside such a phase. A stale memo therefore
        // cannot survive the clear that would invalidate it. Coarsening does
        // not need to be tracked either — an add to a coarsened rset returns
        // early anyway, so skipping it changes nothing.
        // Packed key. `dst_idx`/`src_idx` are region indices, bounded by
        // `heap_size / region_size`, so 32 bits each is enormous headroom; the
        // `+ 1` keeps `0` free as the empty marker.
        let edge_key = (((dst_idx as u64) + 1) << 32) | ((src_idx as u64) + 1);
        let tag_ok = RSET_EDGE_MEMO_TAG.with(|c| {
            if c.get() == (collector_id, cur_epoch) {
                true
            } else {
                // A new collector instance or a new reclassification epoch
                // retires the whole memo in one step.
                c.set((collector_id, cur_epoch));
                RSET_EDGE_MEMO.with(|m| m.set([0; RSET_EDGE_MEMO_SLOTS]));
                false
            }
        });
        if tag_ok && RSET_EDGE_MEMO.with(|m| m.get()).contains(&edge_key) {
            if gc_flags().g1_dbg_accessor {
                self.rset_edge_memo_hit.fetch_add(1, Ordering::Relaxed);
            }
            // JIT round 21, lane vhcae (VHCAE21-1): this memo's tag says the
            // edge is in the rset in `cur_epoch`, which is the compiled
            // filter's claim too; refresh it if another edge took the entry.
            self.jit_edge_filter.note_edge(dst_idx, src_idx, cur_epoch);
            note_last_edge();
            return;
        }
        // Record the edge as memoized only after an add below actually lands.
        // Insert at the front so the most recent edge is found first; the
        // oldest falls off the end.
        let remember_edge = |epoch: u64| {
            if gc_flags().g1_dbg_accessor {
                self.rset_edge_recorded.fetch_add(1, Ordering::Relaxed);
            }
            // LANE A (soundness) — RE-TAGGING THE MEMO MUST RETIRE IT.
            //
            // This used to be a bare `c.set((collector_id, epoch))`, and
            // `epoch` is not always `cur_epoch`: the slow path below re-reads
            // the epoch UNDER THE REGIONS LOCK and passes that. The two differ
            // exactly when a recycle/retype phase bumped the epoch while this
            // thread was blocked acquiring that lock — which is the window the
            // epoch exists to detect. The old code then stamped the NEW epoch
            // onto a memo still holding edges recorded under the OLD one, so
            // every one of those stale keys became live again. A later store of
            // such an edge would hit the memo and skip the rset insert, while
            // the `G1Region::reset` that accompanied the bump had already
            // cleared the entry it was relying on — a missing remembered-set
            // entry, i.e. a referent that a later pause never evacuates and
            // whose region it then frees.
            //
            // The `tag_ok` check above already retires the whole memo on a tag
            // mismatch; this is the same rule applied at the other end, where
            // the epoch being recorded is the one that may have moved.
            RSET_EDGE_MEMO_TAG.with(|c| {
                if c.get() != (collector_id, epoch) {
                    RSET_EDGE_MEMO.with(|m| m.set([0; RSET_EDGE_MEMO_SLOTS]));
                    c.set((collector_id, epoch));
                }
            });
            RSET_EDGE_MEMO.with(|m| {
                let mut memo = m.get();
                memo.rotate_right(1);
                memo[0] = edge_key;
                m.set(memo);
            });
            // JIT round 21, lane vhcae (VHCAE21-1): the insert landed in
            // `epoch` -- the compiled edge filter may skip this edge's call
            // for the rest of that epoch (`G1JitEdgeFilter`).
            self.jit_edge_filter.note_edge(dst_idx, src_idx, epoch);
        };

        let hit = LAST_RSET_TARGET.with(|cell| {
            if let Some((cached_collector, cached_idx, cached_ptr, cached_epoch)) = cell.get() {
                // SECURITY FIX (V7a): only honour the fast path when the
                // cache was populated in the *current* reclassification
                // epoch. A stale `cached_epoch` means a collection has
                // recycled/retyped regions since the entry was captured,
                // so the cached `*const G1Region` may now name a Free (or
                // re-typed) region whose rset we must NOT touch. On
                // mismatch we fall through to the Free-gated slow path,
                // which re-validates under the regions lock and refreshes
                // the cache with the new epoch.
                if cached_collector == collector_id
                    && cached_idx == dst_idx
                    && cached_epoch == cur_epoch
                {
                    // SAFETY: see method-level invariant. `cached_ptr`
                    // was captured from `&regions[dst_idx]` (Vec never
                    // reallocates), the collector identity check rules
                    // out reuse across collector instances, and the
                    // epoch check rules out a recycled region.
                    // `add_reference_in_generation` takes `&self` (interior
                    // `parking_lot::Mutex` on the FxHashMap — see
                    // `RememberedSet`).
                    //
                    // G1AUD-5: stamp the entry with `cur_epoch`, the
                    // reclassification generation this store happened in. Free
                    // — the value was already loaded to validate the cache.
                    unsafe {
                        (*cached_ptr)
                            .rset
                            .add_reference_in_generation(src_idx, cur_epoch);
                    }
                    return true;
                }
            }
            false
        });
        if hit {
            // W3-B — RE-VALIDATE THE EPOCH AFTER THE INSERT, NOT ONLY BEFORE.
            //
            // Filed as `lane-a-rset-tls-fast-path-epoch-window`. The block
            // above is the only path through this function that takes NO
            // regions lock: it checks `cached_epoch == cur_epoch` and then
            // inserts through a raw `*const G1Region`. Between those two steps
            // a recycle/retype phase can run, and it does its two steps in this
            // order — bump `rset_cache_epoch`, then `G1Region::reset` each
            // condemned region, which calls `self.rset.clear()`. An insert that
            // lands between them is cleared by step 2, and the edge is gone
            // from the remembered set while the reference is still in the heap:
            // the next pause that collects the destination never scans the
            // source, and Phase 5 frees a referent that is still named. That is
            // a use-after-free, and it is the one direction this file's rules
            // say never to take.
            //
            // WHY A SECOND LOAD IS ENOUGH. Every one of the six bump sites
            // (`young_collection_serial`, `young_collection_parallel`,
            // `mixed_collection`, `mixed_collection_parallel`,
            // `drain_kept_self_forwards`, `cleanup`) is the FIRST statement
            // after `let mut regions = self.regions.write();`, and every
            // `G1Region::reset` runs later under that same acquisition — this
            // was the open question in §4 option 1 of the filed page, and it is
            // now checked. So a reset is always preceded, within one write-lock
            // acquisition, by a bump. If the epoch has not moved across our
            // insert, no acquisition that could have reset anything overlapped
            // it, and the entry we just made is still there.
            //
            // WHY NOT the per-region `recycled_in_generation` instead (§4
            // option 4, the filed recommendation). That field is the right
            // thing to check — it is written by the reset itself rather than at
            // a different moment — and as of W3-A it IS an `AtomicU64`
            // (`G1Region::recycled_in_generation` / `set_recycled_in_generation`,
            // `Acquire`/`Release`, with the store deliberately after
            // `rset.clear()`), so the blocker this paragraph used to name — "it
            // is a plain `u64`, reading it from a lock-free path is a data
            // race" — is gone. W3-A stopped there rather than rewriting this
            // protocol, for a reason that belongs to whoever takes option 4:
            //
            //   Substituting the per-region stamp for the global epoch in the
            //   POST-insert check is not a pure precision win. Today, a benign
            //   bump elsewhere in the heap sends this store down the slow path,
            //   which re-stamps the entry with `epoch_under_lock` — the NEW
            //   generation. Option 4 would keep the fast path and leave the
            //   entry stamped with `cur_epoch`, the OLD one. `rset_entry_is_stale`
            //   is `recorded_generation < source.recycled_in_generation`, so an
            //   entry carrying an older stamp is closer to being pruned, and a
            //   pruned live entry is a use-after-free — the one direction this
            //   file's rules say never to take. The window is unreachable under
            //   the safepoint argument either way, and `fast_path_epoch_raced`
            //   has measured 0 over 1,649,917 cross-region stores, so there is
            //   no observed cost to buy back. That trade is W3-B's to make.
            //
            // See `w3a-region-metadata-ub.md` §9. This remains the interim the
            // filed page recommends, and unlike that page's description of it
            // ("not a proof") the ordering check above makes it one, modulo the
            // second load observing a store that precedes it.
            //
            // The redo is the SLOW path below, unmodified: it takes
            // `self.regions.read()`, which cannot be held while a pause holds
            // the write lock, re-applies the `Free` gate, and stamps the entry
            // with the epoch it validated under. Over-retaining a duplicate
            // entry is the cost when the race was benign; that is the correct
            // direction.
            if self.rset_generation() == cur_epoch {
                remember_edge(cur_epoch);
                note_last_edge();
                return;
            }
            // Counted, never asserted: this window is only reachable while a
            // pause is running, and a `debug_assert!` that fires inside a GC
            // pause manufactures a worse bug than it reports. Fall through.
            let before = self
                .rset_fast_path_epoch_raced
                .fetch_add(1, Ordering::Relaxed);
            if before == 0 {
                // Once per process, and NOT behind a debug flag. The standing
                // argument for why this window is unreachable — a mutator only
                // reaches the barrier while running Java, and a pause needs
                // every mutator at a safepoint — is the same argument that
                // would make `rset_cache_epoch` unnecessary, and the epoch was
                // added as a security fix over an observed intermittent
                // heap-corruption abort. So the one observation that decides
                // between those two readings is whether this ever fires on a
                // real workload, and that observation must not require knowing
                // to switch a flag on first. Subsequent occurrences are silent;
                // the counter carries the total.
                tracing::warn!(
                    "[g1] remembered-set write barrier: the lock-free fast path saw the \
                     reclassification epoch move across its own rset insert, and redid the \
                     insert under the regions lock. This is the window filed as \
                     lane-a-rset-tls-fast-path-epoch-window; the fail-safe held, but the \
                     window is reachable on this workload. Reported once per process."
                );
            }
        }

        // Cache miss: take the regions lock just long enough to capture
        // the stable pointer, populate the TLS cache, then perform the
        // RSet add via the same `&self` interior-mutex path.
        //
        // Round-9 fix (HIGH C4): post-filter by `region_type` here, on the
        // slow path only. The cached `region_lookup` table is keyed by
        // address-range alone and so `lookup_region_for_addr` happily
        // returns the index of a Free (or HumongousContinuation) region
        // whose backing buffer still covers `dst_addr` from a prior cycle.
        // Recording references into Free regions inflates RSet traffic and,
        // more critically, holds references that will be reset to garbage
        // at the next GC. We replicate the gating that
        // `is_addr_in_live_region` (line 2324) and `is_object_address`
        // (line 2267) already apply on the read-side root-scan paths.
        //
        // SECURITY FIX (V7a): the fast-path TLS cache is now also gated
        // by `rset_cache_epoch` (see above). When the collector moves a
        // region between Eden/Survivor/Old/Free it bumps the epoch under
        // this same lock, so the previously-documented stale-cache window
        // — where a cache entry could briefly admit a write into a
        // just-freed region — is closed: any cached entry from before the
        // bump fails the epoch comparison and is forced down this
        // Free-gated slow path. We re-read the epoch under the lock so the
        // value stamped into the cache is consistent with the
        // `region_type` we validate.
        let regions = self.regions.read();
        if regions[dst_idx].region_type() == RegionType::Free {
            return;
        }
        let epoch_under_lock = self.rset_cache_epoch.load(Ordering::Acquire);
        let region_ptr: *const G1Region = &regions[dst_idx];
        LAST_RSET_TARGET.with(|cell| {
            cell.set(Some((collector_id, dst_idx, region_ptr, epoch_under_lock)));
        });
        // G1AUD-5: stamp with the generation validated under the lock, so the
        // stamp and the `region_type` check describe the same instant.
        regions[dst_idx]
            .rset
            .add_reference_in_generation(src_idx, epoch_under_lock);
        remember_edge(epoch_under_lock);
        note_last_edge();
    }

    /// The [`cratonvm_gc_core::gc_quiescence::incomplete_reason`] code when this
    /// collection's JIT root set is known to be INCOMPLETE, or `None` when
    /// every live compiled frame was enumerated.
    ///
    /// # Why G1 needs this and not only [`Self::jit_pinned_region_set_screened`]
    ///
    /// `jit_pinned_region_set` is G1's stand-in for the generational
    /// collector's non-moving-while-in-JIT sweep: the root gatherer publishes
    /// each conservatively-discovered JIT-frame root, and the region holding
    /// it is kept out of the collection set so the un-rewritable
    /// register/spill slot that names it cannot go stale.
    ///
    /// That is only sound when the conservative scan actually *saw* the frame.
    /// Several coverage obligations fail precisely because it did not:
    /// `UNREGISTERED_JIT_FRAME` (a compiled frame sits above the entry chain's
    /// cover, so the scan band never reaches it), `FOREIGN_INNERMOST_RBP` (the
    /// innermost recorded RBP belongs to a deeper callee entered by a direct
    /// JIT->JIT call, so the entry's map does not describe the frame that is
    /// actually there), `MISSING_EXACT_RBP` (the frame cannot be bounded at
    /// all). In each of those the pin set comes back *empty for that frame*,
    /// G1 sees no reason to exclude anything, and it evacuates an object whose
    /// only reference lives in a slot nothing will rewrite. The next
    /// dereference would be a native `SIGSEGV`, not a controlled Java error —
    /// the shape the two retired 2026-08 full-suite G1 comparisons (the
    /// `g1-collector-fullsuite-crashes-hangs-fails-20260806` and
    /// `g1-fullsuite-regression-20260807` write-ups) recorded, from three and
    /// one crash sites respectively, with the collector's own "last
    /// incomplete-coverage reason" field already naming an obligation in every
    /// report.
    ///
    /// # Why this is a DIAGNOSTIC, not a fail-safe
    ///
    /// Each of the reasons above is, on inspection, already backed by a
    /// conservative scan whose roots G1 pins -- or, for a word published
    /// MOVABLE, whose register the shadow reload refreshes (gci i9/ng, i8/nt
    /// review item 3: `jit_pinned_region_set` does not pin a movable word, so
    /// this argument needs the shadow reload; it holds with shadow emission on,
    /// and with it off only since i8/nt made a blind spill-image word pin
    /// (`conservative_roots::register_image_word_publishes_movable`). A
    /// register the shadow push missed is the residual; see that function):
    ///
    /// * an unregistered frame is detected and its whole band is scanned by
    ///   `conservative_roots::scan_active_jit_frames`, which pushes the band's
    ///   oops into the same `roots` vector `memory::roots::collect_roots`
    ///   then republishes through `gc_quiescence::add_pinned_jit_root`;
    /// * a precise entry always ALSO gets a conservative band scan
    ///   (`scan_compiled_frame_bands`, or the whole-band `scan_one_frame`
    ///   fallback when its metadata is not trustworthy), so an unresolvable
    ///   innermost RBP or a missing exact RBP still leaves the frame covered;
    /// * a parked or blocked peer publishes its own conservative JIT roots via
    ///   `interpreter::update_root_snapshot`'s `publish_pinned_jit_roots`;
    /// * a forcibly-frozen peer and its helper window are pinned by
    ///   `interpreter::pin_frozen_peer_roots_for_g1`, and its un-retired TLAB
    ///   tail by [`Self::set_jit_tlab_skip_regions`].
    ///
    /// So "coverage incomplete" under G1 means *the roots are not REWRITABLE*,
    /// which is the normal state whenever a thread is in compiled code — not
    /// *the roots were not ENUMERATED*. Measured: 330263 of 330264 pauses on
    /// `probes/MovingYoungConcurrentProbe 6 400 2000`. Refusing to evacuate on
    /// it is therefore both unnecessary and ruinous — the same run needs ONE
    /// collection with the lever off and takes 330264 no-op pauses with it on,
    /// because a pause that frees nothing is immediately re-triggered by the
    /// next allocation.
    ///
    /// The refusal is kept as an opt-IN bisection lever
    /// (`CRATONVM_G1_COVERAGE_PIN`, [`cratonvm_gc_core::gc_flags`]`().g1_coverage_pin`):
    /// under it G1 moves nothing, so a G1-only crash that survives it is not
    /// caused by a relocation the root set failed to cover. That is the
    /// experiment both 2026-08 full-suite G1 pages asked for and could not run.
    ///
    /// This function is the DETECTION only and is deliberately NOT gated on
    /// that flag, so the counters report the rate in both arms. A lever that
    /// also switches off its own measurement cannot settle anything.
    pub fn root_coverage_incomplete_reason() -> Option<usize> {
        // `force_non_moving_jit_roots` is the root gatherer's own OSR-shadow
        // verdict and does not always travel with a reason code; report the
        // stored reason when there is one, and the OSR code when there is not,
        // so the record never claims `NONE` while refusing to evacuate.
        let flagged = cratonvm_gc_core::gc_quiescence::moving_young_coverage_incomplete();
        let forced = cratonvm_gc_core::gc_quiescence::force_non_moving_jit_roots();
        if !flagged && !forced {
            return None;
        }
        let reason = cratonvm_gc_core::gc_quiescence::moving_young_incomplete_reason();
        Some(if reason == cratonvm_gc_core::gc_quiescence::incomplete_reason::NONE {
            cratonvm_gc_core::gc_quiescence::incomplete_reason::OSR_SHADOW
        } else {
            reason
        })
    }

    /// Box a non-Object `Value` for storage in a REFERENCE array element, or
    /// return it unchanged when it is already a reference (or the array is not
    /// a reference array).
    ///
    /// The wrapper is a one-field `AUTOBOX_CLASS_ID` object, exactly as
    /// `GenerationalHeap::set_array_element` and `Heap::set_array_element`
    /// build it, so [`Self::autobox_payload`] and every other backend's reader
    /// recognise it. Must be called BEFORE taking the `regions` lock — the
    /// allocation needs it.
    ///
    /// `None` means the wrapper could not be allocated: the heap is full. The
    /// caller turns that into `ARRAY_STORE_OUT_OF_MEMORY` rather than letting
    /// `alloc_object` `std::process::abort()` the VM — see that constant.
    pub fn autobox_for_reference_array(&self, array: ObjectRef, value: Value) -> Option<Value> {
        if matches!(value, Value::Object(_)) {
            return Some(value);
        }
        if self.get_header(array).element_type() != ArrayElementType::Reference {
            return Some(value);
        }
        let wrapper = self.try_alloc_object(cratonvm_gc_core::heap::AUTOBOX_CLASS_ID, 1)?;
        self.set_field(wrapper, 0, value);
        // Arm the process-wide wrapper latch — see the matching note in
        // `GenerationalHeap::set_array_element` and `cratonvm_gc_core::autobox`.
        cratonvm_gc_core::autobox::note_wrapper_created();
        Some(Value::Object(Some(wrapper)))
    }

    /// The primitive inside an auto-box wrapper, or `None` when `candidate` is
    /// an ordinary object.
    ///
    /// Validates the address before dereferencing its header: a reference array
    /// element is a raw word and a stale or garbage one could point anywhere,
    /// so an unchecked read here would be wild. Mirrors the same guard in
    /// `GenerationalHeap::get_array_element`.
    pub fn autobox_payload(&self, candidate: ObjectRef) -> Option<Value> {
        self.is_object_address(candidate.as_ptr() as usize)?;
        // SAFETY: `is_object_address` confirmed `candidate` points at a valid
        // object header inside one of this collector's regions.
        let header = unsafe { &*(candidate.as_ptr() as *const ObjectHeader) };
        if header.class_id != cratonvm_gc_core::heap::AUTOBOX_CLASS_ID {
            return None;
        }
        Some(self.get_field(candidate, 0))
    }

    /// Whether this pause must decline to evacuate: the detection from
    /// [`Self::root_coverage_incomplete_reason`] AND the opt-in
    /// `CRATONVM_G1_COVERAGE_PIN` lever.
    ///
    /// A pure function of its two inputs so both arms are testable —
    /// `gc_flags()` latches for the process, so a test cannot flip the lever
    /// from inside one.
    pub fn refuse_evacuation(coverage_incomplete: Option<usize>, lever_on: bool) -> Option<usize> {
        coverage_incomplete.filter(|_| lever_on)
    }

    /// Whether this pause is running with a live compiled frame AND an empty
    /// conservative JIT root publication.
    ///
    /// # Why this is a fail-safe where `root_coverage_incomplete_reason` is a
    /// diagnostic
    ///
    /// [`Self::root_coverage_incomplete_reason`]'s header argues at length that
    /// every incomplete-coverage reason is nonetheless *backed by a
    /// conservative scan whose roots G1 pins*, and it is right — which is why
    /// its refusal is an opt-in lever that fires on ~99.9% of pauses and
    /// selects nothing. But that argument has a premise: that the scan
    /// published something. This predicate is the case where the premise is
    /// false.
    ///
    /// With `jit_active=true`, `pin_addrs=0` is being consumed as "there are no
    /// JIT roots". What it actually means is "the scan found none", which is
    /// **unknown**, not **none**. An empty publication and a genuinely
    /// reference-free compiled frame are indistinguishable at the point where
    /// the collection set is built, and only one of them is safe to evacuate.
    /// The generational collector never faces the question — it moves nothing
    /// at all while any thread is in JIT. G1 moves everything it was not told
    /// to pin, and is therefore exposed exactly when the telling comes up
    /// empty.
    ///
    /// Measured instance: `PolynomialTest` under `-XX:+UseG1GC --Xmx 1g` takes
    /// one young pause, at which `jit_active=true pin_addrs=0 pin_regions={}`,
    /// and the region holding the object a compiled `StringLatin1.newString`
    /// was using goes into the collection set. The failure surfaces four frames
    /// later as `IllegalFormatConversionException: d != java.lang.Object`.
    ///
    /// # This predicate only means what it says because of the fix in `roots.rs`
    ///
    /// It was first written believing `pin_addrs=0` under a live compiled frame
    /// meant the conservative scan had run and found nothing. It did not. The
    /// scan was being SKIPPED: `collect_roots` took the precise-only branch
    /// whenever the oop-map coverage proof passed, and G1's pin set is built
    /// from that scan's output alone, so an empty publication was simply what
    /// precise mode looked like from here. Measured with
    /// `CRATONVM_DBG_JIT_ROOTSCAN`: `precise_only=true scan_added=0` on the
    /// failing test, `precise_only=false scan_added=14` on a JIT-heavy probe
    /// that never trips this predicate at all.
    ///
    /// That is why refusing on it was ruinous before the fix — it refused every
    /// precise-mode pause: 1444 consecutive no-op pauses on `PolynomialTest`
    /// and `OutOfMemoryError` on 8 tests, the same wall
    /// `CRATONVM_G1_COVERAGE_PIN` hits at its ~99.99% rate. The predicate was
    /// detecting a MODE, not a defect.
    ///
    /// `collect_roots` no longer lets G1 take that branch, so the conservative
    /// scan always runs under this collector and an empty publication is once
    /// again the thing this function claims: the scan ran and found nothing
    /// while a compiled frame was live. That is a genuine anomaly and worth
    /// counting — which is what `record_g1_pause_empty_jit_publication` does,
    /// on every run, ungated.
    ///
    /// `CRATONVM_G1_PIN_EMPTY_PUBLICATION` remains OFF and remains a bisection
    /// lever rather than protection: a refusal reclaims nothing, so it can only
    /// buy time for a publication that later becomes non-empty. It is kept for
    /// the case where this counter goes non-zero again and the question is once
    /// more "is a relocation under an unproven root set what is failing here?".
    ///
    /// # What this does not catch
    ///
    /// Both halves are process-wide. `is_active()` is a striped global depth
    /// with no thread identity, and the pin map is summed across every thread's
    /// entry, so what this actually asks is "some thread is in JIT and NO
    /// thread published anything". The stricter question — did EVERY thread
    /// currently in JIT publish something? — needs per-thread accounting the
    /// quiescence API does not expose: a peer in JIT that published nothing is
    /// invisible here as soon as any OTHER thread published one address.
    ///
    /// That residual is narrower than the hole it closes and is not what the
    /// `PolynomialTest` failure was (`pin_addrs=0` was the process-wide total),
    /// but it is real, and it is a reason to prefer candidate fix 2 — repairing
    /// the scan — over widening this predicate. See
    /// `bug-g1-evacuates-live-jit-reference-20260819-FIXED.md`.
    pub fn empty_jit_publication(&self) -> bool {
        // "A compiled frame is live." `is_active()` counts only JIT entries
        // that pushed a `JitEntryGuard`; a frame reached without one (the
        // process entry point into app `main`, and the other A5 cases) holds
        // the only reference to an object exactly as well. Ask both detectors,
        // because either one saying yes makes an empty publication unsafe.
        let frames_live = cratonvm_gc_core::gc_quiescence::compiled_frames_live()
            || cratonvm_gc_core::gc_quiescence::conservative_peers_live();
        if !frames_live {
            return false;
        }
        // "The publication is empty." Both pin vocabularies, since either one
        // being non-empty means the scan produced something for this pause to
        // pin and the premise above holds again. This mirrors
        // `jit_pinned_region_set`, which is the consumer whose empty result is
        // the actual hazard — keep the two in step.
        cratonvm_gc_core::gc_quiescence::pinned_jit_root_count() == 0
            && self.jit_tlab_skip_regions.lock().is_empty()
    }

    /// Whether this pause must decline to evacuate because of an empty JIT root
    /// publication: the detection from [`Self::empty_jit_publication`] AND the
    /// opt-in `CRATONVM_G1_PIN_EMPTY_PUBLICATION` lever.
    ///
    /// A pure function of its two inputs so both arms are testable —
    /// `gc_flags()` latches for the process, so a test cannot flip the lever
    /// from inside one. Same shape and same polarity as
    /// [`Self::refuse_evacuation`]: both refusals are opt-in, because neither
    /// reclaims anything and a pause that frees nothing is re-triggered by the
    /// next allocation. See [`Self::empty_jit_publication`] for what this one
    /// turned out to be detecting before `collect_roots` was fixed.
    pub fn refuse_evacuation_for_empty_publication(detected: bool, lever_on: bool) -> bool {
        detected && lever_on
    }

    /// `CRATONVM_G1_DBG_PINS=1` — what each pinned region COSTS this pause, and
    /// which pin vocabulary published it.
    ///
    /// `pin_regions={0, 11, 16}` (the line this extends) names the regions but
    /// not the bill. A pin excludes a whole region from the collection set, so
    /// the retention it buys is that region's OCCUPANCY, not the size of the
    /// object the conservative word pointed at — and an investigation that
    /// cannot see the occupancy cannot tell a pin that costs 40 bytes from one
    /// that costs a megabyte. On
    /// `docs/feature-designs/jit-ir-optimizations.md`
    /// that distinction IS the defect: three pinned regions, and the two that
    /// hold nothing the roots name are the whole of the 3224-vs-2228 gap.
    ///
    /// Provenance, because the three vocabularies have different repairs:
    ///
    /// * `jit` — an address in `gc_quiescence::pinned_jit_roots_snapshot()`, i.e.
    ///   a conservative compiled-frame word. Narrowing this needs precise oop
    ///   maps.
    /// * `tlab` — a frozen peer's un-retired TLAB tail
    ///   ([`Self::jit_tlab_skip_regions`]). Narrowing this needs the tail
    ///   parseable, not the region excluded.
    /// * `nonobj` — a root that failed [`Self::addr_is_followable_object`], pinned
    ///   by [`Self::pinned_region_set_including_non_object_roots`].
    ///
    /// Diagnostic-only, and it recomputes the provenance rather than threading it
    /// through the pin set: this runs once per pause behind a flag that is off,
    /// and a `RegionSet` is a bitset with nowhere to put a label.
    pub fn describe_pin_set(&self, regions: &[G1Region], pinned: &RegionSet) -> String {
        let region_size = self.config.region_size.max(1);
        // Provenance, by region, recomputed from the three publishers.
        let mut jit: RegionSet = RegionSet::new();
        if jit_frames_live_for_pins() {
            for addr in cratonvm_gc_core::gc_quiescence::pinned_jit_roots_snapshot() {
                if let Some(i) = self.lookup_region_for_addr(addr) {
                    jit.insert(i);
                }
            }
        }
        let mut tlab: RegionSet = RegionSet::new();
        for &(start, end) in self.jit_tlab_skip_regions.lock().iter() {
            if let Some(i) = self.lookup_region_for_addr(start) {
                tlab.insert(i);
            }
            if end > start {
                if let Some(i) = self.lookup_region_for_addr(end - 1) {
                    tlab.insert(i);
                }
            }
        }
        let mut nonobj: RegionSet = RegionSet::new();
        // Per region: the PIN addresses that landed in it, not every root that
        // did. Counting roots was the first version of this line and it is
        // useless — region 0 reported `roots=5369` because the whole root set
        // lives in Survivor, which says nothing about what pinned it. Five
        // addresses decide this CSet; those five are the census.
        let mut pin_hits: std::collections::HashMap<usize, Vec<usize>> =
            std::collections::HashMap::new();
        let mut pin_addrs: Vec<usize> = if jit_frames_live_for_pins() {
            cratonvm_gc_core::gc_quiescence::pinned_jit_roots_snapshot()
        } else {
            Vec::new()
        };
        pin_addrs.sort_unstable();
        pin_addrs.dedup();
        for addr in pin_addrs {
            let Some(idx) = self.lookup_region_for_addr(addr) else {
                continue;
            };
            pin_hits.entry(idx).or_default().push(addr);
            if !self.addr_is_followable_object(regions, addr, "pin-census") {
                nonobj.insert(idx);
            }
        }

        let mut out = String::new();
        let mut pinned_bytes = 0usize;
        for idx in pinned.iter() {
            let Some(r) = regions.get(idx) else {
                out.push_str(&format!(" [{idx}:no-such-region]"));
                continue;
            };
            let occ = r.cursor();
            pinned_bytes += occ;
            let mut prov = String::new();
            for (set, label) in [(&jit, "jit"), (&tlab, "tlab"), (&nonobj, "nonobj")] {
                if set.contains(&idx) {
                    if !prov.is_empty() {
                        prov.push('+');
                    }
                    prov.push_str(label);
                }
            }
            if prov.is_empty() {
                // A pinned region no vocabulary claims is not a tidy-up item:
                // the pin set and this census would then disagree about what
                // pinned it, and the pin set is the one that decides the CSet.
                prov.push_str("UNATTRIBUTED");
            }
            let empty: Vec<usize> = Vec::new();
            let hits = pin_hits.get(&idx).unwrap_or(&empty);
            // Each pin address with the size of the object it names, because
            // that is the number the region's occupancy has to be read against:
            // a 16-byte object holding a 997 KB region out of the CSet and a
            // 976 KB object holding its own are the same line otherwise.
            let mut named = String::new();
            for addr in hits.iter().take(4) {
                let (verdict, _) = self.classify_candidate_header(regions, *addr);
                let base = r.data.as_ptr() as usize;
                let size = if verdict == HeaderVerdict::Object {
                    // SAFETY: `Object` is the verdict that both tag bytes decoded
                    // and the shape is sane, which is what the sizer needs.
                    cratonvm_gc_core::concurrent_mark::concurrent_mark_object_size(
                        *addr as *const ObjectHeader,
                    )
                    .unwrap_or(0)
                } else {
                    0
                };
                named.push_str(&format!(
                    " 0x{addr:x}(+0x{:x},{verdict:?},{size}B)",
                    addr.wrapping_sub(base)
                ));
                // Not an object start: the object whose extent holds the word
                // (an interior or one-past-the-end pointer), or none.
                if verdict != HeaderVerdict::Object {
                    let mut within = String::from(" in=none");
                    let skips = self.jit_tlab_skip_spans();
                    let parsed = compact_in_place::walk_region(regions, idx, &skips, |p, h, sz| {
                        let p = p as usize;
                        if p < *addr && *addr <= p + sz {
                            within = format!(
                                " in=[{}+0x{:x}/{sz}B]",
                                cratonvm_gc_core::collector::class_name_for_diagnostics(h.class_id.as_u32()),
                                *addr - p
                            );
                        }
                    });
                    if !parsed {
                        within.push_str(" (walk-stopped)");
                    }
                    named.push_str(&within);
                }
                if let Some(provenance) = cratonvm_gc_core::gc_quiescence::lookup_jit_root_provenance(*addr) {
                    named.push_str(&format!(" prov=[{provenance}]"));
                }
            }
            out.push_str(&format!(
                " [{idx}:{:?} occ={}K/{}K pins={} {}{}]",
                r.region_type(),
                occ >> 10,
                region_size >> 10,
                hits.len(),
                prov,
                named,
            ));
        }
        format!("pinned_bytes={}K{}", pinned_bytes >> 10, out)
    }

    /// gci i9/fa (`gci-i1orch-windows-jni-foreign-attach-hangs-RETIRED-UNFIXED-20261018`, G1
    /// half) -- add this young pause's pin set to the `[GC] g1 pin-attribution:`
    /// sums, by publisher. `CRATONVM_G1_DBG_PINS=1` only (the caller's gate),
    /// beside the per-pause `[g1][PINS]` line it summarises.
    ///
    /// The vi5g-g1r F rows put a median of 11 pinned regions (mean 7.8 MB) on
    /// every young pause of `Gcd1JniRootsProbe` on G1, mostly Eden, from words
    /// published by somebody: the per-pause line names the regions and the
    /// first four words of each, not WHICH thread held them. The split here is
    /// [`cratonvm_gc_core::gc_quiescence::pinned_jit_roots_snapshot_split`]'s: the
    /// collecting thread's own published words against everyone else's (other
    /// threads' deposits and this cycle's helper-window scans), filtered by the
    /// same movable rule [`Self::jit_pinned_region_set_screened`] applies, plus the
    /// frozen peers' TLAB tails; what none of them claims was pinned by a root
    /// that is not an object start. A region only one publisher claims is a
    /// region that publisher alone kept out of the collection set.
    pub fn note_pin_attribution(&self, regions: &[G1Region], pinned: &RegionSet) {
        let honour_movable = g1_movable_pins_enabled();
        let kept = |addr: usize| {
            let claimed = cratonvm_gc_core::gc_quiescence::is_movable_jit_root(addr);
            let vetoed = cratonvm_gc_core::gc_quiescence::is_unrewritable_jit_root(addr);
            !(honour_movable && claimed && !vetoed)
        };
        let (own, others) = if jit_frames_live_for_pins() {
            cratonvm_gc_core::gc_quiescence::pinned_jit_roots_snapshot_split()
        } else {
            (Vec::new(), Vec::new())
        };
        // Only the caller's own words answer to its movable claims (JIT round
        // 21 lane parkcredit); every other publisher's word pins.
        let to_regions = |addrs: &[usize], filtered: bool| -> RegionSet {
            addrs
                .iter()
                .copied()
                .filter(|&a| !filtered || kept(a))
                .filter_map(|a| self.lookup_region_for_addr(a))
                .collect()
        };
        let own_set = to_regions(&own[..], true);
        let others_set = to_regions(&others[..], false);
        let mut tlab_set = RegionSet::new();
        for &(start, end) in self.jit_tlab_skip_regions.lock().iter() {
            if let Some(i) = self.lookup_region_for_addr(start) {
                tlab_set.insert(i);
            }
            if end > start {
                if let Some(i) = self.lookup_region_for_addr(end - 1) {
                    tlab_set.insert(i);
                }
            }
        }
        let young_of = |idx: usize| {
            regions.get(idx).map(|r| {
                (
                    matches!(r.region_type(), RegionType::Eden | RegionType::Survivor),
                    r.cursor() as u64,
                )
            })
        };
        let mut row = attribute_pin_regions(pinned, young_of, &own_set, &others_set, &tlab_set);
        row.xt_words = cratonvm_gc_core::gc_quiescence::xt_cycle_pinned_jit_root_count() as u64;
        // gci i24/jp: this cycle's cross-thread pins on their own (`by_xt`).
        let xt = if jit_frames_live_for_pins() {
            cratonvm_gc_core::gc_quiescence::xt_cycle_pinned_jit_roots_snapshot()
        } else {
            Vec::new()
        };
        row.xt = pinned_regions_claimed_by(pinned, &to_regions(&xt[..], false));
        self.pin_attribution.add(&row);
    }

    /// [`Self::jit_pinned_region_set_screened`] PLUS every region holding a root that is
    /// not the start of a live object.
    ///
    /// The root array is over-approximate by design. `collect_roots` filters
    /// operand-stack slots with `is_heap_addr` — a RANGE check — because the
    /// strict `is_object_address` probe dropped genuine young / mid-init roots
    /// when it was used there (2026-08-04). So a primitive `long` (a file size,
    /// a hash, a computed interior address) whose bits land in the heap range is
    /// handed to the collector as a root. For a NON-MOVING sweep that is safe:
    /// a false positive only over-retains, which is what that comment says.
    ///
    /// A MOVING collector cannot leave it at that. The root loops screened a
    /// root for CSet membership and nothing else, so `evacuate_object` read a
    /// header at whatever address arrived, sized a memcpy from those bytes and
    /// copied them (measured: 6-8 such roots per affected H2 pause, all COPIED,
    /// and the copy then broke the destination region's object grid), installed
    /// old→new in the pointer map — which the VM's post-GC remap then applies to
    /// the very operand-stack slot the address came from, rewriting a Java
    /// `long` — and, when to-space ran out instead, self-forwarded it into an
    /// identity entry that `retry_after_evacuation_failure` picked up as a
    /// "live self-forwarded seed".
    ///
    /// Evacuating such a root is unsound and so is skipping it, for the same
    /// reason: the collector cannot tell a real reference from a long, so it may
    /// neither rewrite the slot nor drop what it might point at. Pin the region
    /// instead — precisely what this collector already does with a conservative
    /// JIT root it cannot rewrite.
    ///
    /// **What this does not catch.** The screen reads the bytes AT the address,
    /// so an interior pointer whose bytes happen to decode as a header is
    /// indistinguishable from an object start — a zeroed field cell is the
    /// standard example (`class_id=0, num_slots=0, kind=Object` sizes to exactly
    /// `HEADER_SIZE`), and the rset-source walk documents the same hole. Every
    /// non-object root measured on H2 was of the shape this DOES catch
    /// (`BadElementTag`, `ImplausibleShape`), but "no such root reached the
    /// evacuator" is not what this guarantees; "no root whose bytes are not a
    /// header did" is.
    ///
    /// The 8-byte header widened that hole (`obj + 8` of a compact instance
    /// decodes cleanly), so a root must also carry `GC_FLAG_HEADER`. Unlike
    /// the old-generation marker's candidate screen, where a false negative
    /// drops a root (see `header_reserved_fields_plausible` in `gen_heap.rs`),
    /// a false negative here only pins a region that could have moved: a real
    /// object without the flag stays where it is and stays live.
    pub fn pinned_region_set_including_non_object_roots(
        &self,
        regions: &[G1Region],
        roots: &[ObjectRef],
    ) -> RegionSet {
        self.pinned_region_set_with_pin_screen(
            regions,
            roots,
            false,
            false,
        )
    }

    /// [`Self::pinned_region_set_including_non_object_roots`] for a young or
    /// mixed pause driver: a blocked peer's captured stack word pins the
    /// object it lies in, not its region, under `g1_pins_peer_stack_objects`.
    /// Publishes this pause's object pins ([`Self::peer_object_pinned`]).
    pub fn pause_pinned_region_set(&self, regions: &[G1Region], roots: &[ObjectRef]) -> RegionSet {
        self.pinned_region_set_with_pin_screen(
            regions,
            roots,
            false,
            true,
        )
    }

    /// [`Self::pinned_region_set_including_non_object_roots`] with the gci
    /// i10/pp above-cursor screen as a parameter, so both arms are testable
    /// without the environment.
    ///
    /// `skip_above_cursor` (`CRATONVM_G1_PIN_SKIP_ABOVE_CURSOR=1`, default
    /// OFF): a JIT pin word or a non-object root that lies strictly above its
    /// region's cursor ([`pin_word_names_no_allocation`]) pins nothing. Such a
    /// word names no byte ever allocated in the region, so there is no object
    /// for it to hold in place, and the root loops already leave a non-object
    /// CSet root untouched (`note_root_object_plausibility` /
    /// `note_root_object_plausibility_or_skip`: skipped, never evacuated, slot
    /// unchanged). The foreign-attach rows counted 13 170 such `AboveCursor`
    /// words over 4 176 young pauses (stale words of recycled regions in the
    /// churn thread's spill slots and the parked main thread's frames), each
    /// holding a whole Eden region out of the pause and into the wholesale
    /// Phase-2 / Phase-4 walks. Pins are computed before Phase 1, so every
    /// cursor read here is the mutators' published one.
    pub fn pinned_region_set_with_pin_screen(
        &self,
        regions: &[G1Region],
        roots: &[ObjectRef],
        skip_above_cursor: bool,
        object_pins: bool,
    ) -> RegionSet {
        let (mut set, mut declined) = self.jit_pinned_region_set_screened(
            skip_above_cursor.then_some(regions),
            regions,
            !object_pins,
        );
        if object_pins {
            declined += self.pin_peer_stack_objects(regions, skip_above_cursor, &mut set);
        }
        for root in roots {
            let addr = root.as_ptr() as usize;
            let Some(idx) = self.lookup_region_for_addr(addr) else {
                continue;
            };
            if skip_above_cursor
                && !set.contains(&idx)
                && self.pin_word_above_cursor(regions, addr)
            {
                declined += 1;
                continue;
            }
            // BOTH screens -- see `addr_is_followable_object`. Gating this on
            // the tag screen alone is what put the containing region of an
            // INTERIOR root into the CSet: the root passed, its region was not
            // pinned out, and the root loop then evacuated the array element it
            // pointed at as if it were an object. Pinning the region out here
            // is the correct treatment and the cheap one -- the region simply
            // does not join the CSet, so nothing in it moves and the interior
            // root stays valid, which is what a conservatively-discovered root
            // needs and what this set was built to give it.
            if set.contains(&idx) {
                continue;
            }
            // A root must carry `GC_FLAG_HEADER`, which every allocator stamps
            // and no field word does in practice. Since the 8-byte header an
            // INTERIOR pointer to a compact instance's field (`obj + 8` from a
            // JIT frame's saved-register image) decodes as a plausible header:
            // class id and mark are the halves of the field, and a pointer's
            // upper half is a clean NEUTRAL plain-object mark with a zero flags
            // byte. That passed `addr_is_followable_object`, the root loop
            // EVACUATED it, and the "claim" wrote FORWARDED into what was the
            // upper half of the real object's forwarding target
            // (`RETIRE_TARGET_MISMATCH`). Refusing here pins the region, the
            // treatment a root that cannot be rewritten already gets.
            // SAFETY: `addr` lies in an arena region (`lookup_region_for_addr`).
            let stamped = unsafe {
                (*(addr as *const ObjectHeader)).gc_flags() & cratonvm_types::GC_FLAG_HEADER != 0
            };
            if !stamped {
                let n = ROOT_WITHOUT_HEADER_FLAG_PINNED.fetch_add(1, Ordering::Relaxed) + 1;
                if n <= 4 || n.is_power_of_two() {
                    let prov = cratonvm_gc_core::gc_quiescence::lookup_jit_root_provenance(addr)
                        .unwrap_or_else(|| "none".into());
                    tracing::warn!(
                        "[g1] root names a word with no GC_FLAG_HEADER (#{n}): addr=0x{addr:x} \
                         prov=[{prov}] -- an interior pointer; pinning region {idx} instead of \
                         evacuating it",
                    );
                }
            }
            if stamped && self.addr_is_followable_object(regions, addr, "root-pin-scan") {
                continue;
            }
            let n = NON_OBJECT_ROOT_SEEN.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 8 || n.is_power_of_two() {
                let prov = cratonvm_gc_core::gc_quiescence::lookup_jit_root_provenance(addr)
                    .unwrap_or_else(|| "none".into());
                tracing::warn!(
                    "[g1] root is not an object (#{n}): addr=0x{addr:x} prov=[{prov}] {} — pinning region \
                     {idx} instead of evacuating it.",
                    self.describe_rejected_address(regions, addr),
                );
            }
            set.insert(idx);
        }
        if declined > 0 {
            self.phase_totals.note_pin_words_above_cursor(declined);
        }
        set
    }

}
