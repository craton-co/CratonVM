use super::*;

fn body(src: &str, sig: &str) -> String {
    let start = src
        .find(&format!("\n{sig}"))
        .unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &src[start + 1..];
    let stop = rest
        .find("\n\u{7d}\n")
        .unwrap_or_else(|| panic!("end of `{sig}` not found"));
    rest[..stop].to_string()
}

/// Only a G1 remark with no open concurrent cycle is the stop-the-world
/// full mark's.
#[test]
fn gci_i30ob_only_a_cycleless_g1_remark_is_the_stw_full_mark() {
    assert!(stw_full_mark_remark(NonCollectionPause::G1Remark, true, false));
    assert!(!stw_full_mark_remark(NonCollectionPause::G1Remark, true, true));
    assert!(!stw_full_mark_remark(NonCollectionPause::G1Remark, false, false));
    assert!(!stw_full_mark_remark(NonCollectionPause::G1InitialMark, true, false));
    assert!(!stw_full_mark_remark(NonCollectionPause::GenRemark, true, false));
}

/// The stop-the-world full mark's remark books no collection and queues
/// no notification; its finish stamps every pool instead.
#[test]
fn gci_i30ob_the_stw_full_mark_books_nothing() {
    let src = include_str!("../gc_events.rs").replace("\r\n", "\n");
    let seal = body(&src, "pub(super) fn non_collection_pause_seal(");
    let guard = seal.find("_ if b.stw_full_mark => {}").expect("no booking");
    let book = seal
        .find("None => b.set.note_non_collection_pause(work_ns)")
        .expect("the ordinary booking");
    assert!(guard < book);
    let finish = body(&src, "pub(super) fn non_collection_pause_finish(");
    let stamp = finish
        .find("stw_full_mark_stamps_every_pool(&b.set, &after);")
        .expect("every pool stamped");
    let records = finish
        .find("gc_notifications_between(")
        .expect("the notification records");
    assert!(stamp < records, "it returns before any record is made");
    let start = body(&src, "pub(super) fn non_collection_pause_start(");
    assert!(start.contains(
        "stw_full_mark_remark(kind, shared.mem.heap.is_g1(), g1_cycle.is_some())"
    ));
}


/// vi20 Batch 3 `sg25_f_fulljfr_on`: HotSpot 25 names the G1 full
/// collection `G1Full` in `jdk.GarbageCollection` (`G1Old` is its
/// concurrent cycle's tracer).
#[test]
fn gci_i30ob_the_g1_full_collection_is_g1full_in_jfr() {
    assert_eq!(G1_FULL_JFR_NAME, "G1Full");
}

/// Hand-back 1 (vi30-ob S1): a G1 full collection leaves `G1 Survivor
/// Space`'s collection usage at 0 used / 0 committed, as HotSpot's
/// `G1FullCollector` does; the other pools keep their samples.
#[test]
fn gci_i30ob_a_g1_full_collection_leaves_the_survivor_empty() {
    use cratonvm_gc::gc_metrics::JmxPoolUsage;
    let pool = |name: &'static str, used: u64, committed: u64| JmxPoolUsage {
        name,
        init: Some(0),
        used,
        committed,
        max: None,
        collection_used: None,
        managers: &[],
    };
    let rows = g1_full_collection_usage_rows(&[
        pool("G1 Eden Space", 0, 4 << 20),
        pool(cratonvm_gc::gc_metrics::JMX_POOL_G1_SURVIVOR, 1 << 20, 2 << 20),
        pool("G1 Old Gen", 9 << 20, 16 << 20),
    ]);
    assert_eq!((rows[0].used, rows[0].committed), (0, 4 << 20));
    assert_eq!((rows[1].used, rows[1].committed), (0, 0));
    assert_eq!((rows[2].used, rows[2].committed), (9 << 20, 16 << 20));
    let src = include_str!("../gc_events.rs").replace("\r\n", "\n");
    let stamp = body(&src, "fn stw_full_mark_stamps_every_pool(");
    assert!(stamp.contains("g1_full_collection_usage_rows(&after.pools)"));
    let finish = body(&src, "    pub(super) fn finish(self) {");
    assert!(finish.contains("g1_full_collection_usage_rows(&after.pools)"));
}

/// Hand-back 2 (vi34-ob S1): the `System.gc()` door re-stamps the
/// Survivor pool alone at its end, after every pause it ran, for a
/// stop-the-world cycle without the one-report scope; on a fresh G1 VM the
/// stamp reads HotSpot's empty Survivor.
#[test]
fn gci_i30ob_the_full_door_ends_with_an_empty_survivor_stamp() {
    use crate::config::{GcAlgorithm, VmConfig};
    let g1 = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::G1,
        ..VmConfig::default()
    });
    g1_full_door_leaves_the_survivor_empty(&g1);
    let set = g1.mem.backend_gc_beans().expect("G1 beans");
    let sampler = g1.mem.heap.jmx_pool_sampler().expect("a sampler");
    let pools = set.pools(&sampler.sample());
    let survivor = pools
        .iter()
        .find(|p| p.name == cratonvm_gc::gc_metrics::JMX_POOL_G1_SURVIVOR)
        .expect("the survivor pool");
    assert_eq!(survivor.collection_used, Some(0));
    let src = include_str!("../gc_and_alloc/zgc_refusal_line.rs").replace("\r\n", "\n");
    let f = body(
        &src,
        "pub(in crate::runtime::interpreter::gc_and_alloc) fn force_full_collection(",
    );
    let compaction = f
        .find("g1_full_compaction_pause(shared, thread, door);")
        .expect("the compaction pause");
    let stamp = f
        .find("super::gc_events::g1_full_door_leaves_the_survivor_empty(shared);")
        .expect("the door-end stamp");
    let finish = f.find("report.finish()").expect("the one report");
    assert!(compaction < stamp && stamp < finish);
    // gci i41/rs: the door-end call survives only under the `=0` kill.
    assert!(f.contains("if stw_cycle && full_report.is_none()"));
    assert!(f.contains("&& !g1_full_door_survivor_stamp_in_pause_enabled()"));
}

/// gci i41/rs (`gci-i39rv-waves-i35-i38-review-residuals-FIXED-20261016` item
/// 1): the in-pause stamp. On a fresh G1 VM the shared helper stamps
/// HotSpot's empty Survivor; a pause with no bean snapshot (a folded
/// one-report pause, or no start at all) stamps nothing; and the pause
/// takes the stamp after its seal and before its release.
#[test]
fn gci_i41rs_the_compaction_pause_stamps_the_survivor_inside_the_pause() {
    use crate::config::{GcAlgorithm, VmConfig};
    let g1 = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::G1,
        ..VmConfig::default()
    });
    let set = g1.mem.backend_gc_beans().expect("G1 beans");
    let sampler = g1.mem.heap.jmx_pool_sampler().expect("a sampler");
    let survivor_collection_used = || {
        set.pools(&sampler.sample())
            .iter()
            .find(|p| p.name == cratonvm_gc::gc_metrics::JMX_POOL_G1_SURVIVOR)
            .expect("the survivor pool")
            .collection_used
    };
    // No start: nothing stamped.
    let untouched = survivor_collection_used();
    g1_full_door_compaction_stamps_the_survivor(&g1, &None);
    assert_eq!(survivor_collection_used(), untouched, "no pause, no stamp");
    g1_stamp_the_survivor_as_a_full_collection_leaves_it(set, &set.pools(&sampler.sample()));
    assert_eq!(survivor_collection_used(), Some(0));
    let src =
        include_str!("../gc_and_alloc/gc_notifications_go_to_delivery.rs").replace("\r\n", "\n");
    let pause = body(
        &src,
        "pub(in crate::runtime::interpreter::gc_and_alloc) fn run_collection_pause(",
    );
    let before = pause
        .find("let g1_full_compactions_before = g1_full_compaction_pause_count(shared);")
        .expect("the count before the collection");
    let roots = pause
        .find("collect_roots_registry_appended(shared, thread)")
        .expect("the roots");
    let seal = pause.find("gc_event_seal(shared, &mut gc_event);").expect("the seal");
    let stamp = pause
        .find("super::gc_events::g1_full_door_compaction_stamps_the_survivor(shared, &gc_event);")
        .expect("the in-pause stamp");
    let release = pause
        .find(".complete_gc(result.pointer_map)")
        .expect("the release");
    assert!(before < roots && seal < stamp && stamp < release);
    let full = include_str!("../gc_and_alloc/zgc_refusal_line.rs").replace("\r\n", "\n");
    let helper = body(
        &full,
        "pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_door_compaction_pause_stamps_survivor(",
    );
    assert!(helper.contains("&& switch_on()"), "the switch is read last");
}

/// Hand-back 3 (vi34-ob `.hse`): a `GCTraceTime` start line needs its
/// scope's tag set selected too -- `gc` for the pause start lines,
/// `gc,phases` for a phase -- so `-Xlog:gc+start` alone prints none.
#[test]
fn gci_i30ob_start_lines_need_their_scope_tag_set() {
    let src = include_str!("../gc_events.rs").replace("\r\n", "\n");
    for writer in [
        "fn xlog_collection_lines(",
        "fn xlog_escalated_collection_lines(",
        "    fn xlog_lines(",
    ] {
        let b = body(&src, writer);
        assert!(b.contains("xlog_start_line_on(LogTag::GcStart, LogTag::Gc)"), "{writer}");
        assert!(!b.contains("if xlog_on(LogTag::GcStart)"), "{writer}");
        assert!(!b.contains("|| xlog_on(LogTag::GcPhasesStart))"), "{writer}");
    }
    let remark = body(&src, "fn g1_cycle_edge_finish(");
    assert!(remark.contains("if !xlog_on(crate::runtime::unified_logging::LogTag::Gc)"));
    let helper = body(&src, "fn xlog_start_line_on(");
    assert!(helper.contains("xlog_on(scope) && xlog_on(start)"));
}

/// h4g1 item 6: use HotSpot's cycle-bean shape.
#[test]
fn gci_i30ob_hotspot_cycle_beans_are_default_on() {
    assert!(g1_jmx_hotspot_cycle_beans_enabled());
}

/// HotSpot 25 G1 Full GC's five info phases, in Serial's line shape; the
/// times sum to the report's duration.
#[test]
fn gci_i30ob_g1_full_gc_phases_are_hotspots() {
    let (s, e) = g1_full_gc_phase_lines(7, 0, 3_218_000);
    assert_eq!(s, "GC(7) Phase 1: Mark live objects");
    assert_eq!(e, "GC(7) Phase 1: Mark live objects 3.218ms");
    assert_eq!(g1_full_gc_phase_lines(7, 1, 0).1, "GC(7) Phase 2: Prepare compaction 0.000ms");
    assert_eq!(g1_full_gc_phase_lines(7, 2, 0).0, "GC(7) Phase 3: Adjust pointers");
    assert_eq!(g1_full_gc_phase_lines(7, 3, 0).0, "GC(7) Phase 4: Compact heap");
    assert_eq!(g1_full_gc_phase_lines(7, 4, 0).0, "GC(7) Phase 5: Reset Metadata");
    let ns = g1_full_phase_ns(5, 9);
    assert_eq!(ns, [5, 0, 0, 9, 0]);
    assert_eq!(ns.iter().sum::<u64>(), 14);
    // The reference block sits inside Phase 1.
    let src = include_str!("../gc_events.rs").replace("\r\n", "\n");
    let lines = body(&src, "    fn xlog_lines(");
    let open = lines.find("phase(0, true, false);").expect("phase 1 opens");
    let refs = lines
        .find("g1_full_report_reference_lines(gc_id, refs)")
        .expect("the reference block");
    let close = lines.find("phase(0, false, true);").expect("phase 1 closes");
    assert!(open < refs && refs < close);
}

/// g3x item 12: no class unloaded, no second walk.
#[test]
fn gci_i30ob_metaspace_after_is_the_before_without_an_unload() {
    assert!(metaspace_after_is_the_before(3, 3));
    assert!(!metaspace_after_is_the_before(3, 4));
    let src = include_str!("../gc_events.rs").replace("\r\n", "\n");
    let split = body(&src, "fn xlog_escalated_collection_lines(");
    assert!(split.contains("metaspace_after_bytes(shared, start)"));
}

/// g3x item 7: every Serial / G1 collection's `gc,start` line carries the
/// pause's start.
#[test]
fn gci_i30ob_gc_start_is_decorated_with_the_pause_start() {
    let src = include_str!("../gc_events.rs").replace("\r\n", "\n");
    let plain = body(&src, "fn xlog_collection_lines(");
    assert!(plain.contains("Some(start.t0),"));
    let split = body(&src, "fn xlog_escalated_collection_lines(");
    assert!(split.contains("Some(start.t0),"));
    assert!(split.contains("Some(start.t0 + std::time::Duration::from_nanos(young_ns)),"));
    let remark = body(&src, "fn g1_cycle_edge_finish(");
    assert!(remark.contains("edge.pause_start + std::time::Duration::from_nanos(remark_ns),"));
    assert!(remark.contains("log_unified_at(&[tag], LogLevel::Info, &line, at);"));
}

/// tl item 6: the TLAB line is printed from the start record, so a
/// deferred G1 young report (which owns its `GcEventStart`) keeps it.
#[test]
fn gci_i30ob_the_tlab_line_travels_with_the_start() {
    let src = include_str!("../gc_events.rs").replace("\r\n", "\n");
    let deferred = src
        .find("pub(super) struct G1DeferredYoungReport {")
        .expect("the deferred report");
    assert!(src[deferred..].contains("    start: GcEventStart,"));
    for writer in ["fn xlog_collection_lines(", "fn xlog_escalated_collection_lines("] {
        let b = body(&src, writer);
        assert!(b.contains("log_tlab_totals(start.tlab_totals.as_deref(),"), "{writer}");
    }
}
