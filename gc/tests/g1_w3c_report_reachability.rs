// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane W3-C — instrumentation that does not print is not instrumentation.
//!
//! # The failure this file guards
//!
//! Three separate defects in this round have the same shape, and all three
//! were found by a person rather than by a test:
//!
//! * **P7** (`lane-e-small-findings-not-fixed.md`) — `print_gc_summary` was
//!   reached only from `vm-cli`'s normal-return teardown, and the workloads
//!   its numbers were wanted for end in `System.exit`, which never unwinds
//!   Rust frames. Every line in it was structurally unreachable on those runs.
//! * **`drain_fixup_totals`** — a CORRECT counter added in wave 2 to close a
//!   real gap (each drain pass overwrote the previous pass's `phases.fixup_*`)
//!   whose only callers were `gc/tests/*.rs`. There was no supported way to
//!   read it on a shipped binary.
//! * **`work_units_done`** and the `[GC-STAT]` `try_read` census — counters
//!   that printed a number which was not the number their name claimed, and an
//!   instrument armed where it could not fire.
//!
//! A subagent sweep over the GC crate for this wave found **34** `pub fn`
//! accessors whose only callers are tests, doc comments, or a `Debug` impl
//! nothing formats. This file does not try to pin all 34 — most are genuinely
//! test-only helpers. It pins the ones this lane WIRED UP, so that deleting a
//! census line is a test failure rather than a silent return to the state P7
//! describes.
//!
//! **LANE W6-A (wave 6)** closed that sweep — every remaining accessor now has
//! a disposition rather than a status, and six more were wired. Two of those
//! landed outside `collector_decision_report`, so this file grew a second
//! section for them at the bottom: `[GC] g1 card-table:` needs `&self` and
//! `[GC] tlab-waste:` belongs to `tlab.rs`. Both are FORMATTERS returning a
//! `String` for exactly the reason this file exists — a line nothing can
//! assert on is a line that can be deleted by accident. See
//! `docs/internal/g1-2026-09-20/w6a-twenty-nine-accessors.md`.
//!
//! # Why `collector_decision_report` and not `print_gc_summary`
//!
//! Both are now reachable on both shutdown arms: `vm-cli`'s
//! `maybe_dump_shutdown_reports` reaches `print_gc_summary` through a
//! `Weak<SharedVm>` stashed at VM construction, and that was verified against
//! a release binary (`w3c-print-gc-summary-and-the-system-exit-arm.md`). The
//! split between them is now about the DATA, not about reachability: a process
//! static goes in `collector_decision_report`, which needs no live collector;
//! anything needing `&self` goes in `print_gc_summary`, which can only run
//! while one exists. `collector_decision_report` returns a `String`, so it is
//! the half a test can assert on without capturing stderr.

use cratonvm_gc::g1::G1CollectorConfig;
use cratonvm_gc::gc_metrics::collector_decision_report;
use cratonvm_gc::G1Collector;

/// Every label this lane put on the both-arms report, with what each one is
/// for. A deletion here should be a deliberate act with this list updated, not
/// a side effect of tidying a logging function — which is how P7 happened.
const REQUIRED_LINES: &[(&str, &str)] = &[
    (
        "[GC] g1 mark-pauses:",
        "remark and cleanup are stop-the-world pauses that go through neither \
         `record_collection_with_phases` nor the pause percentiles; before this \
         line no duration for either existed anywhere in the process",
    ),
    (
        "[GC] g1 satb-walk:",
        "the O(threads) registry walk inside those two pauses -- the census \
         `w2c-the-satb-registry-walk-is-still-o-threads-per-cycle.md` named as \
         the thing that would decide between its three candidate fixes",
    ),
    (
        "[GC] g1 drain-fixup:",
        "the evacuation-failure drain's REPEATED whole-heap fix-up; wave 2 \
         added the accumulators and wired no reader to them",
    ),
    (
        "[GC] g1 mark-residue:",
        "`region_type_decode_failsafe` (documented must-be-zero, a counter \
         rather than a `debug_assert!` precisely so it could be read) and the \
         Phase 3.5 resurrect census (its own denominator)",
    ),
    (
        "[GC] g1 mark-loader-pins:",
        "whether the three class-loader registries were populated at all, \
         which is what falsifies any A/B over CRATONVM_G1_MARK_SIDE_TABLES \
         before its wall clock is read",
    ),
    (
        // LANE W6-A.
        "[GC] deadref-recv:",
        "the POPULATION of the receiver-side dead-reference screen. \
         `note_deadref_recv` eprintln's its first twenty-four occurrences and \
         then nothing, and its own write site says the counter exists so that \
         `the number is the population and not the printout` -- after which \
         the counter had one test caller and no other reader, so a run with 24 \
         reports and a run with 24 000 were indistinguishable. Expected ZERO: \
         a non-zero is a field access through a receiver that named no live \
         object",
    ),
    (
        // LANE W7-C.
        "[GC] g1 evac-copy-span:",
        "the EXPENSIVE column of the evacuation distribution. Every \
         distribution reading published in this round was taken from \
         `scanned`, which is the reference walk; `process_object` charges a \
         parent's children's BYTES to whoever scanned the parent, so a pause \
         with seventeen workers scanning and one worker copying 100% of the \
         bytes read as well balanced on every number the report printed, \
         `idle_ms=0` included. `span_ratio` is that pause's reading, and it \
         is accumulated PER PAUSE because the lifetime table launders a \
         rotating solo copier into balanced-looking rows",
    ),
    (
        // LANE `gate` (2026-09-22).
        "[GC] g1 block-offset:",
        "the block-offset table's three censuses. Every reader all three had          was under `gc/tests/`, so `CRATONVM_G1_BLOCK_OFFSET` could be turned          on against a shipped binary and produce no reading at all -- which is          what `scripts/check-orphan-instruments.sh` reported on its first real          run. `audit_violations` is a documented MUST-BE-ZERO: a non-zero says          an object holding a cross-region reference sat inside bytes a jump          skipped, which is the producer rule the whole optimisation rests on          breaking. `tail_breaks`/`no_entry` are what separate the three causes          of `jumps=0`, which otherwise print identically",
    ),
];

#[test]
fn every_w3c_census_line_is_on_the_report_emitted_on_both_shutdown_arms() {
    let text = collector_decision_report();
    for (label, why) in REQUIRED_LINES {
        assert!(
            text.contains(label),
            "`{label}` is missing from collector_decision_report().\n\
             It is there because: {why}.\n\
             That report is what `vm-cli::maybe_dump_shutdown_reports` emits on \
             BOTH the normal-return and the System.exit arm; a counter that is \
             not on it cannot be read from a shipped binary by a workload that \
             exits through `System.exit`, which is most of them.\n\
             Full report was:\n{text}"
        );
    }
}

#[test]
fn the_census_lines_print_their_zeros() {
    // The rule this round keeps re-deriving: a counter that appears only when
    // it is non-zero cannot be told apart from a counter whose report never
    // ran. `lane-e-small-findings-not-fixed.md` §4 states it as "an instrument
    // armed where it cannot fire is worse than no instrument, because its zero
    // reads as a measurement".
    //
    // This test runs in a process where no G1 collection has happened, so
    // every field below IS zero. If a future edit wraps one of these blocks in
    // an `if n > 0`, this fails.
    let text = collector_decision_report();
    for field in [
        "remark_pauses=",
        "cleanup_pauses=",
        "calls=",
        "threads_visited=",
        "threads_held=",
        "walks=",
        "region_type_decode_failsafes=",
        "cycles_with_loader_pins=",
        "loader_tail_hits=",
        // LANE W6-A — capitalised for the reason `COPIED=` and
        // `REEVACUATED_AFTER_RETIRE=` are: a reader scanning a wall of census
        // lines has to be able to see which zeros are load-bearing.
        "DEADREF_RECEIVER_HITS=",
        // LANE W7-C — the copy span's own denominators. `span_ratio=0.00` on a
        // process that evacuated nothing has to be distinguishable from
        // `span_ratio=1.00` on a process where one worker copied every byte,
        // and the only thing that separates them is the population beside it.
        "copying_pauses=",
        "empty_pauses=",
        "max_worker_bytes=",
        "span_ratio=",
        "solo_copier_pauses=",
        "copier_slots=",
        // LANE `gate` — the block-offset line, every field. `jumps=0` is the
        // ordinary reading (the lever is opt-in) and it is exactly the reading
        // that needs `tail_breaks`/`no_entry` beside it to be interpretable, so
        // an `if jumps > 0` wrapper here would hide the census in precisely the
        // state it is wanted in.
        "jumps=",
        "bytes_jumped=",
        "entries_refused=",
        "tail_breaks=",
        "no_entry=",
        "audit_spans=",
        "audit_objects=",
        "audit_violations=",
    ] {
        assert!(
            text.contains(field),
            "`{field}` must print even when it is zero.\nReport was:\n{text}"
        );
    }
}

#[test]
fn each_census_line_states_the_flag_that_governs_it() {
    // §7.1 of `orchestrator-wave-1-measurements.md`: `CRATONVM_G1_CARD_CLEAN=0`
    // ENABLED card cleaning for months, and it was caught only because lane
    // W2-E printed the flag's effective value beside the result. The rule that
    // came out of it -- "a measurement is not finished until it reports what
    // the switch it moved actually resolved to" -- is only enforceable if the
    // report carries the resolved value.
    let text = collector_decision_report();
    // LANE W6-A adds `screen=`: the dead-receiver screen is entirely behind
    // `CRATONVM_DBG_DEADREF_STORE`, so on a default build its count is zero by
    // CONSTRUCTION and "armed and clean" reads exactly like "never asked".
    for field in [
        "jit_mark_driver=",
        "discard=",
        "narrow=",
        "side_tables=",
        "screen=",
    ] {
        assert!(
            text.contains(field),
            "`{field}` is the resolved value of the switch that governs the line \
             it sits on, and it must be printed beside the number so an A/B \
             cannot be read against an arm that never engaged.\nReport was:\n{text}"
        );
    }
}

#[test]
fn a_report_with_no_collection_still_says_so_rather_than_being_empty() {
    // The pre-existing contract, re-asserted here because the W3-C block is
    // appended unconditionally and an accidental early return above it would
    // take the whole block with it.
    let text = collector_decision_report();
    assert!(
        text.contains("no collection has run yet") || text.contains("[GC] decision"),
        "the report must always lead with a decision line:\n{text}"
    );
    assert!(
        text.lines().count() >= REQUIRED_LINES.len(),
        "report is shorter than the number of lines this lane requires:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// LANE W6-A — the lines added by the wave-6 close of the unread-accessor sweep
//
// These two are NOT on `collector_decision_report`, and that is deliberate
// rather than an oversight: the card-table census needs `&self` (the table
// hangs off the collector) and the TLAB census is `tlab.rs`'s own formatter.
// Both are FORMATTERS rather than prints for exactly this reason — so the
// contract can be pinned here without capturing stderr, which is the same
// split `g1_evac_worker_census_report` and `tlab::tlab_census_lines` already
// use.
// ---------------------------------------------------------------------------

/// The `[GC] g1 card-table:` line, which `print_gc_summary` emits.
///
/// It exists because four accessors had no reader on any shipped path:
/// `G1CardTable::card_count`, `dirty_card_total`, `block_entries_in`, and the
/// pair `arena_base`/`arena_len` that names the span the third one is asked
/// about. `dirty_card_total`'s own doc calls itself the numerator of the
/// measurement that would falsify this collector's card-cleaning policy, and
/// until wave 6 that measurement could not be taken on a release binary.
#[test]
fn the_card_table_census_line_prints_its_zeros_and_its_switches() {
    let gc = G1Collector::new(G1CollectorConfig {
        heap_size: 8 * 1024 * 1024,
        region_size: 1024 * 1024,
        ..Default::default()
    });
    let line = gc.card_table_census_line();
    assert!(
        line.starts_with("[GC] g1 card-table:"),
        "unexpected prefix: {line}"
    );
    // Every field, on a collector that has collected nothing — so every count
    // below IS zero, and an `if n > 0` wrapper introduced later fails here.
    // A census that appears only when it is non-zero cannot be told apart from
    // a census whose report never ran; that is the whole of P7.
    for field in [
        "cards=",
        "dirty=",
        "saturation=",
        "block_entries=",
        "block_coverage=",
    ] {
        assert!(line.contains(field), "`{field}` missing from: {line}");
    }
    // W3-C's rule: every census line names the switch that governs it.
    // `card_clean` is what pushes saturation back down; `block_offsets` is
    // what fills the block-offset table at all, and a coverage count read
    // without it is the §7.1 failure over again.
    for switch in ["card_clean=", "block_offsets="] {
        assert!(
            line.contains(switch),
            "`{switch}` is the resolved value of a switch that governs this \
             line and must be printed beside the number: {line}"
        );
    }
    // The ratios are published beside BOTH of their terms, never alone.
    // Rule 5 of the round's method: an estimate without its sample count has
    // not been published.
    assert!(
        line.contains("cards=") && line.contains("saturation="),
        "a saturation percentage without its card count is a ratio with no \
         denominator: {line}"
    );
}

/// `tail_sinks=` on `[GC] tlab-waste:`.
///
/// `sink_tail=` has been on that line since the TLAB census existed, and with
/// no `TlabTailSink` registered it is zero by construction —
/// `reclaim_tail_via_sinks` returns on its first atomic load. The count of
/// registered sinks is what separates "nothing offered to take tails back"
/// from "the sinks declined", and `tlab_tail_sink_count` had no caller
/// anywhere in the workspace until wave 6.
///
/// **This test is not the whole wiring, and cannot be.** `VmHeap::print_gc_
/// summary` emits this line from its `Generational` arm ONLY, so a G1 run
/// never reaches it — which is why the same pair (`tail_sinks=` and
/// `sink_tail_bytes=`) is also on `[GC] g1 tlab:` in
/// `G1Collector::print_gc_summary`. That one is an `eprintln!` rather than a
/// formatter and is verified by capturing a release run instead; see
/// `w6a-twenty-nine-accessors.md` §2.2 and §6. A green test here with a G1
/// binary that prints nothing is precisely the failure this file exists to
/// prevent, one layer out.
#[test]
fn the_tlab_waste_line_carries_the_sink_count_that_explains_its_sink_bytes() {
    let [waste, _guard] = cratonvm_gc::tlab::tlab_census_lines();
    assert!(
        waste.contains("sink_tail="),
        "the bytes this line's denominator explains are missing: {waste}"
    );
    assert!(
        waste.contains("tail_sinks="),
        "`sink_tail=` cannot be read without the number of registered sinks: \
         with none, it is zero by construction and says nothing about TLAB \
         retirement.\nLine was:\n{waste}"
    );
}
