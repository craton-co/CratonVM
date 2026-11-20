use super::*;
use crate::gc_quiescence::incomplete_reason;

#[test]
fn normalization_is_a_pure_function_of_the_raw_counters() {
    let raw = GcMetricsRaw {
        card_marks_executed: 400,
        barrier_ref_stores: 1_000,
        cards_found_dirty: 50,
        // The legacy total is the sum of the two halves below; `from_raw`
        // is pure, so this literal states the relationship that
        // `gc_metrics_raw` maintains rather than deriving it.
        duplicate_card_marks: 150,
        duplicate_card_marks_buffered: 50,
        duplicate_card_marks_barrier: 100,
        remembered_set_bytes: 2_048,
        old_to_young_edges: 200,
        // The CSet-verify counters are pure observability: they take no
        // part in any normalization below, so this literal pins them at
        // zero to say so rather than to exercise them.
        cset_verify_objects: 0,
        cset_verify_pauses: 0,
        cset_verify_dangling: 0,
        cset_verify_truncated: 0,
        cset_verify_bytes: 0,
        cset_verify_unbounded: 0,
        rset_coarsened: 0,
        humongous_eager_marked: 0,
        humongous_eager_root_seeded: 0,
        humongous_eager_walked_sources: 0,
        humongous_eager_spans: 0,
        humongous_eager_bytes: 0,
        humongous_eager_declined: 0,
        refinement_nanos: 4_000_000,
        refinement_passes: 4,
        allocated_objects: 1_000,
        allocated_bytes: 64_000,
        live_bytes: 8_192,
        // Rendered, not a denominator (gen r5w2/obs6).
        allocated_bytes_total: 0,
        gc_triggered_by_vram_pressure: 0,
    };
    let r = GcMetricsReport::from_raw(raw, true);

    assert_eq!(r.card_marks_per_allocated_object, 0.4);
    assert_eq!(r.dirty_cards_per_allocated_object, 0.05);
    assert_eq!(r.old_to_young_edges_per_allocated_object, 0.2);
    assert_eq!(r.remembered_set_bytes_per_live_byte, 0.25);
    assert_eq!(r.refinement_nanos_per_live_byte, 4_000_000.0 / 8_192.0);
    assert_eq!(r.old_to_young_edges_per_live_byte, 200.0 / 8_192.0);
    // 200 edges across 50 dirty cards.
    assert_eq!(r.old_to_young_edge_density, 4.0);
    // The frozen legacy figure: 150 / (150 + 50). Kept bit-for-bit across
    // the 2026-09-21 numerator split precisely so no embedder's number
    // changes meaning under them; it is superseded, not redefined.
    assert_eq!(r.duplicate_mark_ratio, 0.75);
    // The honest one: 100 barrier duplicates out of 400 barrier marks.
    // Same barrier, same gate, so this one is a fraction.
    assert_eq!(r.barrier_duplicate_mark_ratio, 0.25);
    assert_eq!(r.barrier_hit_rate, 0.4);
    assert_eq!(r.refinement_nanos_per_pass, 1_000_000.0);
}

/// A report taken before anything has been collected must read as "no cost
/// observed", never `NaN` — a `NaN` in a summary line is indistinguishable
/// from a parse bug for whoever reads the log.
#[test]
fn every_ratio_is_zero_when_its_denominator_is_zero() {
    let r = GcMetricsReport::from_raw(GcMetricsRaw::default(), false);
    for v in [
        r.card_marks_per_allocated_object,
        r.dirty_cards_per_allocated_object,
        r.old_to_young_edges_per_allocated_object,
        r.card_marks_per_alloc_mib,
        r.dirty_cards_per_alloc_mib,
        r.old_to_young_edges_per_alloc_mib,
        r.remembered_set_bytes_per_live_byte,
        r.refinement_nanos_per_live_byte,
        r.old_to_young_edges_per_live_byte,
        r.old_to_young_edge_density,
        r.duplicate_mark_ratio,
        r.barrier_duplicate_mark_ratio,
        r.barrier_hit_rate,
        r.refinement_nanos_per_pass,
    ] {
        assert_eq!(v, 0.0, "a zero denominator must normalize to 0.0, not NaN");
        assert!(v.is_finite());
    }
}

/// gen r5w5/sizer9 — step 3 of the denominators page: the per-MiB ratios
/// divide by the VM's cumulative allocation, and only by it (the legacy
/// object and byte gauges take no part).
#[test]
fn the_alloc_ratios_are_per_mib_of_the_cumulative_total() {
    let raw = GcMetricsRaw {
        card_marks_executed: 400,
        cards_found_dirty: 50,
        old_to_young_edges: 200,
        allocated_objects: 3,
        allocated_bytes: 17,
        allocated_bytes_total: 4 << 20,
        ..GcMetricsRaw::default()
    };
    let r = GcMetricsReport::from_raw(raw, true);
    assert_eq!(r.card_marks_per_alloc_mib, 100.0);
    assert_eq!(r.dirty_cards_per_alloc_mib, 12.5);
    assert_eq!(r.old_to_young_edges_per_alloc_mib, 50.0);
    let text = r.to_string();
    assert!(
        text.contains(concat!(
            "[GC] cards/alloc: dirty_cards_per_alloc_mib=12.500000 ",
            "edges_per_alloc_mib=50.000000 card_marks_per_alloc_mib=100.000000 ",
            "(allocated_bytes_total=4194304 slow_path_objects=3)"
        )),
        "{text}"
    );
}

#[test]
fn collector_side_counters_accumulate_without_the_hot_path_gate() {
    reset_metrics_for_test();
    set_hot_path_counters_enabled(false);

    record_cards_found_dirty(7);
    record_duplicate_card_marks(3);
    record_old_to_young_edges(21);
    record_refinement(1_500);
    record_refinement(2_500);
    record_remembered_set_bytes(4_096);
    record_remembered_set_bytes(8_192); // gauge: last value wins
    record_heap_occupancy(100, 3_200, 1_024);

    let raw = gc_metrics_raw();
    assert_eq!(raw.cards_found_dirty, 7);
    assert_eq!(raw.duplicate_card_marks_buffered, 3);
    assert_eq!(
        raw.duplicate_card_marks, 3,
        "the legacy total reads the same as before the split"
    );
    assert_eq!(raw.old_to_young_edges, 21);
    assert_eq!(raw.refinement_nanos, 4_000);
    assert_eq!(raw.refinement_passes, 2);
    assert_eq!(raw.remembered_set_bytes, 8_192, "rset bytes is a gauge");
    assert_eq!(raw.allocated_objects, 100);
    assert_eq!(raw.live_bytes, 1_024);

    // The gated counters stayed untouched, and the report says so rather
    // than reporting a measured zero.
    assert_eq!(raw.card_marks_executed, 0);
    let report = gc_metrics_report();
    assert!(!report.hot_path_counters_enabled);
    assert_eq!(report.old_to_young_edge_density, 3.0);
    assert!(report.to_string().contains("NOT ARMED"));
}

/// `reset_metrics_for_test` must clear EVERY counter, not the eleven of
/// twenty-two it used to.
///
/// A partial reset is the worst shape a reset can have: the caller reads it
/// as a clean slate, so the fields it silently skipped turn every later
/// assertion on them into an assertion about which other test in this
/// binary ran first. Written as "set everything, reset, expect
/// `GcMetricsRaw::default()`" so a counter added to `Counters` without a
/// matching line in the reset fails here rather than somewhere unrelated.
#[test]
fn reset_clears_every_counter_not_just_the_card_ones() {
    reset_metrics_for_test();
    set_hot_path_counters_enabled(true);

    record_barrier_ref_store();
    record_card_mark();
    record_cards_found_dirty(3);
    record_duplicate_card_marks(4);
    // gengc-round3: the barrier half is a SEPARATE counter and needs its
    // own line here. This is the field the gap page warned keeps drifting.
    record_duplicate_card_mark_barrier();
    record_old_to_young_edges(5);
    record_remembered_set_bytes(6);
    record_refinement(7);
    record_heap_occupancy(8, 9, 10);
    // The 2026-09-20 G1 round widened this to carry BYTES and the
    // unbounded-pass flag, so the fixture has to move those two as
    // well: a counter the fixture never touches cannot show that
    // `reset` clears it.
    record_g1_cset_verify(11, 1112, 12, true, true);
    record_g1_rset_coarsened();
    record_g1_humongous_liveness(13, 14, 15);
    record_g1_eager_humongous(16, 17, true);
    // gen r5w2/obs6.
    record_vm_allocation_totals(18);
    record_vram_pressure_gc();

    let dirty = gc_metrics_raw();
    assert_ne!(
        dirty,
        GcMetricsRaw::default(),
        "the fixture must actually move the counters"
    );
    // Each of the eleven the old reset skipped, plus the two halves of the
    // 2026-09-21 duplicate-mark split, individually — so a regression names
    // the field rather than just the aggregate. (These two are here because
    // the fixture must be shown to MOVE them: a counter the fixture never
    // touches would pass the `== default()` check below while having no
    // reset line at all.)
    for (name, v) in [
        (
            "duplicate_card_marks_buffered",
            dirty.duplicate_card_marks_buffered,
        ),
        (
            "duplicate_card_marks_barrier",
            dirty.duplicate_card_marks_barrier,
        ),
        ("cset_verify_objects", dirty.cset_verify_objects),
        ("cset_verify_pauses", dirty.cset_verify_pauses),
        ("cset_verify_dangling", dirty.cset_verify_dangling),
        ("cset_verify_truncated", dirty.cset_verify_truncated),
        ("rset_coarsened", dirty.rset_coarsened),
        ("humongous_eager_marked", dirty.humongous_eager_marked),
        (
            "humongous_eager_root_seeded",
            dirty.humongous_eager_root_seeded,
        ),
        (
            "humongous_eager_walked_sources",
            dirty.humongous_eager_walked_sources,
        ),
        ("humongous_eager_spans", dirty.humongous_eager_spans),
        ("humongous_eager_bytes", dirty.humongous_eager_bytes),
        ("humongous_eager_declined", dirty.humongous_eager_declined),
        ("allocated_bytes_total", dirty.allocated_bytes_total),
        (
            "gc_triggered_by_vram_pressure",
            dirty.gc_triggered_by_vram_pressure,
        ),
    ] {
        assert_ne!(v, 0, "fixture did not move {name}");
    }

    reset_metrics_for_test();
    assert_eq!(
        gc_metrics_raw(),
        GcMetricsRaw::default(),
        "reset_metrics_for_test must clear EVERY field of Counters; a field \
         it skips makes a later assertion on that field an assertion about \
         test ordering",
    );

    set_hot_path_counters_enabled(false);
}

/// Every counter something increments must appear in the rendered report.
///
/// `rset_coarsened` and `allocated_bytes` were both written by production
/// code (`region.rs`'s coarsening path and `record_heap_occupancy`) and
/// read by nothing: no ratio, no rendered field, and no in-tree caller of
/// `gc_metrics_raw()` outside this module. A counter in that state is
/// indistinguishable from a counter that is never published, which is the
/// exact failure this module's header says it exists to prevent.
#[test]
fn the_report_renders_the_counters_nothing_else_reads() {
    reset_metrics_for_test();
    // Pinned explicitly: the assertions below distinguish the armed
    // barrier line from the NOT-ARMED one, and under `--test-threads=1`
    // libtest runs cases on one thread, where this thread-local gate
    // would otherwise carry whatever the previous test left.
    set_hot_path_counters_enabled(false);
    record_heap_occupancy(7, 4_242, 99);
    record_g1_rset_coarsened();
    record_g1_rset_coarsened();
    // gen r5w2/obs6: the cumulative total is monotone — a smaller later
    // publication does not lower it.
    record_vm_allocation_totals(123_456);
    record_vm_allocation_totals(1_000);
    record_vram_pressure_gc();

    let text = gc_metrics_report().to_string();
    assert!(
        text.contains("vram_pressure_gcs=1"),
        "vram_pressure_gcs is recorded on GPU pressure collections and must render: {text}",
    );
    assert!(
        text.contains("allocated_bytes_total=123456"),
        "allocated_bytes_total is published by the VM and must be rendered, \
         monotone: {text}",
    );
    // gen r5w5/sizer9: `allocated_bytes` is no longer rendered (no
    // production publisher writes it since the Generational heap stopped
    // publishing its occupancy under that name — step 2 of the
    // denominators page); the object gauge is rendered under the name of
    // what it counts.
    assert!(
        !text.contains("allocated_bytes="),
        "the occupancy-as-allocation gauge must not be rendered: {text}",
    );
    assert!(
        text.contains("slow_path_objects=7"),
        "the object gauge is rendered as what it counts: {text}",
    );
    // Step 3: the per-object keys are gone and the per-MiB ones divide
    // by the cumulative total — 0 marks over 123456 bytes here.
    for key in [
        "dirty_cards_per_alloc_mib=",
        "edges_per_alloc_mib=",
        "card_marks_per_alloc_mib=",
    ] {
        assert!(text.contains(key), "{key} missing: {text}");
    }
    for old in [
        "dirty_cards_per_obj=",
        "edges_per_obj=",
        "card_marks_per_obj=",
    ] {
        assert!(!text.contains(old), "{old} must be gone: {text}");
    }
    assert!(
        text.contains("rset_coarsened=2"),
        "rset_coarsened is bumped by region.rs and must be rendered: {text}",
    );
    // And the renamed duplicate key: the old `duplicate_ratio=` named a
    // quantity this number is not (see `duplicate_mark_ratio`).
    assert!(
        text.contains("dup_over_dup_plus_scanned="),
        "the duplicate figure must be keyed by its arithmetic, not by the \
         buffered-path fraction it is not: {text}",
    );
    assert!(
        !text.contains("duplicate_ratio="),
        "the misleading key must be gone: {text}",
    );
    // gengc-round3: the buffered half is rendered unconditionally (it is
    // ungated), and it is what tells a reader whether the dead buffered
    // pipeline has a producer again.
    assert!(
        text.contains("dup_buffered=0"),
        "the buffered duplicate half must be rendered: {text}",
    );
    // The barrier half and its ratio are rendered ONLY on the armed line,
    // because both terms are gated. Unarmed, the report must say so rather
    // than print a measured-looking zero.
    assert!(
        !text.contains("dup_barrier="),
        "the gated barrier half must not be rendered while the gate is \
         off — that is the 'unmeasured is not zero' rule: {text}",
    );
    assert!(
        text.contains("dup_barrier are unmeasured, not zero"),
        "and the unarmed line must name it: {text}",
    );

    reset_metrics_for_test();
}

/// gengc-round3 — the numerator and its denominator must be armed
/// together, and the legacy total must keep its old value.
///
/// `duplicate_card_marks` used to be one counter fed by two sites on two
/// clocks: `drain_pending` (ungated, per drain) and the write barrier's
/// already-dirty arm (gated, per store). No denominator divides that sum,
/// which is why `duplicate_mark_ratio` could not be repaired by changing
/// its divisor — see
/// `docs/feature-designs/jit-barrier-lowering.md`.
///
/// Three things are asserted here, in the order they would break:
///
/// 1. the barrier counter disarms with `card_marks_executed`, so the ratio
///    never has one live term and one dead one;
/// 2. the buffered counter does **not** disarm, because it is per-drain;
/// 3. `duplicate_card_marks` still reads as the sum, so an embedder that
///    was reading it sees no change.
#[test]
fn the_duplicate_numerator_and_its_denominator_arm_together() {
    reset_metrics_for_test();

    // Disarmed: the barrier half and its denominator both stay put; the
    // buffered half still accumulates.
    set_hot_path_counters_enabled(false);
    record_card_mark();
    record_duplicate_card_mark_barrier();
    record_duplicate_card_marks(2);
    let raw = gc_metrics_raw();
    assert_eq!(raw.card_marks_executed, 0, "denominator is gated");
    assert_eq!(raw.duplicate_card_marks_barrier, 0, "numerator is gated");
    assert_eq!(
        raw.duplicate_card_marks_buffered, 2,
        "the buffered half is per-drain and must NOT be gated",
    );

    // Armed: four barrier marks, one of which was a duplicate.
    set_hot_path_counters_enabled(true);
    for _ in 0..4 {
        record_card_mark();
    }
    record_duplicate_card_mark_barrier();

    let raw = gc_metrics_raw();
    assert_eq!(raw.card_marks_executed, 4);
    assert_eq!(raw.duplicate_card_marks_barrier, 1);
    assert!(
        raw.duplicate_card_marks_barrier <= raw.card_marks_executed,
        "holds by construction: `write_barrier` records a card mark on \
         both arms of the branch whose else-arm records the duplicate",
    );
    assert_eq!(
        raw.duplicate_card_marks, 3,
        "the legacy field is the sum of the two halves and nothing else",
    );

    let report = GcMetricsReport::from_raw(raw, true);
    assert_eq!(report.barrier_duplicate_mark_ratio, 0.25);
    let text = report.to_string();
    assert!(
        text.contains("dup_barrier=1") && text.contains("dup_barrier_over_card_marks=0.2500"),
        "the armed line renders the pair and spells its arithmetic: {text}",
    );
    assert!(
        text.contains("duplicate_marks=3") && text.contains("dup_buffered=2"),
        "the first line renders the total and the buffered half: {text}",
    );

    set_hot_path_counters_enabled(false);
    reset_metrics_for_test();
}

/// A COMPLETE G1 pause must not attribute itself to
/// `incomplete_reason::NONE`.
///
/// `NONE`'s label is `"none"`, so an `incomplete.then_some(NONE)` would
/// render `incomplete=N ... reasons: none=N` — "the root set was
/// incomplete, for no reason".
///
/// This used to drive the boolean-only recorder
/// (`record_g1_pause_coverage`), which round 10 deleted for having no caller
/// anywhere in the tree. The property is the reason form's now, and the
/// reason form is the one that ships.
///
/// # Why the totals are `>=` and the reason row is `==`
///
/// The asymmetry is deliberate and is what keeps this test off the flaky
/// list. `G1_PAUSES` / `G1_PAUSES_COVERAGE_INCOMPLETE` are process-global
/// statics that `g1.rs` ALSO feeds from every real collection, and this
/// binary runs its tests in parallel — so a concurrent pause can raise
/// either total between the two reads. They are monotone
/// (`reset_metrics_for_test` does not touch them), so `>=` is the strongest
/// claim that is true, and it still fails if this call stops counting.
///
/// Row 0 is different: the ONLY way a concurrent collection can raise it is
/// by passing `Some(incomplete_reason::NONE)`, which is precisely the defect
/// this test exists to catch. So `==` there is not a race — it is a second
/// chance to catch the same bug, from another thread.
#[test]
fn a_complete_pause_attributes_no_reason() {
    let none_before = g1_coverage_reason_counts()
        .into_iter()
        .find(|(label, _)| *label == crate::gc_quiescence::incomplete_reason::label(0))
        .map(|(_, n)| n)
        .unwrap_or(0);
    let (pauses_before, incomplete_before) = g1_pause_coverage_counts();

    record_g1_pause_coverage_reason(None);
    record_g1_pause_coverage_reason(Some(1));

    let (pauses_after, incomplete_after) = g1_pause_coverage_counts();
    assert!(
        pauses_after >= pauses_before + 2,
        "both pauses are counted: before={pauses_before} after={pauses_after}"
    );
    assert!(
        incomplete_after >= incomplete_before + 1,
        "and at least one of them as incomplete: \
         before={incomplete_before} after={incomplete_after}"
    );

    let none_after = g1_coverage_reason_counts()
        .into_iter()
        .find(|(label, _)| *label == crate::gc_quiescence::incomplete_reason::label(0))
        .map(|(_, n)| n)
        .unwrap_or(0);
    assert_eq!(
        none_after, none_before,
        "a complete pause must leave the per-reason census untouched — a \
         `none=N` row reads as a reason, and `none` is not one",
    );
}

#[test]
fn barrier_counters_record_only_while_armed() {
    reset_metrics_for_test();

    set_hot_path_counters_enabled(false);
    for _ in 0..10 {
        record_barrier_ref_store();
        record_card_mark();
    }
    assert_eq!(gc_metrics_raw().card_marks_executed, 0);
    assert_eq!(gc_metrics_raw().barrier_ref_stores, 0);

    set_hot_path_counters_enabled(true);
    for _ in 0..10 {
        record_barrier_ref_store();
    }
    for _ in 0..4 {
        record_card_mark();
    }
    let raw = gc_metrics_raw();
    assert_eq!(raw.barrier_ref_stores, 10);
    assert_eq!(raw.card_marks_executed, 4);
    assert_eq!(
        GcMetricsReport::from_raw(raw, true).barrier_hit_rate,
        0.4,
        "4 of 10 reference stores crossed a generation"
    );

    set_hot_path_counters_enabled(false);
}

#[test]
fn every_decision_reason_has_a_label_and_a_moving_verdict() {
    for code in decision_reason::UNRECORDED..decision_reason::COUNT {
        assert_ne!(
            decision_reason::label(code),
            "unknown",
            "decision reason {code} needs a label — the report prints it verbatim",
        );
        // `is_moving` must be total: every defined code answers.
        let moving = decision_reason::is_moving(code);
        assert_eq!(
            moving,
            decision_reason::label(code).starts_with("moving-"),
            "the label prefix and `is_moving` must agree for code {code}",
        );
        // 2026-09-21: and the same for the third arm. A `skipped-*` code
        // that answered `is_moving` would be counted as a relocating
        // cycle; one that answered neither predicate would be silently
        // folded into `non_moving`, which is the claim that the sweep ran.
        let skipped = decision_reason::is_skipped(code);
        assert_eq!(
            skipped,
            decision_reason::label(code).starts_with("skipped-"),
            "the label prefix and `is_skipped` must agree for code {code}",
        );
        assert!(
            !(skipped && moving),
            "a refused cycle relocated nothing; code {code} cannot be both",
        );
    }
    assert_eq!(
        decision_reason::label(decision_reason::COUNT),
        "unknown",
        "COUNT must be one PAST the last defined reason",
    );
}




/// The histogram totals every recorded cycle on BOTH arms.
///
/// This is now load-bearing outside this module: `VmHeap::print_gc_summary`
/// takes `moving_cycles_total` / `non_moving_cycles_total` from here for the
/// `[GC] moving_young:` line, because the counter that line used to print
/// (`gc_quiescence::moving_young_cycle_count`) is bumped only inside
/// `gen_heap`'s `if moving_young && has_conservative_roots` — i.e. only for
/// moving cycles taken while a JIT frame was live. A run with no compiled
/// frames that relocated on every collection reported zero.
///
/// Fresh test thread: the histogram is thread-local under `cfg(test)`.
#[test]
fn the_decision_histogram_totals_every_recorded_cycle() {
    let (m0, n0, s0, _) = decision_histogram();
    assert_eq!((m0, n0, s0), (0, 0, 0), "fresh test thread");

    // Three moving cycles, none of them under a live JIT frame — the shape
    // `moving_young_cycle_count()` reports as zero.
    for _ in 0..3 {
        record_collector_decision(
            "generational",
            decision_reason::MOVING_NO_JIT_FRAMES,
            incomplete_reason::NONE,
        );
    }
    record_collector_decision(
        "generational",
        decision_reason::NON_MOVING_PROMOTION_OOM_RISK,
        incomplete_reason::NONE,
    );

    // And one REFUSED cycle, which is neither.
    record_collector_decision(
        "generational",
        decision_reason::SKIPPED_YOUNG_RESERVED_TLAB_TAILS,
        incomplete_reason::NONE,
    );

    let (moving, non_moving, skipped, rows) = decision_histogram();
    assert_eq!(
        (moving, non_moving, skipped),
        (3, 1, 1),
        "every recorded decision lands on exactly one of the THREE arms — a refusal counted as non-moving would claim the non-moving sweep ran, and nothing ran",
    );
    assert_eq!(
        crate::gc_quiescence::moving_young_cycle_count(),
        0,
        "…and this is the counter that cannot see them: it only counts \
         moving cycles taken under a live JIT frame",
    );
    assert!(rows
        .iter()
        .any(|(l, n)| *l == "moving-no-jit-frames-live" && *n == 3));
    assert!(rows
        .iter()
        .any(|(l, n)| *l == "nonmoving-promotion-oom-risk" && *n == 1));
    assert!(rows
        .iter()
        .any(|(l, n)| *l == "skipped-young-reserved-tlab-tails" && *n == 1));
}


#[test]
fn decision_report_distinguishes_a_proven_moving_cycle() {
    record_collector_decision(
        "generational",
        decision_reason::MOVING_WITH_PROVEN_JIT_COVERAGE,
        incomplete_reason::NONE,
    );
    let d = last_collector_decision().unwrap();
    assert!(d.young_moving);
    let text = d.to_string();
    assert!(text.contains("MOVING"), "{text}");
    assert!(
        !text.contains("unproven_obligation"),
        "a proven cycle must not print a fallback obligation: {text}",
    );
}

// -----------------------------------------------------------------------
// G1 per-cycle facts
// -----------------------------------------------------------------------

#[test]
fn every_g1_degraded_flag_has_a_label() {
    // Walk every bit in ALL and require a distinct, non-empty label. This
    // is what stops a new fail-safe from being added to the collector and
    // then printing as an opaque bitmask in the summary.
    let mut seen: Vec<&str> = Vec::new();
    for bit in 0..32u32 {
        let mask = 1u32 << bit;
        if g1_degraded::ALL & mask == 0 {
            continue;
        }
        let labels = g1_degraded::labels(mask);
        assert_eq!(labels.len(), 1, "bit {bit} must map to exactly one label");
        assert!(!labels[0].is_empty(), "bit {bit} has an empty label");
        assert!(
            !seen.contains(&labels[0]),
            "duplicate label {:?} for bit {bit}",
            labels[0],
        );
        seen.push(labels[0]);
    }
    assert_eq!(
        g1_degraded::labels(g1_degraded::NONE).len(),
        0,
        "an undegraded cycle names no fail-safe",
    );
    // A bit outside ALL contributes nothing rather than an "unknown" entry.
    assert!(g1_degraded::labels(1 << 31).is_empty());
}


#[test]
fn g1_cycle_report_names_every_degraded_mode_that_fired() {
    record_g1_cycle(
        g1_cycle_kind::MIXED,
        6,
        2,
        3,
        11,
        g1_degraded::EVACUATION_FAILURE
            | g1_degraded::JIT_PINNED_REGIONS_EXCLUDED
            | g1_degraded::MARK_WORKLIST_OVERFLOW,
    );
    let f = last_g1_cycle().expect("just recorded");
    assert_eq!(f.sequence, 1);
    assert_eq!(f.kind, g1_cycle_kind::MIXED);
    assert_eq!((f.cset_young, f.cset_old), (6, 2));
    assert_eq!(f.regions_pinned_out, 3);
    assert_eq!(f.rset_sources_scanned, 11);

    let text = f.to_string();
    assert!(text.contains("kind=mixed"), "{text}");
    assert!(text.contains("cset_young=6 cset_old=2"), "{text}");
    assert!(text.contains("pinned_out=3"), "{text}");
    assert!(text.contains("evacuation-failure-self-forwarded"), "{text}");
    assert!(text.contains("jit-pinned-regions-excluded"), "{text}");
    assert!(text.contains("mark-worklist-overflow-rescan"), "{text}");
}

#[test]
fn an_undegraded_g1_cycle_says_so_rather_than_printing_nothing() {
    // "degraded=" with an empty tail would be indistinguishable from a
    // truncated log line; a clean cycle must say `none` explicitly.
    record_g1_cycle(g1_cycle_kind::YOUNG, 4, 0, 0, 2, g1_degraded::NONE);
    let text = last_g1_cycle().unwrap().to_string();
    assert!(text.contains("degraded=none"), "{text}");
}

