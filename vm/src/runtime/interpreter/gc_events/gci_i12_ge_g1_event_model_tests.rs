//! gci i12/ge: the G1 cycle's id and `-Xlog:gc` lines
//! (`gci-i11go-...-review-findings-20261013` item 1), the JFR young / old
//! kind (item 3) and the one-report scope of a G1 full collection
//! (`gci-i10dl-g1-full-collection-door-residuals-FIXED-20261016` (b)).
use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::runtime::unified_logging::LogTag;
use std::sync::atomic::Ordering;

const M: usize = 1024 * 1024;

fn vm(gc_algorithm: GcAlgorithm) -> SharedVm {
    SharedVm::new(VmConfig {
        gc_algorithm,
        ..VmConfig::default()
    })
}

/// HotSpot 25 G1's cycle lines: the open line, then `Pause Remark` /
/// `Pause Cleanup` (start, end, cpu) and the cycle's end under the
/// cycle's id; the heap change is the remark's.
#[test]
fn gci_i12_ge_g1_cycle_lines_have_hotspots_shape() {
    assert_eq!(render_xlog_g1_cycle_open_line(8), "GC(8) Concurrent Mark Cycle");
    let lines = render_xlog_g1_remark_lines(
        8,
        (55 * M, 40 * M, 128 * M),
        1_234_000,
        Some(100_000),
        Some(25_400_000),
        Some((0.02, 0.0)),
    );
    let text: Vec<(LogTag, &str)> = lines.iter().map(|(t, l)| (*t, l.as_str())).collect();
    assert_eq!(
        text,
        vec![
            (LogTag::GcStart, "GC(8) Pause Remark"),
            (LogTag::Gc, "GC(8) Pause Remark 55M->40M(128M) 1.234ms"),
            (LogTag::GcCpu, text[2].1),
            (LogTag::GcStart, "GC(8) Pause Cleanup"),
            (LogTag::Gc, "GC(8) Pause Cleanup 40M->40M(128M) 0.100ms"),
            (LogTag::GcCpu, text[5].1),
            (LogTag::Gc, "GC(8) Concurrent Mark Cycle 25.400ms"),
        ]
    );
    assert!(text[2].1.starts_with("GC(8) User="), "{}", text[2].1);
    assert!(text[5].1.ends_with("Real=0.00s"), "{}", text[5].1);
    // An incomplete remark: its lines only, the cycle stays open; no cpu
    // line without a sample, no end line without a known start.
    let lines = render_xlog_g1_remark_lines(9, (3 * M, 3 * M, 8 * M), 500_000, None, None, None);
    let text: Vec<&str> = lines.iter().map(|(_, l)| l.as_str()).collect();
    assert_eq!(text, vec!["GC(9) Pause Remark", "GC(9) Pause Remark 3M->3M(8M) 0.500ms"]);
    let lines =
        render_xlog_g1_remark_lines(9, (3 * M, 2 * M, 8 * M), 500_000, Some(10_000), None, None);
    assert_eq!(lines.len(), 4, "no cycle end line without its start");
}

/// G1's initial mark writes no JFR row of its own (its concurrent-start
/// collection's `GC Pause` is the only one under that id), even while a
/// recording would take one: `gen_cycle_jfr_row` answers `None` for it.
#[test]
fn gci_i12_ge_the_g1_initial_mark_writes_no_row() {
    use super::NonCollectionPause as P;
    assert!(g1_initial_mark_has_no_row(P::G1InitialMark));
    let shared = vm(GcAlgorithm::G1);
    assert!(gen_cycle_jfr_row(&shared, P::G1InitialMark, Some((3, 0))).is_none());
    assert!(gen_cycle_jfr_row(&shared, P::G1Remark, None).is_none(), "no open cycle");
}

/// A G1 marking pause takes its cycle edge on every run (no recording):
/// the initial mark takes ONE id and opens the cycle; the remark finds it
/// and re-opens it; a folded initial mark takes the report's id. Off G1
/// and for other pauses, no edge.
#[test]
fn gci_i12_ge_a_g1_cycle_takes_its_id_on_every_run() {
    use super::NonCollectionPause as P;
    let shared = vm(GcAlgorithm::G1);
    let before = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
    let open = g1_cycle_edge(&shared, P::G1InitialMark, None).expect("a G1 initial mark");
    assert_eq!(open.cycle_id, before);
    assert_eq!(shared.mem.gc_cycle_count.load(Ordering::Relaxed), before + 1);
    assert_eq!(g1_open_cycle_id(&shared), Some(before));
    open.cycle.mark_started(open.cycle_id, 77);
    let remark = g1_cycle_edge(&shared, P::G1Remark, None).expect("the open cycle");
    assert_eq!((remark.cycle_id, remark.mark_start_ns), (before, 77));
    assert_eq!(g1_open_cycle_id(&shared), Some(before), "re-opened for a retry");
    assert_eq!(remark.cycle_start_ns, 77);
    // A retried remark: no JFR marking start, the -Xlog cycle start kept.
    let retry = g1_cycle_edge(&shared, P::G1Remark, None).expect("still open");
    assert_eq!((retry.mark_start_ns, retry.cycle_start_ns), (0, 77));
    assert_eq!(
        shared.mem.gc_cycle_count.load(Ordering::Relaxed),
        before + 1,
        "the remark takes no id"
    );
    let folded = g1_cycle_edge(&shared, P::G1InitialMark, Some(500)).expect("folded");
    assert_eq!(folded.cycle_id, 500);
    assert!(!folded.xlog, "a folded pause prints no cycle line");
    assert_eq!(shared.mem.gc_cycle_count.load(Ordering::Relaxed), before + 1);
    assert!(g1_cycle_edge(&shared, P::FrameTrace, None).is_none());
    let generational = vm(GcAlgorithm::Generational);
    assert!(g1_cycle_edge(&generational, P::G1InitialMark, None).is_none());
    assert_eq!(g1_open_cycle_id(&generational), None);
}

/// Item 3: the kind follows HotSpot's operation, not the cause text.
#[test]
fn gci_i12_ge_the_jfr_kind_follows_the_operation() {
    let full = [
        GcDoor::SystemGc,
        GcDoor::DiagnosticCommand,
        GcDoor::HeapInspection,
        GcDoor::HeapDump,
    ];
    const GEN: (bool, bool) = (true, false);
    const G1: (bool, bool) = (false, true);
    const ZGC: (bool, bool) = (false, false);
    for backend in [GEN, G1] {
        for door in full {
            assert!(jfr_old_event_rule(door, false, backend, false), "{door:?}");
        }
        for door in [
            GcDoor::AllocationThreshold,
            GcDoor::AllocationFailure,
            GcDoor::G1HumongousAllocation,
        ] {
            assert!(!jfr_old_event_rule(door, false, backend, false), "{door:?}");
            assert!(jfr_old_event_rule(door, true, backend, false), "old wins");
        }
    }
    // `-XX:+ExplicitGCInvokesConcurrent`: G1's System.gc / GC.run are a
    // concurrent start (young); Serial ignores the flag.
    for door in [GcDoor::SystemGc, GcDoor::DiagnosticCommand] {
        assert!(!jfr_old_event_rule(door, false, G1, true), "{door:?}");
        assert!(jfr_old_event_rule(door, false, GEN, true), "{door:?}");
    }
    assert!(jfr_old_event_rule(GcDoor::HeapDump, false, G1, true));
    // Metadata: Serial's full collection, G1's concurrent-start young pause.
    assert!(jfr_old_event_rule(GcDoor::MetadataThreshold, false, GEN, false));
    assert!(!jfr_old_event_rule(GcDoor::MetadataThreshold, false, G1, false));
    assert!(jfr_old_event_rule(GcDoor::G1CompactionPause, false, G1, false));
    // ZGC: the historical rule (`System.gc` only), unchanged.
    assert!(jfr_old_event_rule(GcDoor::SystemGc, false, ZGC, false));
    assert!(!jfr_old_event_rule(GcDoor::DiagnosticCommand, false, ZGC, false));
}

/// Which G1 full collections are ONE `Pause Full` on HotSpot G1: not
/// `Metadata GC Threshold` (a concurrent-start young pause), not
/// `System.gc()` / `GC.run` under `-XX:+ExplicitGCInvokesConcurrent`.
#[test]
fn gci_i12_ge_which_full_collections_are_one_pause_full() {
    for door in [
        GcDoor::SystemGc,
        GcDoor::DiagnosticCommand,
        GcDoor::HeapInspection,
        GcDoor::HeapDump,
    ] {
        assert!(g1_door_is_a_pause_full(door, false), "{door:?}");
    }
    assert!(!g1_door_is_a_pause_full(GcDoor::SystemGc, true));
    assert!(!g1_door_is_a_pause_full(GcDoor::DiagnosticCommand, true));
    assert!(g1_door_is_a_pause_full(GcDoor::HeapInspection, true));
    for door in [
        GcDoor::MetadataThreshold,
        GcDoor::AllocationThreshold,
        GcDoor::AllocationFailure,
        GcDoor::G1CompactionPause,
        GcDoor::G1HumongousAllocation,
    ] {
        assert!(!g1_door_is_a_pause_full(door, false), "{door:?}");
    }
}

/// The scope folds its owner's collection pauses always and its marking
/// pauses while the full collection's own cycle runs; nobody else's.
#[test]
fn gci_i12_ge_a_full_scope_folds_only_its_owners_pauses() {
    assert!(g1_full_scope_folds(true, false, true));
    assert!(g1_full_scope_folds(true, false, false));
    assert!(g1_full_scope_folds(true, true, true));
    assert!(!g1_full_scope_folds(true, true, false));
    assert!(!g1_full_scope_folds(false, false, true));
    assert!(!g1_full_scope_folds(false, true, true));

    let shared = vm(GcAlgorithm::G1);
    assert!(g1_full_scope_fold(&shared, false).is_none(), "no scope open");
    let other = std::thread::spawn(|| std::thread::current().id())
        .join()
        .expect("the id thread");
    let scope = |owner| crate::vm::realms::heap_realm::G1FullReportScope {
        owner,
        gc_id: None,
        pause_ns: 0,
        pauses: 0,
        marking_folded: true,
        marking_ns: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        longest_ns: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        ref_census: None,
    };
    *shared.mem.alloc_ladder.g1_full_report.lock() = Some(scope(other));
    assert!(g1_full_scope_fold(&shared, false).is_none(), "another thread's scope");
    *shared.mem.alloc_ladder.g1_full_report.lock() = Some(scope(std::thread::current().id()));
    let before = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
    let (id, _) = g1_full_scope_fold(&shared, false).expect("the owner's pause");
    assert_eq!(id, before);
    let (again, marking) = g1_full_scope_fold(&shared, true).expect("its own cycle");
    assert_eq!(again, id, "one id per report");
    assert_eq!(shared.mem.gc_cycle_count.load(Ordering::Relaxed), before + 1);
    marking.add(5);
    g1_full_scope_add_collection(&shared, 7);
    {
        let guard = shared.mem.alloc_ladder.g1_full_report.lock();
        let s = (*guard).as_ref().expect("still open");
        assert_eq!((s.pause_ns, s.pauses), (7, 1));
        assert_eq!(s.marking_ns.load(Ordering::Relaxed), 5);
        assert_eq!(s.longest_ns.load(Ordering::Relaxed), 7, "the longest folded pause");
    }
    {
        let mut guard = shared.mem.alloc_ladder.g1_full_report.lock();
        if let Some(s) = &mut *guard {
            s.marking_folded = false;
        }
    }
    assert!(g1_full_scope_fold(&shared, true).is_none(), "a later cycle is its own");
    assert!(g1_full_scope_fold(&shared, false).is_some());
    let generational = vm(GcAlgorithm::Generational);
    *generational.mem.alloc_ladder.g1_full_report.lock() =
        Some(scope(std::thread::current().id()));
    assert!(g1_full_scope_fold(&generational, false).is_none(), "G1 only");
}

/// gci i24/rv (`gci-i24rv-i22-i23-default-path-review-findings-FIXED-20261017`
/// item 16): the remark of the `System.gc()` stop-the-world cycle (no
/// open concurrent cycle) inside a one-report full collection files its
/// `gc+ref` block under the report's id; with no scope -- the default --
/// the answer is `None` and `remark_reference_xlog_id` keeps its ordinal.
#[test]
fn gci_i24rv_a_folded_remark_files_under_the_reports_id() {
    let shared = vm(GcAlgorithm::G1);
    assert_eq!(g1_full_scope_marking_id(&shared), None, "no scope open");
    let other = std::thread::spawn(|| std::thread::current().id())
        .join()
        .expect("the id thread");
    let scope = |owner, gc_id, marking_folded| crate::vm::realms::heap_realm::G1FullReportScope {
        owner,
        gc_id,
        pause_ns: 0,
        pauses: 0,
        marking_folded,
        marking_ns: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        longest_ns: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        ref_census: None,
    };
    let me = std::thread::current().id();
    *shared.mem.alloc_ladder.g1_full_report.lock() = Some(scope(me, Some(41), true));
    assert_eq!(g1_full_scope_marking_id(&shared), Some(41));
    *shared.mem.alloc_ladder.g1_full_report.lock() = Some(scope(me, None, true));
    assert_eq!(g1_full_scope_marking_id(&shared), None, "no pause took the id yet");
    *shared.mem.alloc_ladder.g1_full_report.lock() = Some(scope(me, Some(41), false));
    assert_eq!(g1_full_scope_marking_id(&shared), None, "its marking is done");
    *shared.mem.alloc_ladder.g1_full_report.lock() = Some(scope(other, Some(41), true));
    assert_eq!(g1_full_scope_marking_id(&shared), None, "another thread's scope");
    let generational = vm(GcAlgorithm::Generational);
    *generational.mem.alloc_ladder.g1_full_report.lock() = Some(scope(me, Some(41), true));
    assert_eq!(g1_full_scope_marking_id(&generational), None, "G1 only");
    // The remark's id asks the open cycle first, then the scope, then
    // the ordinal.
    let src = concat!(
        include_str!("../gc_and_alloc/mod.rs"),
        "\n",
        include_str!("../gc_and_alloc/mtroots_on.rs"),
        "\n",
        include_str!("../gc_and_alloc/gc_notifications_go_to_delivery.rs"),
        "\n",
        include_str!("../gc_and_alloc/zgc_refusal_line.rs"),
        "\n",
        include_str!("../gc_and_alloc/finalizers_forced.rs"),
        "\n",
        include_str!("../gc_and_alloc/reference_round.rs"),
        "\n",
        include_str!("../gc_and_alloc/on_out_of_memory_error_commands.rs"),
        "\n",
        include_str!("../gc_and_alloc/frozen_peer_referents.rs"),
        "\n",
        include_str!("../gc_and_alloc/g1_force_full_cycle.rs")
    )
    .replace("\r\n", "\n");
    let at = src
        .find("\npub(in crate::runtime::interpreter::gc_and_alloc) fn remark_reference_xlog_id(")
        .expect("the id");
    let body = &src[at..at + src[at..].find("\n\u{7d}\n").expect("its end")];
    let cycle = body.find("g1_open_cycle_id(shared)").expect("the cycle");
    let scoped = body
        .find(".or_else(|| super::gc_events::g1_full_scope_marking_id(shared))")
        .expect("the scope");
    let ordinal = body.find(".gc_cycle_count").expect("the ordinal");
    assert!(cycle < scoped && scoped < ordinal);
}

/// gci i25/sg (`gci-i24rv-i22-i23-default-path-review-findings-FIXED-20261017`
/// item 16): inside a one-report G1 full collection every
/// reference-processing round (young, remark, compaction) hands its
/// census to the scope -- the owner's collection rounds always, its
/// remark while its own cycle runs -- and the report prints ONE summed
/// block under its id at the top-level indent. Nothing is absorbed with
/// no scope (the default), before the id is taken, for another thread,
/// for a later cycle's remark, or off G1.
#[test]
fn gci_i25sg_a_full_report_prints_one_summed_reference_block() {
    use crate::runtime::unified_logging::RefProcCensus;
    let young = RefProcCensus {
        discovered: [0, 2, 0, 0],
        processed: [0, 2, 0, 0],
        total_ns: 1_000_000,
        phase_ns: [1_000_000, 0, 0],
        ..RefProcCensus::default()
    };
    let remark = RefProcCensus {
        discovered: [1, 3, 0, 1],
        processed: [1, 3, 0, 1],
        total_ns: 2_000_000,
        phase_ns: [2_000_000, 0, 0],
        ..RefProcCensus::default()
    };
    let sum = ref_census_sum(Some(&young), &remark);
    assert_eq!(sum.discovered, [1, 5, 0, 1]);
    assert_eq!(sum.processed, [1, 5, 0, 1]);
    assert_eq!(sum.dropped, [0; 4]);
    assert_eq!((sum.total_ns, sum.phase_ns), (3_000_000, [3_000_000, 0, 0]));
    assert_eq!(ref_census_sum(None, &young), young);

    let shared = vm(GcAlgorithm::G1);
    assert!(
        !g1_full_scope_absorb_ref_census(&shared, false, &young),
        "no scope open: deferred as before"
    );
    let other = std::thread::spawn(|| std::thread::current().id())
        .join()
        .expect("the id thread");
    let me = std::thread::current().id();
    let scope = |owner, gc_id, marking_folded| crate::vm::realms::heap_realm::G1FullReportScope {
        owner,
        gc_id,
        pause_ns: 0,
        pauses: 0,
        marking_folded,
        marking_ns: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        longest_ns: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        ref_census: None,
    };
    let taken = |s: &SharedVm| {
        (*s.mem.alloc_ladder.g1_full_report.lock())
            .as_ref()
            .and_then(|scope| scope.ref_census)
    };
    *shared.mem.alloc_ladder.g1_full_report.lock() = Some(scope(me, None, true));
    assert!(!g1_full_scope_absorb_ref_census(&shared, false, &young), "no id yet");
    *shared.mem.alloc_ladder.g1_full_report.lock() = Some(scope(other, Some(9), true));
    assert!(!g1_full_scope_absorb_ref_census(&shared, false, &young), "another thread");
    *shared.mem.alloc_ladder.g1_full_report.lock() = Some(scope(me, Some(9), true));
    assert!(g1_full_scope_absorb_ref_census(&shared, false, &young), "the young round");
    assert!(g1_full_scope_absorb_ref_census(&shared, true, &remark), "its own remark");
    assert_eq!(taken(&shared), Some(sum));
    {
        let mut guard = shared.mem.alloc_ladder.g1_full_report.lock();
        if let Some(s) = &mut *guard {
            s.marking_folded = false;
        }
    }
    assert!(
        !g1_full_scope_absorb_ref_census(&shared, true, &remark),
        "a later cycle's remark reports its own block"
    );
    assert!(g1_full_scope_absorb_ref_census(&shared, false, &young), "the compaction round");
    let all = ref_census_sum(Some(&sum), &young);
    assert_eq!(taken(&shared), Some(all));
    let generational = vm(GcAlgorithm::Generational);
    *generational.mem.alloc_ladder.g1_full_report.lock() = Some(scope(me, Some(9), true));
    assert!(!g1_full_scope_absorb_ref_census(&generational, false, &young), "G1 only");

    // The report's block: one, under its id, top-level indent.
    assert!(g1_full_report_reference_lines(9, None).is_empty());
    let lines: Vec<String> = g1_full_report_reference_lines(9, Some(&all))
        .into_iter()
        .map(|(_, _, t)| t)
        .collect();
    assert_eq!(lines.len(), 17, "{lines:?}");
    assert!(lines.iter().all(|l| l.starts_with("GC(9) ")), "{lines:?}");
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.starts_with("GC(9) Reference Processing: "))
            .count(),
        1,
        "{lines:?}"
    );
    assert!(lines.contains(&"GC(9) Reference Processing: 4.0ms".to_string()), "{lines:?}");
    assert!(
        lines.contains(&"GC(9)   WeakReference Discovered: 7, Dropped: 0, Processed: 7".to_string()),
        "{lines:?}"
    );

    // Wiring: both producers offer the census to the scope before
    // deferring it; the report prints the block right after `gc,start`.
    let alloc = concat!(include_str!("../gc_and_alloc/mod.rs"), "\n", include_str!("../gc_and_alloc/mtroots_on.rs"), "\n", include_str!("../gc_and_alloc/gc_notifications_go_to_delivery.rs"), "\n", include_str!("../gc_and_alloc/zgc_refusal_line.rs"), "\n", include_str!("../gc_and_alloc/finalizers_forced.rs"), "\n", include_str!("../gc_and_alloc/reference_round.rs"), "\n", include_str!("../gc_and_alloc/on_out_of_memory_error_commands.rs"), "\n", include_str!("../gc_and_alloc/frozen_peer_referents.rs"), "\n", include_str!("../gc_and_alloc/g1_force_full_cycle.rs")).replace("\r\n", "\n");
    let body_of = |src: &str, sig: &str| -> String {
        let at = src.find(sig).unwrap_or_else(|| panic!("{sig}"));
        let rest = &src[at..];
        rest[..rest.find("\n\u{7d}\n").expect("its end")].to_string()
    };
    let post = crate::runtime::interpreter::gc_and_alloc::process_references_text(alloc.as_str());
    let absorb = post
        .find("g1_full_scope_absorb_ref_census(shared, false, &census)")
        .expect("the collection round offers its census");
    let defer = post
        .find("defer_xlog_reference_processing_at(")
        .expect("and defers otherwise");
    assert!(absorb < defer);
    let rows = body_of(
        alloc.as_str(),
        "\npub(in crate::runtime::interpreter::gc_and_alloc) fn remark_reference_rows(",
    );
    let absorb = rows
        .find("g1_full_scope_absorb_ref_census(shared, true, &census)")
        .expect("the remark offers its census");
    let defer = rows
        .find("defer_xlog_reference_processing(")
        .expect("and defers otherwise");
    assert!(absorb < defer);
    let own = include_str!("../gc_events.rs").replace("\r\n", "\n");
    let report = body_of(own.as_str(), "\n    fn xlog_lines(\n");
    let start = report.find("LogTag::GcStart").expect("gc,start");
    let block = report
        .find("g1_full_report_reference_lines(gc_id, refs)")
        .expect("the block");
    let heap = report.find("render_xlog_g1_heap_lines(").expect("gc,heap");
    assert!(start < block && block < heap);
}


/// The report's notification is the `G1 Old Generation` record only,
/// ending at its start plus the folded pauses' sum.
#[test]
fn gci_i12_ge_the_report_notifies_the_old_bean_only() {
    use cratonvm_gc::gc_metrics::{
        BackendBeanShape, BackendGcBeans, GcBeanSnapshot, JmxPoolSample,
    };
    let b = BackendGcBeans::new(BackendBeanShape::G1);
    let s = [JmxPoolSample::default(); 3];
    let before = GcBeanSnapshot::now(b.collectors(), b.pools(&s));
    b.note_collection(1_000);
    b.note_full_collection(4_000_000);
    let after = GcBeanSnapshot::now(b.collectors(), b.pools(&s));
    let recs = cratonvm_gc::gc_metrics::gc_notifications_between(&before, &after, "System.gc()", 1);
    assert_eq!(recs.len(), 2, "another thread's young pause moved its bean too");
    let recs = g1_full_report_records(recs, 4_000_000);
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].collector, "G1 Old Generation");
    assert_eq!(recs[0].action, "end of major GC");
    assert_eq!(
        recs[0].end.duration_since(recs[0].start),
        std::time::Duration::from_nanos(4_000_000)
    );
}
