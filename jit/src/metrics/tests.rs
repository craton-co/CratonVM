use super::*;

// The enable flag and the ring are process-wide, and the cargo harness
// runs `#[test]`s in parallel threads of one process. Every test that
// touches either takes this first, so they serialize against each other
// (they cannot serialize against an unrelated `lib.rs` compile test, which
// is why the assertions below are written to tolerate foreign reports
// wherever they cannot exclude them). Defined one level up as
// `METRICS_TEST_LOCK` so `ir_lower`'s wiring test shares the same lock —
// it flips the same process-wide enable flag from another module.
use super::METRICS_TEST_LOCK as TEST_LOCK;

fn sample(class: &str, outcome: Outcome, path: CompilerPath) -> CompilationReport {
    let mut r = CompilationReport::new(class, "m", "()I", true);
    r.outcome = outcome;
    r.path = path;
    r
}

#[test]
fn report_round_trips_through_to_json() {
    let mut r = CompilationReport::new("java/lang/String", "hashCode", "()I", true);
    r.seq = 7;
    r.path = CompilerPath::Optimizing;
    r.outcome = Outcome::Installed;
    r.admission = Some("admitted to the optimizing pipeline".to_string());
    r.nodes_built = Measured::Value(41);
    r.nodes_at_lower = Measured::Value(33);
    r.live_nodes_at_lower = Measured::Value(29);
    r.frame_bytes = Measured::Value(392);
    r.code_bytes = Measured::Value(188);
    r.ir_safepoints = Measured::Value(3);
    r.oop_maps = Measured::Value(0);
    r.deopt_points = Measured::Value(2);
    r.deopt_metadata_bytes = Measured::Value(768);
    r.code_cache_bytes_at_install = Measured::Value(65_536);
    r.total_wall_ns = Measured::Value(123_456);
    r.phases[Phase::Build.index()].add_run(1_000);
    r.phases[Phase::Build.index()].nodes_before = Measured::Value(0);
    r.phases[Phase::Build.index()].nodes_after = Measured::Value(41);
    r.bailouts.push(BailoutRecord {
        category: "ir_verification",
        phase: "pre-lower".to_string(),
        detail: "n3 \"input\"\tout of range\n".to_string(),
    });

    let json = r.to_json();
    // Structure: one line, balanced, and every field the consumer keys on.
    assert!(!json.contains('\n'), "JSON lines must be one line: {json}");
    assert!(json.starts_with('{') && json.ends_with('}'));
    assert!(json.contains("\"seq\":7"));
    assert!(json.contains("\"class\":\"java/lang/String\""));
    assert!(json.contains("\"method\":\"hashCode\""));
    assert!(json.contains("\"descriptor\":\"()I\""));
    assert!(json.contains("\"tier_requested\":\"c2\""));
    assert!(json.contains("\"path\":\"optimizing\""));
    assert!(json.contains("\"outcome\":\"installed\""));
    assert!(json.contains("\"nodes_built\":41"));
    assert!(json.contains("\"frame_bytes\":392"));
    assert!(json.contains("\"code_bytes\":188"));
    assert!(json.contains("\"deopt_metadata_bytes\":768"));
    assert!(json.contains("\"total_wall_ns\":123456"));
    assert!(json.contains("\"category\":\"ir_verification\""));
    // Escaping: the raw quote/tab/newline must not appear unescaped.
    assert!(json.contains("\\\"input\\\""), "{json}");
    assert!(json.contains("\\t"), "{json}");
    assert!(json.contains("\\n"), "{json}");
    // Every phase is present by name, so a consumer never has to guess
    // whether a missing key means zero.
    for phase in Phase::ALL {
        assert!(
            json.contains(&format!("\"phase\":\"{}\"", phase.name())),
            "missing {} in {json}",
            phase.name()
        );
    }
    assert_eq!(
        json.matches("{\"phase\":").count(),
        Phase::ALL.len(),
        "one row per phase"
    );
}

#[test]
fn unmeasured_is_distinguishable_from_zero() {
    let r = CompilationReport::new("C", "m", "()V", false);
    // Nothing supplied yet.
    assert_eq!(r.spills, Measured::NotMeasured);
    assert!(!r.spills.is_measured());
    assert_eq!(r.spills.get(), None);
    assert!(r.to_json().contains("\"spills\":null"));

    let mut measured = r.clone();
    measured.spills = Measured::Value(0);
    assert!(measured.spills.is_measured());
    assert_eq!(measured.spills.get(), Some(0));
    assert!(measured.to_json().contains("\"spills\":0"));

    // The two JSON encodings differ — which is the entire point.
    assert_ne!(r.to_json(), measured.to_json());

    // Same distinction for a phase that never ran vs one that ran fast.
    let never = &r.phases[Phase::Encode.index()];
    assert_eq!(never.runs, 0);
    assert_eq!(never.wall_ns, Measured::NotMeasured);
    assert!(never.json().contains("\"wall_ns\":null"));

    let mut instant = PhaseRecord::new(Phase::Encode);
    instant.add_run(0);
    assert_eq!(instant.runs, 1);
    assert_eq!(instant.wall_ns, Measured::Value(0));
    assert!(instant.json().contains("\"wall_ns\":0"));
}

#[test]
fn phase_names_and_indices_agree() {
    for (i, phase) in Phase::ALL.iter().enumerate() {
        assert_eq!(phase.index(), i, "{} index", phase.name());
        assert_eq!(Phase::from_name(phase.name()), Some(*phase));
    }
    assert_eq!(Phase::from_name("no_such_phase"), None);
    let r = CompilationReport::new("C", "m", "()V", false);
    assert_eq!(r.phases.len(), Phase::ALL.len());
    for phase in Phase::ALL {
        assert_eq!(r.phase(phase).map(|p| p.phase), Some(phase));
    }
}

#[test]
fn ring_buffer_is_bounded_and_evicts_oldest() {
    let _guard = TEST_LOCK.lock();
    set_enabled_for_test(false); // no concurrent compile can publish
    clear_reports();
    let cap = ring_capacity();
    let overshoot = cap + 5;
    for i in 0..overshoot {
        record_report(sample(
            &format!("ring/Probe{i}"),
            Outcome::Installed,
            CompilerPath::Optimizing,
        ));
    }
    let retained = compilation_reports();
    assert_eq!(retained.len(), cap, "ring must be bounded by its capacity");
    // Oldest evicted, newest kept.
    assert_eq!(retained.first().unwrap().class_name, "ring/Probe5");
    assert_eq!(
        retained.last().unwrap().class_name,
        format!("ring/Probe{}", overshoot - 1)
    );
    assert!(last_compilation_report(None).is_some());
    // The trailing `.` anchors the filter to the method-key separator, so
    // `Probe7` cannot be satisfied by `Probe70`. Guarded on the capacity
    // because `CRATONVM_JIT_METRICS_RING` can shrink the retained window.
    if cap >= 8 {
        assert_eq!(
            last_compilation_report(Some("ring/Probe7."))
                .map(|r| r.class_name)
                .as_deref(),
            Some("ring/Probe7")
        );
    }
    assert!(last_compilation_report(Some("ring/Probe0.")).is_none());
    // Sequence numbers are monotonic and gap-free within a retained window.
    for pair in retained.windows(2) {
        assert_eq!(pair[1].seq, pair[0].seq + 1);
    }
    clear_reports();
    set_enabled_for_test(false);
}

#[test]
fn summary_counts_match_recorded_reports() {
    let _guard = TEST_LOCK.lock();
    set_enabled_for_test(false);
    clear_reports();
    record_report(sample("s/A", Outcome::Installed, CompilerPath::Optimizing));
    record_report(sample("s/B", Outcome::Installed, CompilerPath::SinglePass));
    record_report(sample("s/C", Outcome::BailedOut, CompilerPath::Optimizing));
    record_report(sample("s/D", Outcome::Abandoned, CompilerPath::NotEntered));
    let mut fell = sample("s/E", Outcome::Installed, CompilerPath::SinglePass);
    fell.fell_through_to_single_pass = true;
    fell.phases[Phase::Optimize.index()].add_run(500);
    fell.phases[Phase::SinglePass.index()].add_run(1_500);
    record_report(fell);

    let s = summary();
    assert_eq!(s.retained, 5);
    assert_eq!(s.retained, compilation_reports().len());
    fn count(list: &[(&'static str, u64)], key: &str) -> Option<u64> {
        list.iter().find(|(n, _)| *n == key).map(|(_, c)| *c)
    }
    assert_eq!(count(&s.by_outcome, "installed"), Some(3));
    assert_eq!(count(&s.by_outcome, "bailed_out"), Some(1));
    assert_eq!(count(&s.by_outcome, "abandoned"), Some(1));
    assert_eq!(count(&s.by_outcome, "in_progress"), Some(0));
    assert_eq!(count(&s.by_path, "optimizing"), Some(2));
    assert_eq!(count(&s.by_path, "single_pass"), Some(2));
    assert_eq!(count(&s.by_path, "not_entered"), Some(1));
    assert_eq!(s.fell_through_to_single_pass, 1);
    // Every outcome/path bucket is present even at zero, and the buckets
    // sum to the retained count.
    assert_eq!(s.by_outcome.len(), Outcome::ALL.len());
    assert_eq!(
        s.by_outcome.iter().map(|(_, c)| *c).sum::<u64>(),
        s.retained as u64
    );
    assert_eq!(
        s.by_path.iter().map(|(_, c)| *c).sum::<u64>(),
        s.retained as u64
    );
    // Phase totals come from the reports, and a phase nothing ran reports
    // zero runs (not a bogus time).
    assert_eq!(count(&s.phase_totals_ns, "optimize"), Some(500));
    assert_eq!(count(&s.phase_totals_ns, "single_pass"), Some(1_500));
    assert_eq!(count(&s.phase_runs, "optimize"), Some(1));
    assert_eq!(count(&s.phase_runs, "encode"), Some(0));
    assert_eq!(count(&s.phase_totals_ns, "encode"), Some(0));
    // Bailout categories are the process-wide table, verbatim. Compared by
    // NAME rather than by value: the counters are shared with every other
    // test in this process, so a count can advance between the two reads —
    // which is exactly the "sample, not snapshot" contract `bailout.rs`
    // documents.
    let summary_names: Vec<&str> = s.bailout_categories.iter().map(|(n, _)| *n).collect();
    let table_names: Vec<&str> = crate::bailout::bailout_counts()
        .iter()
        .map(|(n, _)| *n)
        .collect();
    assert_eq!(summary_names, table_names);
    assert!(!summary_names.is_empty());
    assert!(s.total_recorded >= 5);
    // JSON is well-formed enough for a consumer to key on.
    let json = s.to_json();
    assert!(json.contains("\"by_outcome\":{\"installed\":3"));
    assert!(json.contains("\"retained\":5"));
    assert!(!json.contains('\n'));

    clear_reports();
    set_enabled_for_test(false);
}

#[test]
fn disabled_recorder_records_and_publishes_nothing() {
    let _guard = TEST_LOCK.lock();
    set_enabled_for_test(false);
    clear_reports();
    let before_total = TOTAL_RECORDED.load(Ordering::Relaxed);

    // Both the explicit opt-out and the flag-driven constructor.
    for rec in [
        CompileRecorder::disabled(),
        CompileRecorder::begin("C", "m", "()V", true),
    ] {
        assert!(!rec.is_enabled());
        assert!(rec.snapshot().is_none());
        // Every hook is reachable and does nothing observable.
        let timer = rec.phase(Phase::Build);
        assert!(!timer.is_measuring(), "no clock is read when disabled");
        drop(timer);
        rec.phase_nodes(Phase::Optimize, 10, 8);
        rec.set_admission("ignored");
        rec.enter_optimizing_pipeline();
        rec.enter_single_pass();
        rec.set_nodes_built(10);
        rec.set_graph_at_lower(10, 9, 2);
        rec.set_peak_live_values(4);
        rec.set_spills(1);
        rec.set_reloads(1);
        rec.note_bailout_reason(BailoutReason::RegisterPressure, "lower");
        // The TLS hooks find nothing to record against.
        let t = current_phase(Phase::Verify);
        assert!(!t.is_measuring());
        drop(t);
        note_current_bailout(&Bailout::new(BailoutReason::RegisterPressure), "verify");
        note_current_peak_live_values(4);
        note_current_spills(3);
        note_current_reloads(2);
        drop(rec); // Drop must not publish.
    }

    assert!(
        compilation_reports().is_empty(),
        "a disabled recorder must publish nothing"
    );
    assert_eq!(
        TOTAL_RECORDED.load(Ordering::Relaxed),
        before_total,
        "a disabled recorder must not advance the publication counter"
    );
    set_enabled_for_test(false);
}

#[test]
fn enabled_recorder_publishes_on_drop_with_phase_and_bailout() {
    let _guard = TEST_LOCK.lock();
    set_enabled_for_test(true);
    clear_reports();
    {
        let rec = CompileRecorder::begin("metrics/Probe", "run", "(I)I", true);
        assert!(rec.is_enabled());
        rec.set_admission("admitted to the optimizing pipeline");
        rec.enter_optimizing_pipeline();
        let t = rec.phase(Phase::Build);
        assert!(t.is_measuring());
        drop(t);
        rec.set_nodes_built(12);
        rec.phase_nodes(Phase::Optimize, 12, 9);
        // The TLS hook reaches the innermost in-flight compilation, which
        // is what lets `ir_verify_reject` record without a signature change.
        {
            let t = current_phase(Phase::Verify);
            assert!(t.is_measuring(), "the TLS hook must find this recorder");
        }
        note_current_bailout(
            &Bailout::with_context(BailoutReason::RegisterPressure, "n7"),
            "pre-lower",
        );
        rec.enter_single_pass();
        let snap = rec.snapshot().expect("in-flight snapshot");
        assert_eq!(snap.outcome, Outcome::InProgress);
        assert!(snap.fell_through_to_single_pass);
    }
    let published =
        last_compilation_report(Some("metrics/Probe.run")).expect("recorder must publish");
    assert_eq!(
        published.outcome,
        Outcome::BailedOut,
        "bailout was recorded"
    );
    assert_eq!(published.path, CompilerPath::SinglePass);
    assert!(published.fell_through_to_single_pass);
    assert_eq!(published.nodes_built, Measured::Value(12));
    assert_eq!(
        published.phases[Phase::Optimize.index()].nodes_after,
        Measured::Value(9)
    );
    assert_eq!(published.phases[Phase::Build.index()].runs, 1);
    assert!(published.phases[Phase::Build.index()].wall_ns.is_measured());
    assert_eq!(published.phases[Phase::Verify.index()].runs, 1);
    assert!(published.total_wall_ns.is_measured());
    assert_eq!(published.bailouts.len(), 1);
    assert_eq!(published.bailouts[0].category, "register_pressure");
    assert_eq!(published.bailouts[0].phase, "pre-lower");
    assert!(published.bailouts[0].detail.contains("n7"));
    // Uninstrumented quantities stay explicitly unmeasured on a real record.
    assert_eq!(published.peak_live_values, Measured::NotMeasured);
    assert_eq!(published.spills, Measured::NotMeasured);
    assert_eq!(published.reloads, Measured::NotMeasured);
    assert!(published.to_json().contains("\"peak_live_values\":null"));

    clear_reports();
    set_enabled_for_test(false);
}

#[test]
fn abandoned_is_the_outcome_when_no_reason_was_recorded() {
    let _guard = TEST_LOCK.lock();
    set_enabled_for_test(true);
    clear_reports();
    drop(CompileRecorder::begin("metrics/Silent", "m", "()V", false));
    let r = last_compilation_report(Some("metrics/Silent")).expect("published");
    assert_eq!(r.outcome, Outcome::Abandoned);
    assert_eq!(r.path, CompilerPath::NotEntered);
    assert!(!r.optimizing_requested);
    assert!(r.to_json().contains("\"tier_requested\":\"c1\""));
    clear_reports();
    set_enabled_for_test(false);
}

#[test]
fn nested_compilations_record_against_the_innermost() {
    let _guard = TEST_LOCK.lock();
    set_enabled_for_test(true);
    clear_reports();
    {
        let outer = CompileRecorder::begin("metrics/Outer", "m", "()V", true);
        {
            let _inner = CompileRecorder::begin("metrics/Inner", "m", "()V", true);
            // `callee_compiler` re-enters the compiler; the TLS hook must
            // charge the callee, not the caller.
            note_current_bailout(&Bailout::new(BailoutReason::RegisterPressure), "build");
        }
        // The inner recorder has popped, so the hook now finds the outer.
        note_current_bailout(
            &Bailout::new(BailoutReason::UnsupportedShape("multianewarray")),
            "build",
        );
        drop(outer);
    }
    let inner = last_compilation_report(Some("metrics/Inner")).expect("inner published");
    let outer = last_compilation_report(Some("metrics/Outer")).expect("outer published");
    assert_eq!(inner.bailouts.len(), 1);
    assert_eq!(inner.bailouts[0].category, "register_pressure");
    assert_eq!(outer.bailouts.len(), 1);
    assert_eq!(outer.bailouts[0].category, "unsupported_shape");
    assert!(inner.seq < outer.seq, "inner publishes first");
    clear_reports();
    set_enabled_for_test(false);
}

#[test]
fn bailouts_per_report_are_bounded() {
    let _guard = TEST_LOCK.lock();
    set_enabled_for_test(true);
    clear_reports();
    {
        let rec = CompileRecorder::begin("metrics/Flood", "m", "()V", true);
        for _ in 0..100 {
            rec.note_bailout_reason(BailoutReason::RegisterPressure, "build");
        }
    }
    let r = last_compilation_report(Some("metrics/Flood")).expect("published");
    assert!(r.bailouts.len() <= 16, "got {}", r.bailouts.len());
    assert!(!r.bailouts.is_empty());
    clear_reports();
    set_enabled_for_test(false);
}

#[test]
fn spill_reload_and_inline_fields_round_trip_through_to_json() {
    let mut r = CompilationReport::new("inl/Probe", "hot", "(I)I", true);
    r.spills = Measured::Value(7);
    r.reloads = Measured::Value(4);
    r.peak_live_values = Measured::Value(11);
    r.inline_candidates = Measured::Value(9);
    r.inlined_sites = Measured::Value(3);
    r.speculative_inlined_sites = Measured::Value(1);
    r.inlined_bytecodes = Measured::Value(214);
    r.inlined_call_count = Measured::Value(4_000_000_000);
    r.inline_refusals = vec![
        ("guard-not-emittable".to_string(), 5),
        ("budget-exhausted".to_string(), 1),
    ];

    let json = r.to_json();
    assert!(!json.contains('\n'), "JSON lines must be one line: {json}");
    assert!(json.contains("\"spills\":7"), "{json}");
    assert!(json.contains("\"reloads\":4"), "{json}");
    assert!(json.contains("\"peak_live_values\":11"), "{json}");
    assert!(json.contains("\"inline_candidates\":9"), "{json}");
    assert!(json.contains("\"inlined_sites\":3"), "{json}");
    assert!(json.contains("\"speculative_inlined_sites\":1"), "{json}");
    assert!(json.contains("\"inlined_bytecodes\":214"), "{json}");
    // A u64 that does not fit an i32/u32, so a narrowing regression shows.
    assert!(json.contains("\"inlined_call_count\":4000000000"), "{json}");
    // The refusal histogram nests as an object keyed by category, in the
    // tally's first-seen order.
    assert!(
        json.contains("\"inline_refusals\":{\"guard-not-emittable\":5,\"budget-exhausted\":1}"),
        "{json}"
    );
}

#[test]
fn inline_and_allocation_fields_stay_distinguishable_from_zero() {
    let fresh = CompilationReport::new("inl/Fresh", "m", "()V", true);
    for (name, m) in [
        ("inline_candidates", fresh.inline_candidates),
        ("inlined_sites", fresh.inlined_sites),
        ("speculative_inlined_sites", fresh.speculative_inlined_sites),
        ("hierarchy_bound_sites", fresh.hierarchy_bound_sites),
        ("inlined_bytecodes", fresh.inlined_bytecodes),
        ("spills", fresh.spills),
        ("reloads", fresh.reloads),
        ("planned_spills", fresh.planned_spills),
        ("planned_reloads", fresh.planned_reloads),
    ] {
        assert_eq!(m, Measured::NotMeasured, "{name}");
        assert!(
            fresh.to_json().contains(&format!("\"{name}\":null")),
            "{name} must render null: {}",
            fresh.to_json()
        );
    }
    assert_eq!(fresh.inlined_call_count, Measured::NotMeasured);
    assert!(fresh.to_json().contains("\"inlined_call_count\":null"));
    // An empty histogram is an empty object, never a missing key — and it
    // is a different claim from "no tally was harvested", which the
    // `inline_candidates: null` above carries.
    assert!(fresh.inline_refusals.is_empty());
    assert!(fresh.to_json().contains("\"inline_refusals\":{}"));

    // A method the inliner looked at and declined everywhere reports
    // measured zeros, and the two encodings differ.
    let mut none_inlined = fresh.clone();
    none_inlined.inline_candidates = Measured::Value(4);
    none_inlined.inlined_sites = Measured::Value(0);
    none_inlined.inlined_bytecodes = Measured::Value(0);
    none_inlined.inlined_call_count = Measured::Value(0);
    none_inlined.spills = Measured::Value(0);
    none_inlined.reloads = Measured::Value(0);
    let json = none_inlined.to_json();
    assert!(json.contains("\"inlined_sites\":0"), "{json}");
    assert!(json.contains("\"inlined_call_count\":0"), "{json}");
    assert!(json.contains("\"spills\":0"), "{json}");
    assert!(json.contains("\"reloads\":0"), "{json}");
    assert_ne!(fresh.to_json(), json);
}

#[test]
fn ambient_spill_and_reload_hooks_reach_the_innermost_compilation() {
    let _guard = TEST_LOCK.lock();
    set_enabled_for_test(true);
    clear_reports();
    {
        let outer = CompileRecorder::begin("metrics/AllocOuter", "m", "()V", true);
        {
            let _inner = CompileRecorder::begin("metrics/AllocInner", "m", "()V", true);
            // `callee_compiler` nests compilations; an allocator run for the
            // callee must not be billed to the caller.
            note_current_spills(6);
            note_current_reloads(2);
        }
        note_current_spills(0);
        drop(outer);
    }
    let inner = last_compilation_report(Some("metrics/AllocInner")).expect("inner published");
    let outer = last_compilation_report(Some("metrics/AllocOuter")).expect("outer published");
    assert_eq!(inner.spills, Measured::Value(6));
    assert_eq!(inner.reloads, Measured::Value(2));
    // The outer compilation measured zero spills and never measured
    // reloads at all — two different facts, and the report keeps them apart.
    assert_eq!(outer.spills, Measured::Value(0));
    assert_eq!(outer.reloads, Measured::NotMeasured);
    let json = outer.to_json();
    assert!(json.contains("\"spills\":0"), "{json}");
    assert!(json.contains("\"reloads\":null"), "{json}");
    clear_reports();
    set_enabled_for_test(false);
}

/// The planned pair and the emitted pair are four independent fields.
///
/// R5 (`NOTES-misc.md` §2) is the decision this pins: an allocation's
/// `spills` is what it PLANNED and `plan_register_residency` emits only the
/// subset it can prove, so the two figures disagree and a report that
/// pointed both producers at one field would say "one of two numbers" and
/// leave the reader no way to tell which. Writing either pair must leave
/// the other alone, and the gap between them is the measurement.
#[test]
fn the_planned_allocation_figures_do_not_overwrite_the_emitted_ones() {
    let _guard = TEST_LOCK.lock();
    set_enabled_for_test(true);
    clear_reports();
    {
        let rec = CompileRecorder::begin("metrics/AllocPlan", "m", "()V", true);
        // The plan: nine spills, five reloads.
        rec.set_planned_spills(9);
        rec.set_planned_reloads(5);
        // What lowering actually emitted: a strict subset.
        note_current_spills(4);
        note_current_reloads(1);
        drop(rec);
    }
    let r = last_compilation_report(Some("metrics/AllocPlan")).expect("published");
    assert_eq!(r.planned_spills, Measured::Value(9));
    assert_eq!(r.planned_reloads, Measured::Value(5));
    assert_eq!(
        r.spills,
        Measured::Value(4),
        "the emitted pair is untouched"
    );
    assert_eq!(r.reloads, Measured::Value(1));
    let json = r.to_json();
    assert!(json.contains("\"planned_spills\":9"), "{json}");
    assert!(json.contains("\"planned_reloads\":5"), "{json}");
    assert!(json.contains("\"spills\":4"), "{json}");
    assert!(json.contains("\"reloads\":1"), "{json}");
    clear_reports();
    set_enabled_for_test(false);
}

/// A compile that emitted a measurement but had no recorder at allocation
/// time reports the planned pair as `null`, not as zero.
///
/// This is every compile today — the planned pair still has no production
/// writer — and "no plan was harvested" must stay distinct from "the
/// allocator planned nothing", for the same reason [`Measured`] exists at
/// all.
#[test]
fn an_unharvested_allocation_plan_is_null_and_not_zero() {
    let _guard = TEST_LOCK.lock();
    set_enabled_for_test(true);
    clear_reports();
    {
        let rec = CompileRecorder::begin("metrics/AllocNoPlan", "m", "()V", true);
        note_current_spills(0);
        drop(rec);
    }
    let r = last_compilation_report(Some("metrics/AllocNoPlan")).expect("published");
    assert_eq!(r.spills, Measured::Value(0));
    assert_eq!(r.planned_spills, Measured::NotMeasured);
    assert_eq!(r.planned_reloads, Measured::NotMeasured);
    let json = r.to_json();
    assert!(json.contains("\"spills\":0"), "{json}");
    assert!(json.contains("\"planned_spills\":null"), "{json}");
    assert!(json.contains("\"planned_reloads\":null"), "{json}");
    clear_reports();
    set_enabled_for_test(false);
}

#[test]
fn json_escape_covers_control_characters() {
    assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
    assert_eq!(json_escape("x\u{1}y"), "x\\u0001y");
    assert_eq!(json_escape("plain/name;()I"), "plain/name;()I");
}

// ── Scheduling counters ──────────────────────────────────────────

/// Every OSR event is reported, in a fixed order, including zeros.
///
/// The zeros are the point: "this never happened" is information, and a
/// summary whose row set changes with the run is one no sink can diff.
/// The exact edit that trips it: add a name to `OSR_EVENTS` without
/// widening `OSR_COUNTERS`, which is a compile error, or reorder them,
/// which this catches.
#[test]
fn osr_counts_report_every_event_in_a_fixed_order() {
    let names: Vec<&str> = osr_counts().into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, OSR_EVENTS.to_vec());
    assert_eq!(osr_counts().len(), OSR_COUNTERS.len());
}

/// Regression for the string the optimizing-tier OSR door actually sends
/// (`vm/src/runtime/interpreter/jit_bridge.rs`, `osr_optimizing_cached`
/// hit): `"osr_optimizing_artifact_reused"` must resolve to a real row,
/// not be silently dropped as an unknown name. It once was — the row did
/// not exist in [`OSR_EVENTS`], so every cache-hit reuse of an
/// optimizing-tier OSR artifact was counted nowhere and looked identical
/// to a door with no cache hits at all.
#[test]
fn osr_optimizing_artifact_reused_is_a_real_row() {
    let _guard = METRICS_TEST_LOCK.lock();
    reset_osr_counts();
    assert!(OSR_EVENTS.contains(&"osr_optimizing_artifact_reused"));
    record_osr_event("osr_optimizing_artifact_reused");
    record_osr_event("osr_optimizing_artifact_reused");
    let counts: Vec<(&str, u64)> = osr_counts();
    let (_, n) = counts
        .iter()
        .find(|(name, _)| *name == "osr_optimizing_artifact_reused")
        .expect("row present");
    assert_eq!(*n, 2);
    reset_osr_counts();
}

/// `record_osr_event` increments its own row and nothing else, and an
/// unknown name is ignored rather than panicking — it is called from the
/// interpreter's hot back-edge path.
#[test]
fn record_osr_event_increments_its_row_only() {
    let _guard = METRICS_TEST_LOCK.lock();
    reset_osr_counts();
    record_osr_event("osr_entered");
    record_osr_event("osr_entered");
    record_osr_event("osr_exited");
    record_osr_event("not_an_osr_event");
    let counts: Vec<(&str, u64)> = osr_counts();
    for (name, n) in &counts {
        let want = match *name {
            "osr_entered" => 2,
            "osr_exited" => 1,
            _ => 0,
        };
        assert_eq!(*n, want, "{name}");
    }
    // And it reaches the summary, which is the surface a run actually
    // shows — a counter nothing reports is a counter nobody reads.
    assert!(summary().osr.contains(&("osr_entered", 2)));
    reset_osr_counts();
}

#[test]
fn scheduling_counts_report_every_event_in_a_fixed_order() {
    let names: Vec<&str> = scheduling_counts().into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, SCHEDULING_EVENTS.to_vec());
    // The whole point of a fixed row set: an event that never fired still
    // has a row, so "zero drops" and "nobody counts drops" are different
    // readings.
    assert_eq!(names.len(), SCHEDULING_EVENTS.len());
}

#[test]
fn scheduling_events_are_distinct_names() {
    let mut sorted = SCHEDULING_EVENTS.to_vec();
    sorted.sort_unstable();
    let before = sorted.len();
    sorted.dedup();
    assert_eq!(sorted.len(), before, "two events share a name: {sorted:?}");
}

#[test]
fn recording_a_scheduling_event_is_visible_without_metrics_enabled() {
    let _guard = TEST_LOCK.lock();
    // Explicitly OFF. A dropped compilation request must be countable in a
    // default production run, where `CRATONVM_JIT_METRICS` is unset — this
    // is the property that distinguishes these counters from the ring.
    set_enabled_for_test(false);
    reset_scheduling_counts_for_test();

    record_scheduling_event(SCHEDULING_EVENTS[0]);
    record_scheduling_events(SCHEDULING_EVENTS[0], 4);
    record_scheduling_events(SCHEDULING_EVENTS[3], 2);
    // A zero count must not advance anything.
    record_scheduling_events(SCHEDULING_EVENTS[1], 0);
    // An unknown name is ignored rather than panicking.
    record_scheduling_event("not_an_event");

    assert_eq!(scheduling_count(SCHEDULING_EVENTS[0]), Some(5));
    assert_eq!(scheduling_count(SCHEDULING_EVENTS[1]), Some(0));
    assert_eq!(scheduling_count(SCHEDULING_EVENTS[3]), Some(2));
    assert_eq!(scheduling_count("not_an_event"), None);
    // `inflight_epoch_moved` describes a compile that RAN, so it is not a
    // drop and must stay out of the total.
    record_scheduling_events(SCHEDULING_EVENTS[2], 9);
    assert_eq!(scheduling_dropped_total(), 7);
    // A request dropped by a counted trap WAS queued and is thrown away,
    // exactly as the manager's `dropped_requests` mirror counts it.
    record_scheduling_events(SCHEDULING_EVENTS[5], 2);
    assert_eq!(scheduling_dropped_total(), 9);
    // Refusals (nothing queued) and a contained panic (the compile ran)
    // are not drops.
    record_scheduling_event(SCHEDULING_EVENTS[4]);
    record_scheduling_event(SCHEDULING_EVENTS[6]);
    record_scheduling_event(SCHEDULING_EVENTS[7]);
    assert_eq!(scheduling_dropped_total(), 9);

    reset_scheduling_counts_for_test();
}

/// The ineligible-refusal row must exist, and at the index `tiered.rs`
/// will record by.
///
/// `CompilerCore::admit` records `queue_deduplicated` by INDEX
/// (`SCHEDULING_EVENTS[4]`), so the index of its sibling arm is part of the
/// contract and not an implementation detail. Pinning the name at index 7
/// here is what makes the one-line companion edit in `tiered.rs` safe to
/// write against a constant instead of against a literal.
#[test]
fn the_ineligible_refusal_row_exists_at_the_index_the_scheduler_records_by() {
    let _guard = TEST_LOCK.lock();
    set_enabled_for_test(false);
    reset_scheduling_counts_for_test();

    assert_eq!(SCHEDULING_EVENTS[7], "queue_refused_ineligible");
    // A row that exists but cannot be recorded into is the bug this test
    // exists to prevent: the name must resolve through the same lookup
    // `record_scheduling_event` uses.
    assert_eq!(scheduling_count(SCHEDULING_EVENTS[7]), Some(0));
    record_scheduling_events(SCHEDULING_EVENTS[7], 2);
    assert_eq!(scheduling_count(SCHEDULING_EVENTS[7]), Some(2));
    // It is a REFUSAL, not a drop: nothing was ever queued, so it must not
    // move the total that counts thrown-away queued work — exactly the rule
    // `queue_deduplicated` (index 4) already follows.
    assert_eq!(scheduling_dropped_total(), 0);

    reset_scheduling_counts_for_test();
}

/// The deopt stash's two residual counters must reach a dump.
///
/// Both are expected to read zero forever; that claim is only worth
/// something if a run can print them, which is the whole point of the row.
/// The test asserts the plumbing (names, order, JSON key) rather than the
/// values, because the values are process-wide and another test in this
/// binary may legitimately have pushed a nested stash.
#[test]
fn the_summary_prints_the_deopt_stash_residual_counters() {
    let _guard = TEST_LOCK.lock();
    set_enabled_for_test(false);

    let rows = deopt_stash_counts();
    let names: Vec<&str> = rows.iter().map(|(n, _)| *n).collect();
    assert_eq!(names, DEOPT_STASH_EVENTS.to_vec());
    // Names and order, not values: `crate::deopt`'s counters are
    // process-wide and `deopt.rs`'s own tests push nested stashes, so a
    // value comparison across two reads is a race this lock does not cover.
    let summary_names: Vec<&str> = summary().deopt_stash.iter().map(|(n, _)| *n).collect();
    assert_eq!(summary_names, DEOPT_STASH_EVENTS.to_vec());

    let json = summary().to_json();
    assert!(
        json.contains("\"deopt_stash\":{\"deopt_stash_nested\":"),
        "{json}"
    );
    assert!(
        json.contains("\"deopt_stash_dropped_unclaimed\":"),
        "{json}"
    );
}

#[test]
fn summary_carries_the_scheduling_table_and_json() {
    let _guard = TEST_LOCK.lock();
    set_enabled_for_test(false);
    reset_scheduling_counts_for_test();
    record_scheduling_events(SCHEDULING_EVENTS[0], 3);

    let s = summary();
    let names: Vec<&str> = s.scheduling.iter().map(|(n, _)| *n).collect();
    assert_eq!(names, SCHEDULING_EVENTS.to_vec());
    assert_eq!(
        s.scheduling
            .iter()
            .find(|(n, _)| *n == SCHEDULING_EVENTS[0]),
        Some(&(SCHEDULING_EVENTS[0], 3))
    );
    let json = s.to_json();
    assert!(
        json.contains("\"scheduling\":{\"queue_dropped_stale_install_epoch\":3"),
        "{json}"
    );

    reset_scheduling_counts_for_test();
}

#[test]
fn measured_helpers_behave() {
    let m: Measured<u32> = Measured::of(5);
    assert_eq!(m.get(), Some(5));
    assert_eq!(m.or(9), 5);
    assert_eq!(m.as_option(), Some(&5));
    assert_eq!(m.json(), "5");
    let n: Measured<u32> = Measured::NotMeasured;
    assert_eq!(n.get(), None);
    assert_eq!(n.or(9), 9);
    assert_eq!(n.as_option(), None);
    assert_eq!(n.json(), "null");
    assert_eq!(Measured::<u32>::default(), Measured::NotMeasured);
}
