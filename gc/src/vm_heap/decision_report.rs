use std::sync::atomic::Ordering;
use cratonvm_gc_core::gc_metrics::*;

/// A one-or-two-line human-readable statement of what the collector actually
/// did on the last cycle, and why.
///
/// This is the ground truth that settles the `docs/GC.md` ↔ `ARCHITECTURE.md`
/// disagreement about whether young collections move — see
/// `tlab-and-card-audit.md` §3.
pub fn collector_decision_report() -> String {
    // Distribution first: the last decision alone has repeatedly misled.
    let (moving, non_moving, skipped, rows) = decision_histogram();
    let mut hist = String::new();
    if moving + non_moving + skipped > 0 {
        hist.push_str(&format!(
            "[GC] decision histogram: moving={moving} non_moving={non_moving} skipped={skipped}"
        ));
        for (label, n) in &rows {
            hist.push_str(&format!(" {label}={n}"));
        }
        hist.push('\n');
    }
    let mut s = match last_collector_decision() {
        // 2026-09-20: this arm used to state, flatly, that no collection had
        // run. That sentence is FALSE on any allocating run of the DEFAULT
        // backend, because ZGC never calls `record_collector_decision` — the
        // three call sites are `g1.rs` and the two generational arms. It is the
        // line an investigation starts from (it is emitted from vm-cli's
        // shutdown hook on both exit arms), and "no collection has run" is the
        // single most misleading thing it could say, because it invites the
        // reader to stop. Now it says what it actually knows, and names why the
        // default backend produces it. Filed as
        // `docs/feature-designs/zgc-architecture.md`.
        //
        // 2026-09-23: ZGC records one now, from `VmHeap`'s dispatch
        // (`zgc_recording_decision`), so on every backend this arm means what
        // it says. The denial is kept: a collection driven through a path that
        // bypasses `VmHeap` (a unit test calling a collector directly) still
        // records nothing, and "no collection has run" is still the reading
        // that ends an investigation early.
        None => format!(
            "[GC] decision: no collector decision has been recorded \
             (moving_young_requested={}). This does NOT mean no collection has run yet: \
             every backend records one per collection that goes through `VmHeap` (ZGC since \
             2026-09-23), so on a run that collected, an empty record means the collector \
             was driven some other way — read `[GC] zgc-real: collections=N` or \
             `[GC] generational: minor=` for the count.",
            crate::gc_quiescence::moving_young_enabled(),
        ),
        Some(d) => {
            let mut s = d.to_string();
            let fallbacks = crate::gc_quiescence::moving_young_coverage_fallback_count();
            let cycles = crate::gc_quiescence::moving_young_cycle_count();
            s.push('\n');
            s.push_str(&format!(
                "[GC] decision history: moving_cycles_under_live_jit={cycles} \
                 coverage_fallbacks={fallbacks}"
            ));
            // The peer ledger only runs when `peer_depth > 0`. Report the zeros
            // beside the fallbacks, because a cycle that skipped the ledger and
            // a cycle the ledger accepted are indistinguishable in every other
            // counter here -- and only one of them was actually checked.
            let pz = crate::gc_quiescence::PEER_DEPTH_ZERO_TOTAL.load(Ordering::Relaxed);
            let pz_torn = crate::gc_quiescence::PEER_DEPTH_ZERO_TORN.load(Ordering::Relaxed);
            let pz_quiet =
                crate::gc_quiescence::PEER_DEPTH_ZERO_GLOBAL_ZERO.load(Ordering::Relaxed);
            s.push('\n');
            s.push_str(&format!(
                "[GC] peer ledger skipped: peer_depth_zero={pz} \
                 (global_zero={pz_quiet} torn_global_lt_local={pz_torn})"
            ));
            s
        }
    };
    if !hist.is_empty() {
        s = format!("{}{s}", hist);
    }
    // G1 states its own last cycle. Absent on a Generational/ZGC run, so the
    // report keeps its previous shape there byte-for-byte.
    if let Some(g1) = last_g1_cycle() {
        s.push('\n');
        s.push_str(&g1.to_string());
    }
    // How often G1 found this collection's JIT root set incomplete. A rate
    // near zero says the empty-CSet fail-safe costs nothing; a high rate says
    // reclamation is being throttled by an unproven coverage obligation and
    // the histogram row names which one.
    let (g1_pauses, g1_incomplete) = g1_pause_coverage_counts();
    if g1_pauses > 0 {
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 root coverage: pauses={g1_pauses} incomplete={g1_incomplete} ({pct:.2}%){reasons}",
            pct = 100.0 * g1_incomplete as f64 / g1_pauses as f64,
            // The reason, appended rather than on its own line so a reader
            // cannot see the rate without it.
            reasons = {
                let rows = g1_coverage_reason_counts();
                if rows.is_empty() {
                    String::new()
                } else {
                    let mut s = String::from(" reasons:");
                    for (label, n) in rows {
                        s.push_str(&format!(" {label}={n}"));
                    }
                    s
                }
            },
        ));
        // The narrow sibling of the line above, and the one that can actually
        // select: how often a pause ran with a compiled frame live and NOTHING
        // published to pin. Those are the pauses the fail-safe refuses, so this
        // rate IS the cost of the fail-safe. Printed alongside so the two are
        // never read as the same number — `incomplete` near 100% is normal,
        // `empty_publication` above a trickle is not.
        let empty_pub = g1_empty_jit_publication_count();
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 jit publication: pauses={g1_pauses} empty_while_in_jit={empty_pub} ({:.2}%)",
            100.0 * empty_pub as f64 / g1_pauses as f64,
        ));
    }
    // The G1 guard counters, every one of which is a process static and every
    // one of which is expected to read ZERO.
    //
    // Here rather than in `G1Collector::print_gc_summary` for the reason the
    // `promo_dest` block below already gives, and which turns out to have
    // applied to all of them: that function is reached only from `vm-cli`'s
    // normal-return teardown, and `System.exit` never unwinds Rust frames, so
    // on every JUnit workload in the suites (`JUnitCore` exits) not one of
    // these lines had ever been emitted. Their own doc comments say they print
    // "unconditionally ... before the early return" so that a zero can be
    // CITED as evidence; the zero was never printed at all.
    //
    // Unconditional here too, and for the same reason: a counter that only
    // appears when it is non-zero cannot be told apart from a counter whose
    // report never ran.
    {
        let (holder_rejected, holder_clamped) = cratonvm_gc_g1::g1::evacuation_holder_counts();
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 evac_ref_rejected={} (torn={}) evac_holder_rejected={holder_rejected} evac_holder_clamped={holder_clamped} source_walk_desync={}",
            cratonvm_gc_g1::g1::evacuation_refs_rejected(),
            cratonvm_gc_g1::g1::evacuation_refs_rejected_torn(),
            cratonvm_gc_g1::g1::evacuation_source_walk_desyncs(),
        ));
        s.push('\n');
        // LANE W4-B — SEEN and COPIED beside SKIPPED, and the
        // re-evacuation guard beside them.
        //
        // `NON_OBJECT_ROOT_COPIED`'s own doc said the pair was "still
        // printed". It was not: this line carried SKIPPED alone and
        // `non_object_root_counts()` had no caller in the workspace. The two
        // are a matched pair by construction — SKIPPED is what the root loops
        // do INSTEAD of copying, so SEEN is their denominator and COPIED is
        // structurally zero unless someone reintroduces the copy, which is the
        // regression this pair exists to catch.
        //
        // `reevacuated_after_retire` is the same shape and was dark for the
        // same reason: expected zero, non-zero means a driver retired its
        // forwards before a phase that still evacuates, which makes a second
        // live copy of an object and overwrites the correct `pointer_map`
        // entry — the object-IDENTITY defect the Spring Boot residual
        // measured as reflection metadata reading back null. Its static
        // survived only inside a `tracing::warn!`, so on a clean run the
        // number could not be read at all and its silence was
        // indistinguishable from the report never running.
        //
        // Capitals on `COPIED` and `REEVACUATED` for the reason
        // `root_remap_audit: UNREMAPPED` shouts: these are correctness
        // signals, not engagement numbers, and a reader scanning a wall of
        // census lines has to be able to see which zeros are load-bearing.
        //
        // The `[GC] g1 non_object_roots_skipped=` PREFIX is unchanged so the
        // reachability test that pins it keeps pinning it.
        let (nor_seen, nor_copied) = cratonvm_gc_g1::g1::non_object_root_counts();
        s.push_str(&format!(
            "[GC] g1 non_object_roots_skipped={} seen={nor_seen} COPIED={nor_copied} \
             REEVACUATED_AFTER_RETIRE={}",
            cratonvm_gc_g1::g1::non_object_roots_skipped(),
            cratonvm_gc_g1::g1::reevacuated_after_retire(),
        ));
        // Which evacuator actually ran. Without this pair an A/B over the
        // worker count measures an unknown, and the nearest-looking counter
        // belongs to a different collector.
        let (par, ser, workers) = cratonvm_gc_g1::g1::g1_young_evac_counts();
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 young evacuation: parallel={par} serial={ser} workers_last={workers}"
        ));
        s.push('\n');
        // The G1 near-full proxy-define page: the in-place full compaction's
        // census (`g1_compact_in_place.rs`), and why it last declined.
        let (ipc, ipc_reclaimed, ipc_moved, ipc_freed, ipc_declined, ipc_why) =
            cratonvm_gc_g1::g1::g1_in_place_compaction_census();
        // r23 w6: the starved fulls (`CRATONVM_G1_STARVED_FULL`) among them.
        let (sf, sf_freed) = cratonvm_gc_g1::g1::g1_starved_full_census();
        s.push_str(&format!(
            "[GC] g1 in-place-compaction: runs={ipc} reclaimed_bytes={ipc_reclaimed} \
             moved_bytes={ipc_moved} freed_regions={ipc_freed} declined={ipc_declined} \
             last_decline={ipc_why} declines=[{}] excluded=[{}] starved_fulls={sf} \
             starved_full_freed_regions={sf_freed} phase_us={:?}",
            cratonvm_gc_g1::g1::g1_in_place_compaction_declines(),
            cratonvm_gc_g1::g1::g1_in_place_compaction_exclusions(),
            cratonvm_gc_g1::g1::g1_in_place_phase_us(),
        ));
        s.push('\n');
        // g1ep7: the concurrent cycles' root regions.
        let [rr_cycles, rr_declined, rr_objects, rr_bytes, rr_us] = cratonvm_gc_g1::g1::g1_root_region_census();
        s.push_str(&format!(
            "[GC] g1 root-regions: cycles={rr_cycles} declined={rr_declined} objects={rr_objects} \
             bytes={rr_bytes} scan_us={rr_us}"
        ));
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 implausible_legacy_headers={} copy_shape_drift={}",
            cratonvm_gc_g1::g1::evacuation_implausible_class0_copies(),
            cratonvm_gc_g1::g1::evacuation_copy_shape_drifts(),
        ));
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 flat_walk_refused_array={} kept_seed_rejected={}",
            cratonvm_gc_g1::g1::flat_walks_refused_for_array(),
            cratonvm_gc_g1::g1::kept_seeds_rejected(),
        ));
    }
    // Promotion destination supply. Here rather than in
    // `G1Collector::print_gc_summary` because that function runs on the
    // normal-return arm only, and every workload this number is wanted for
    // ends in `System.exit`.
    //
    // `resumed_dest_regions` near zero beside a large pause count is the
    // pre-2026-09-06 behaviour, in which each worker's promotion TLAB took a
    // whole fresh Free region every pause and the old generation grew by the
    // WORKER COUNT per young pause however little was promoted. Read it
    // against `[GC] g1 cycle` above. See `g1::SharedEvac::resume`.
    {
        use std::sync::atomic::Ordering as O;
        let resumed = cratonvm_gc_g1::g1::PARALLEL_EVAC_RESUMED_DEST_REGIONS.load(O::Relaxed);
        let shared = cratonvm_gc_g1::g1::PARALLEL_SHARED_DEST_ALLOCS.load(O::Relaxed);
        let exhausted = cratonvm_gc_g1::g1::PARALLEL_TLAB_POOL_EXHAUSTED.load(O::Relaxed);
        // LANE D wave 2 — `proved_empty` beside `tlab_pool_exhausted` is what
        // says whether the shared-destination fallback is DOING anything or
        // merely being asked. Before the latch the two were equal by
        // construction and `shared_dest_allocs` was zero, which is a fallback
        // that costs a full region-table pass per object and cannot succeed.
        let proved = cratonvm_gc_g1::g1::PARALLEL_SHARED_DEST_PROVED_EMPTY.load(O::Relaxed);
        if resumed | shared | exhausted != 0 || proved != 0 {
            s.push('\n');
            s.push_str(&format!(
                "[GC] g1 promo_dest: resumed_dest_regions={resumed} shared_dest_allocs={shared} tlab_pool_exhausted={exhausted} shared_dest_proved_empty={proved}"
            ));
        }
    }
    // LANE `gate` (2026-09-22) — THE BLOCK-OFFSET TABLE'S THREE CENSUSES, on
    // one line, because none of the three means anything read alone.
    //
    // `g1::block_offset_no_jump_census` was filed as an orphan by
    // `scripts/check-orphan-instruments.sh` on its first real run
    // (`docs/feature-designs/concurrent-gc-maturation.md`)
    // and the sweep that followed it found its two siblings in the same state:
    // every reader of all three was under `gc/tests/`, so on a shipped binary
    // the block-offset lever could be turned on and produce no reading at all.
    //
    // The three answer one question between them, and `g1.rs` says so at each
    // definition:
    //
    // * `jumps` / `bytes_jumped` / `entries_refused` — did the table move the
    //   walk, how far, and how often did it name something that is not an
    //   object header (`refused` is the signal that says turn the lever back
    //   off);
    // * `tail_breaks` / `no_entry` — WHY `jumps == 0`, which has three causes
    //   wanting three different responses. A high `tail_breaks` with a zero
    //   `no_entry` says the walk never had a gap to jump over — a fact about
    //   the workload. A non-zero `no_entry` says it did and the table had no
    //   answer — a fact about the table;
    // * `audit_spans` / `audit_objects` / `audit_violations` —
    //   `CRATONVM_G1_BLOCK_OFFSET_AUDIT`'s verdict. The third is a documented
    //   MUST-BE-ZERO: a non-zero says an object holding a cross-region
    //   reference was inside bytes a jump skipped, which is the producer rule
    //   breaking and the premise the whole optimisation rests on.
    //
    // Unconditional and printed with its zeros, per the rule the blocks above
    // and below keep re-deriving: a census that appears only when it is
    // non-zero cannot be told apart from a census whose report never ran.
    // `audit_spans=0` is itself the reading that says the audit lever was off.
    {
        let (jumps, bytes_jumped, refused) = cratonvm_gc_g1::g1::block_offset_process_census();
        let (tail_breaks, no_entry) = cratonvm_gc_g1::g1::block_offset_no_jump_census();
        let (audit_spans, audit_objects, audit_violations) = cratonvm_gc_g1::g1::block_offset_audit_census();
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 block-offset: jumps={jumps} bytes_jumped={bytes_jumped} \
             entries_refused={refused} tail_breaks={tail_breaks} no_entry={no_entry} \
             audit_spans={audit_spans} audit_objects={audit_objects} \
             audit_violations={audit_violations}"
        ));
    }
    // LANE D (2026-09-20) — the parallel evacuator's PER-WORKER engagement
    // split. Here for exactly the reason the `promo_dest` block above is:
    // this is the census emitted on both exit arms, and the workloads whose
    // parallelism is in question all end in `System.exit`.
    //
    // Unconditional, including the "no parallel evacuation ran" form, because
    // the question this answers — "did the 23 workers this pause woke do any
    // of the work?" — has a wrong answer that looks exactly like a missing
    // report. See `g1::EvacWorkerCensus`.
    {
        s.push('\n');
        s.push_str(&cratonvm_gc_g1::g1::g1_evac_worker_census_report());
    }
    // LANE W6-P (2026-09-21) — THE MIXED-PAUSE DENOMINATOR, one line.
    //
    // Unconditional, and printed as `0 0` when no mixed pause ran, because
    // THAT is the reading. Every A/B this lever has been given compared two
    // whole runs without establishing that either took a mixed pause, and on
    // the current battery neither does: zero mixed pauses in up to 8,945 young
    // ones across eight `G1OldBurstProbe` configurations. A run that prints
    // `mixed-route: serial=0 parallel=0` has measured nothing about
    // `CRATONVM_G1_PARALLEL_MIXED`, whatever its wall clock says.
    {
        // Leading newline: the per-worker census report above does not end in
        // one, so without this the two lines are emitted concatenated
        // (`idle_ms=330[GC] g1 mixed-route: ...`). Observed on a real run.
        let (serial, parallel) = cratonvm_gc_g1::g1::g1_mixed_route_counts();
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 mixed-route: serial={serial} parallel={parallel} total={}\n",
            serial + parallel
        ));
    }
    // LANE W7-W (2026-09-21) — HOW CLOSE THE EVACUATOR'S LOAD-BALANCING VALVE
    // CAME TO OPENING, which `spills` alone cannot say.
    //
    // `EVAC_LOCAL_PUBLISH_HIGH = 256` is compared against a worker's local
    // stack depth. `spills=0` is reported by a workload whose stack never
    // exceeded 4 AND by one that reached 255 on every push — opposite
    // findings about whether work sharing is reachable on real graphs, with
    // identical output. This block prints the distribution behind the count.
    //
    // Unconditional including the disarmed form, for the reason the block
    // above gives for printing `0 0`: a line that appears only when it has
    // something to say cannot be told apart from a report that never ran.
    // See `g1::EvacShareCensus` and
    // `docs/feature-designs/concurrent-gc-maturation.md`.
    //
    // No trailing newline: the next block opens with its own, and the two
    // together emitted an EMPTY stderr line on every `CRATONVM_GC_STATS` run
    // (t1-Q, read as an extra stdout line by a harness that strips `[`-led
    // diagnostics from merged output).
    {
        s.push_str(&cratonvm_gc_g1::g1::g1_evac_share_census_report());
    }
    // LANE W3-C (2026-09-21) — THE MARKING CYCLE'S OWN NUMBERS.
    //
    // Everything in this block was already being counted and none of it had a
    // reader on any path a shipped binary takes. That is the other half of
    // P7's failure — P7 is a correct report on an arm the process does not
    // reach; these were correct counters with no report at all — and it ends
    // the same way: a question that cannot be answered on a release build.
    //
    // HERE rather than in `G1Collector::print_gc_summary` for a reason that is
    // now about the DATA rather than about reachability. `print_gc_summary` is
    // reachable on both arms (`vm-cli`'s `VM_FOR_SHUTDOWN` weak handle), so
    // that is no longer the discriminator. The discriminator is `&self`: every
    // counter below is a process static, so it is readable without a live
    // collector and survives a VM that has already been torn down, whereas
    // `print_gc_summary` can only run while one is alive. A process static
    // belongs on the report that needs no VM. See
    // `w3c-print-gc-summary-and-the-system-exit-arm.md`.
    {
        // The mark cycle's two stop-the-world pauses, NEITHER of which was
        // timed before this line existed: `remark` and `cleanup` do not go
        // through `record_collection_with_phases`, so they are absent from
        // `[GC-STAT]` and from the pause percentiles.
        //
        // `cleanup_pauses=0` is the engagement reading and it is the first
        // thing to check in any marking A/B: on the default build the JIT
        // never drives the cycle (`CRATONVM_G1_JIT_MARK_DRIVER` is opt-in), so
        // a JIT'd workload runs NO mark cycles and every marking-lane flag in
        // this round is inert on it whatever its own counter says.
        let (remark_n, remark_us, cleanup_n, cleanup_us) = cratonvm_gc_g1::g1::mark_pause_totals();
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 mark-pauses: remark_pauses={remark_n} remark_us={remark_us} \
             cleanup_pauses={cleanup_n} cleanup_us={cleanup_us} jit_mark_driver={}",
            false,
        ));
        // LANE W7-D (2026-09-21) — WHICH DOOR, and what it saw.
        //
        // The line above is the outcome; this block is the cause. A default
        // build reads `remark_pauses=1, cleanup_pauses=0` — one cycle opened,
        // none closed — and that single reading is produced by two opposite
        // defects: no driver ran on the path the pauses took, or a driver ran
        // and found the marker unfinished every time. `visits` against
        // `waiting` separates them, per driver, and the rows are printed
        // including the zeroes because a door that was never reached IS the
        // finding (README §5 rule 8: a check that cannot see anything and a
        // check that saw nothing must not read alike).
        //
        // Also the engagement denominator for `CRATONVM_G1_ALLOC_MARK_DRIVE`:
        // an arm on which `door=alloc_fail visits=0` is an arm on which that
        // flag could not have acted, whatever the rest of the run says.
        {
            s.push('\n');
            s.push_str(&cratonvm_gc_g1::g1::mark_door_report());
        }
        // LANE W7-D — WHICH DOOR TOOK THE PAUSE, beside which door drove the
        // cycle. The two questions are different and the answer to the second
        // is only interpretable against the first.
        //
        // `gc_entry_census` has carried this since the ZGC wedge work and has
        // been printed on the ZGC arm only, so on G1 — the collector whose
        // mark-cycle drivers hang off specific entry points — the one census
        // that says which entry point runs had no reader at all. It is what
        // turns "the allocation-failure pause had no lifecycle call" from a
        // reading of the source into a measurement: `forced by
        // alloc-object-shared: N` against `maybe_gc_needs=M` is the ratio the
        // whole default decision rests on.
        {
            let (needs, requested, forced, native) = cratonvm_types::gc_entry_census::totals();
            s.push('\n');
            s.push_str(&format!(
                "[GC] g1 gc-entry: maybe_gc_needs={needs} maybe_gc_requested={requested} \
                 forced={forced} from_native={native}"
            ));
            for (site, n) in cratonvm_types::gc_entry_census::forced_sites() {
                s.push('\n');
                s.push_str(&format!("[GC] g1 gc-entry:   forced by {site}: {n}"));
            }
        }
        // The SATB per-thread registry walk, inside both of those pauses.
        // `threads_visited` is the lock acquisitions the walk pays;
        // `threads_held` is how many of them found anything. The gap between
        // the two is the entire value of a dirty-buffer list, which is the
        // first of the three fixes
        // `w2c-the-satb-registry-walk-is-still-o-threads-per-cycle.md` refused
        // to choose between without this number.
        let (wc, tv, th, ov, oh, we, wn) = crate::satb::registry_walk_census();
        // `barrier_logs` is on the SAME line as `entries` on purpose. A zero
        // `entries` means "no mutator held an unflushed bucket at a cycle
        // boundary" only if the barrier fired at all; without the denominator
        // beside it the same zero also reads as "SATB never ran", which would
        // be a correctness event rather than a cost finding. That is §7.2 of
        // `orchestrator-wave-1-measurements.md` — a probe that did not perform
        // the operation it studied — and it is one number away.
        let (blogs, bspills) = crate::satb::barrier_log_census();
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 satb-walk: calls={wc} threads_visited={tv} threads_held={th} \
             orphans_visited={ov} orphans_held={oh} entries={we} total_us={} \
             us_per_call={:.1} barrier_logs={blogs} barrier_spills={bspills} discard={}",
            wn / 1_000,
            if wc == 0 {
                0.0
            } else {
                wn as f64 / 1_000.0 / wc as f64
            },
            crate::gc_flags().g1_cleanup_satb_discard,
        ));
        // The evacuation-failure drain's repeated whole-heap fix-up.
        //
        // Wave 2 added these four because each drain pass OVERWROTE the
        // previous pass's `phases.fixup_*`, so a pause that spent its time in
        // eight whole-heap walks reported the cost of the last one — and then
        // nothing read them, so the eight-walk claim STILL could not be checked
        // on a shipped binary. `walks` above `pauses_that_drained` is the
        // repetition factor the page is about.
        let (df_walks, df_regions, df_bytes, df_us) = cratonvm_gc_g1::g1::drain_fixup_totals();
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 drain-fixup: walks={df_walks} regions={df_regions} bytes={df_bytes} \
             us={df_us} narrow={}",
            true,
        ));
        // Two more guard counters that had no reader anywhere.
        //
        // `region_type_decode_failsafe` is documented "expected to be ZERO, and
        // structurally unreachable" and is a counter rather than a
        // `debug_assert!` precisely so it can be read instead of aborting a GC
        // worker — but nothing read it, so the argument for preferring a
        // counter to an assertion was not being honoured. `resurrect_*` is the
        // denominator its own doc calls "the question the collector could not
        // answer about itself": whether Phase 3.5 runs on this workload at all.
        let (res_runs, res_copied) = cratonvm_gc_g1::g1::resurrect_phase_census();
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 mark-residue: resurrect_runs={res_runs} resurrect_copied={res_copied} \
             region_type_decode_failsafes={}",
            crate::region::region_type_decode_failsafes(),
        ));
        // The class-loader registry population, which is what decides whether
        // `CRATONVM_G1_MARK_SIDE_TABLES` has anything to remove on this
        // workload. `cycles_with_loader_pins=0` falsifies any A/B over that
        // flag before its wall clock is even read. See
        // `cratonvm_gc_g1::g1::MARK_CYCLES_STARTED`.
        let (mc_started, mc_pins, mc_hits) = cratonvm_gc_g1::g1::mark_loader_pin_census();
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 mark-loader-pins: cycles_started={mc_started} \
             cycles_with_loader_pins={mc_pins} loader_tail_hits={mc_hits} side_tables={}",
            crate::gc_flags().g1_mark_side_tables,
        ));
        // The IHOP policy's ENGAGEMENT, as opposed to its state (which
        // `print_gc_summary`'s `[GC] g1 ihop:` line carries).
        //
        // `polls` is how many times the mark-cycle trigger consulted the
        // policy at all. Until this wave the answer was ZERO on every
        // production run: `VmHeap::g1_should_start_marking` re-implemented the
        // level test inline and never called `check_ihop`, so the back-off
        // behind `CRATONVM_G1_IHOP_BACKOFF` was unreachable. The lever is now
        // connected, and this number is what says so — a reader must be able
        // to tell "armed and never fired" from "never asked".
        let (polls, untimed) = cratonvm_gc_g1::g1::ihop_poll_census();
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 ihop-engagement: polls={polls} untimed_cycles={untimed} \
             adaptive={} gross_growth={} backoff={} backoff_deadline={}",
            crate::gc_flags().g1_adaptive_ihop,
            crate::gc_flags().g1_ihop_gross_growth,
            crate::gc_flags().g1_ihop_backoff,
            // LANE W5-C — the back-off's counted fail-safe. On this line for
            // the reason its three neighbours are: this is the line a reader
            // consults to find out whether a marking lever could have run at
            // all, and a lever missing from it is one whose zero cannot be
            // told from an absence.
            crate::gc_flags().g1_mark_backoff_deadline,
        ));
    }
    // THE 2026-09-08 EVACUATION SCREENS, printed UNCONDITIONALLY so a zero is
    // citable evidence rather than a silence.
    //
    // Every one of these is expected to be ZERO on a healthy run, and each
    // names a distinct door into
    // `g1-eight-byte-write-at-a-live-objects-base-20260906`'s two families:
    //
    //  * `fwd_target_implausible` -- a `MARK_FORWARDED` word decoded to a
    //    target `make_forwarded` could not have installed (`forwarding_target`
    //    strips the low TWO bits, the install asserts the low THREE), so a
    //    non-address was about to be stored into a live reference slot and
    //    pushed onto the gray worklist;
    //  * `supply_non_object` -- an evacuation supply address that is not an
    //    object start;
    //  * `candidate_arena_ptr` -- a candidate whose `class_id`/`shape` pair
    //    recombines to a pointer into this arena, i.e. a body word read at an
    //    address that is not an object start;
    //  * `empty_header_refused` / `empty_header_proved` -- the shape no header
    //    screen can reject (a null `Value` cell's payload word reads as a
    //    zero-field class-0 object), and how often the grid had to be walked to
    //    say so. A large `proved` beside a zero `refused` means the shape is
    //    common and genuine and the proof is paying for nothing;
    //  * `tagged_ref_writes` -- a reference-slot write whose value is not
    //    8-aligned, i.e. a mark word stored where an object address belongs.
    {
        use std::sync::atomic::Ordering as O;
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 evac-screens: fwd_target_implausible={} supply_non_object={} \
             candidate_arena_ptr={} empty_header_refused={} empty_header_proved={} \
             empty_header_waived={} tagged_ref_writes={}",
            cratonvm_gc_g1::g1::FORWARD_TARGET_IMPLAUSIBLE.load(O::Relaxed),
            cratonvm_gc_g1::g1::EVAC_SUPPLY_NON_OBJECT.load(O::Relaxed),
            cratonvm_gc_g1::g1::EVAC_REF_REJECTED_ARENA.load(O::Relaxed),
            cratonvm_gc_g1::g1::EVAC_EMPTY_HEADER_REFUSED.load(O::Relaxed),
            cratonvm_gc_g1::g1::EVAC_EMPTY_HEADER_PROVED.load(O::Relaxed),
            cratonvm_gc_g1::g1::EVAC_EMPTY_HEADER_WAIVED.load(O::Relaxed),
            cratonvm_gc_g1::g1::REF_WRITE_TAGGED_VALUE.load(O::Relaxed),
        ));
    }
    // LANE W6-A — the receiver-side dead-reference screen's POPULATION.
    //
    // `note_deadref_recv` counts a field access whose RECEIVER names no live
    // object: a native that captured a receiver before an allocation and used
    // it afterwards writes into a dead address, and the field it meant to set
    // stays at its JVM default. It is a correctness event and it is expected
    // to be ZERO.
    //
    // It already prints — the first TWENTY-FOUR occurrences, by `eprintln!`,
    // and then nothing. Its own write site says why the counter exists beside
    // that printout: *"Counted before the rate limit, so the number is the
    // population and not the printout … its headline claim — that a
    // receiver-side store is now screened — is exactly the kind of claim a
    // silent instrument can appear to support while being unwired."* The
    // counter was then read by one test and nothing else, so the population
    // was unpublished and a run with 24 reports and a run with 24 000 were
    // indistinguishable. That is rule 3 of this round's method, on a
    // correctness counter.
    //
    // `screen=` beside it is not decoration. The screen is entirely behind
    // `CRATONVM_DBG_DEADREF_STORE` (`gc_flags().dbg_deadref_store` guards all
    // five call sites), so on a default build this number is zero BY
    // CONSTRUCTION. Without the resolved value of the gate, "armed and clean"
    // and "never asked" are the same zero — §7.1 of
    // `orchestrator-wave-1-measurements.md`, which is the failure that
    // retracted a whole measurement this round.
    //
    // Unconditional, including the zero, for the reason every neighbour above
    // is: a counter that appears only when it is non-zero cannot be told apart
    // from a report that never ran.
    {
        s.push('\n');
        s.push_str(&format!(
            "[GC] deadref-recv: DEADREF_RECEIVER_HITS={} screen={}",
            cratonvm_gc_gen::gen_heap::deadref_recv_hits(),
            crate::gc_flags().dbg_deadref_store,
        ));
    }
    // I-6 coverage. Printed whenever the verifier ran at all, including the
    // budgeted release pass, because the interesting reading is `objects`: a
    // zero `dangling` means nothing without the number of objects it is a
    // statement about. `budget_truncated` says how often the budget, rather
    // than the end of the heap, is what ended the pass.
    let verify = gc_metrics_raw();
    if verify.cset_verify_pauses > 0 {
        s.push('\n');
        s.push_str(&format!(
            // LANE W4-B — `bytes`, `unbounded_pauses` and the two switches.
            //
            // W3-C's rule ("every census line names the switch that governs
            // it") applies with unusual force here, because this line has TWO
            // governing switches and one of them is the flag people use to
            // make the line appear. `unbounded_pauses` is the count of pauses
            // that had no budget at all; when it equals `pauses`, every other
            // number on the line describes a whole-heap sweep and
            // `budget_truncated=0` is a tautology rather than a result.
            //
            // `bytes` is the coverage denominator's numerator: paired with the
            // occupied heap it is the sweep RATE, which is the quantity the
            // sampler's whole claim rests on and which nothing published
            // before. See `g1::verify_sweep_pauses` for the rates measured at
            // three heap sizes and for why a flat object budget makes that
            // rate fall linearly with the live set.
            "[GC] g1 cset-verify: pauses={} objects={} bytes={} dangling={} budget_truncated={} unbounded_pauses={} (objects/pause={:.1} bytes/pause={:.0}) budget={} sweep_pauses={}",
            verify.cset_verify_pauses,
            verify.cset_verify_objects,
            verify.cset_verify_bytes,
            verify.cset_verify_dangling,
            verify.cset_verify_truncated,
            verify.cset_verify_unbounded,
            verify.cset_verify_objects as f64 / verify.cset_verify_pauses as f64,
            verify.cset_verify_bytes as f64 / verify.cset_verify_pauses as f64,
            cratonvm_gc_g1::g1::verify_budget_for_report(),
            cratonvm_gc_g1::g1::verify_sweep_pauses_for_report(),
        ));
    }
    if verify.humongous_eager_spans > 0 || verify.humongous_eager_declined > 0 {
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 humongous-eager: spans={} bytes={} declined_pauses={} \
             marked_live={} root_seeded={} walked_sources={}{}",
            verify.humongous_eager_spans,
            verify.humongous_eager_bytes,
            verify.humongous_eager_declined,
            verify.humongous_eager_marked,
            verify.humongous_eager_root_seeded,
            verify.humongous_eager_walked_sources,
            {
                let reasons = g1_eager_decline_counts();
                if reasons.is_empty() {
                    String::new()
                } else {
                    let body: Vec<String> = reasons
                        .iter()
                        .map(|(label, n)| format!("{label}={n}"))
                        .collect();
                    format!(" declined_by: {}", body.join(" "))
                }
            },
        ));
    }
    s
}

