//! gci i27/ob: the `gc,jni` line (`gcg-g3x-xlog-gc-review-findings-FIXED-20261016`
//! item 9) and the G1 concurrent-start label taken from the drive's
//! outcome (`gci-i19ob-g1-concurrent-start-label-is-a-prediction-FIXED-20261017`).
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

/// HotSpot 25's `GCLockerTimingDebugLogger` text (measured on 25.0.3:
/// `Thread blocked to start GC. Resumed after 0ms. Thread "main".`).
#[test]
fn gci_i27ob_gc_jni_line_is_hotspots() {
    assert_eq!(
        render_xlog_gc_jni_line("main"),
        "Thread blocked to start GC. Resumed after 0ms. Thread \"main\"."
    );
    // Printed by the requester before the collection's own lines, and
    // never for a pause folded into a G1 full collection's one report.
    let src = include_str!("../gc_events.rs").replace("\r\n", "\n");
    let by = body(&src, "pub(super) fn gc_event_finish_by(");
    let jni = by.find("render_xlog_gc_jni_line(").expect("the jni line");
    let finish = by.find("gc_event_finish_with(").expect("the collection's lines");
    assert!(jni < finish);
    assert!(by.contains("s.full_scope_id.is_none()"));
    assert!(by.contains("xlog_serial_or_g1_lines(&shared.mem.heap)"));
}

/// The label of a deferred report is the drive's outcome.
#[test]
fn gci_i27ob_deferred_label_follows_the_drive() {
    assert_eq!(g1_deferred_young_label(true), G1YoungPauseType::ConcurrentStart);
    assert_eq!(g1_deferred_young_label(false), G1YoungPauseType::Normal);
    assert_eq!(
        g1_deferred_young_label(true).xlog_label(),
        "Pause Young (Concurrent Start)"
    );
    assert_eq!(g1_deferred_young_label(false).jfr_type(), "Normal");
}

/// Only the IHOP prediction is deferred (the metadata opt-in's cycle opens
/// unconditionally), and only on the single-collection path.
#[test]
fn gci_i27ob_only_the_ihop_prediction_is_deferred() {
    let src = include_str!("../gc_events.rs").replace("\r\n", "\n");
    let with = body(&src, "fn gc_event_finish_with(");
    let defer = with
        .find("return Some(G1DeferredYoungReport \u{7b}")
        .expect("the deferral");
    let gate = with
        .find("g1_concurrent_start_is_the_ihop_prediction(&start, door)")
        .expect("the IHOP gate");
    let switch = with
        .find("true")
        .expect("the kill switch");
    let emit = with
        .find("emit_collection_events(")
        .expect("the immediate report");
    assert!(gate < defer && switch < defer && defer < emit);
    let pred = body(&src, "fn g1_concurrent_start_is_the_ihop_prediction(");
    assert!(pred.contains("start.g1_concurrent_start"));
    assert!(pred.contains("g1_door_drives_mark_cycle(door,"));
    // A requester-less report is labelled by the prediction, as before.
    let by = body(&src, "pub(super) fn gc_event_finish_by(");
    assert!(by.contains("Some(id) => g1_park_deferred_young_report(shared, id, report),"));
    // A stale parked report is printed before this pause's lines.
    let stale = by
        .find("g1_emit_deferred_young_report(shared, id, false);")
        .expect("the stale flush");
    assert!(stale < by.find("render_xlog_gc_jni_line(").expect("the jni line"));
}

/// The drive resolves the parked report: `true` just before the initial
/// mark prints its `Concurrent Mark Cycle` line, `false` at every other
/// exit of the drive and after the forced door's drive decision.
///
/// The drive has three exits that each flush once: the starved-full
/// return (r23 w6, c8a6828262 -- the young pause reports before the
/// `Pause Full` it escalates to), the open-cycle return, and the tail.
#[test]
fn gci_i27ob_the_drive_resolves_the_parked_report() {
    let src = concat!(
        include_str!("../gc_and_alloc/zgc_refusal_line.rs"),
        "\n",
        include_str!("../gc_and_alloc/on_out_of_memory_error_commands.rs"),
        "\n",
        include_str!("../gc_and_alloc/frozen_peer_referents.rs")
    )
    .replace("\r\n", "\n");
    let initial = body(
        &src,
        "pub(in crate::runtime::interpreter) fn g1_concurrent_mark_cycle(",
    );
    let started = initial
        .find("g1_emit_deferred_young_report(shared, thread.thread_id, true);")
        .expect("the started report");
    let open = initial
        .find("non_collection_pause_finish(pause_timer);")
        .expect("the open line");
    let resume = initial
        .find("retire_skip_spans_and_resume(shared, taken);")
        .expect("the release");
    assert!(resume < started && started < open);
    let drive = body(
        &src,
        "pub(in crate::runtime::interpreter) fn g1_drive_mark_cycle(",
    );
    assert_eq!(
        drive
            .matches("g1_emit_deferred_young_report(shared, thread.thread_id, false);")
            .count(),
        3,
        "all three exits of the drive"
    );
    // The starved-full exit flushes BEFORE the full collection prints.
    let starved = drive
        .find("g1_emit_deferred_young_report(shared, thread.thread_id, false);")
        .expect("the starved-full flush");
    let full = drive
        .find("g1_starved_full(shared, thread)")
        .expect("the starved full");
    assert!(starved < full);
    let forced_drive = src
        .find("g1_drive_mark_cycle(shared, thread, MarkDoor::AllocFail);")
        .expect("the forced door's drive");
    let after = &src[forced_drive..];
    let flush = after
        .find("g1_emit_deferred_young_report(shared, thread.thread_id, false);")
        .expect("the forced door's flush");
    assert!(flush < after.find("\n\u{7d}\n").expect("its end"));
}

/// gci i27/ob (`gci-i15rb-...-review-findings-20261014` finding 4's
/// residual): JVMTI `ForceGarbageCollection` is HotSpot's
/// `collect(_jvmti_force_gc)` -- its own cause, a full collection on both
/// backends, the `System.gc()` VM operation, never a mark-cycle drive.
#[test]
fn gci_i27ob_jvmti_force_gc_has_hotspots_cause() {
    let door = GcDoor::JvmtiForceGc;
    assert_eq!(door.cause(), "JvmtiEnv ForceGarbageCollection");
    assert_eq!(g1_door_cause(door, true), "JvmtiEnv ForceGarbageCollection");
    assert_eq!(vm_operation_name(door, true), "G1CollectFull");
    assert_eq!(vm_operation_name(door, false), "SerialGCCollect");
    for explicit_concurrent in [false, true] {
        assert!(g1_door_is_a_pause_full(door, explicit_concurrent));
        assert!(jfr_old_event_rule(door, false, (true, false), explicit_concurrent));
        assert!(jfr_old_event_rule(door, false, (false, true), explicit_concurrent));
    }
    for alloc_drive in [false, true] {
        assert!(!g1_door_drives_mark_cycle(door, alloc_drive));
    }
    let src = concat!(
        include_str!("../gc_and_alloc/zgc_refusal_line.rs"),
        "\n",
        include_str!("../gc_and_alloc/on_out_of_memory_error_commands.rs"),
        "\n",
        include_str!("../gc_and_alloc/frozen_peer_referents.rs")
    )
    .replace("\r\n", "\n");
    let jvmti = body(&src, "pub fn force_gc_for_jvmti(");
    assert!(jvmti.contains("force_full_collection(shared, thread, GcDoor::JvmtiForceGc);"));
    assert!(!jvmti.contains("run_gc_notifications("));
}

/// gcg-g3x item 12: under the debug blocks the `gc,metaspace` figure at
/// each edge reuses the block's walk; the debug block is sampled before.
#[test]
fn gci_i27ob_one_metaspace_walk_per_edge() {
    let src = include_str!("../gc_events.rs").replace("\r\n", "\n");
    let start = body(&src, "pub(super) fn gc_event_start(");
    let sample = start
        .find("let heap_debug_before = if events && xlog_heap_debug_on()")
        .expect("the debug edge first");
    let reuse = start
        .find("metaspace_edge_bytes(shared, heap_debug_before.as_ref())")
        .expect("the before figure reuses it");
    assert!(sample < reuse);
    let lines = body(&src, "fn xlog_collection_lines(");
    // gci i30/ob: the after edge goes through `metaspace_after_bytes`,
    // which reuses the debug block's walk first.
    assert!(lines.contains("metaspace_after_bytes(shared, start)"));
    assert!(!lines.contains("metaspace_used_bytes(shared)"));
    let after = body(&src, "fn metaspace_after_bytes(");
    let debug = after
        .find("start.heap_debug_after.as_ref()")
        .expect("the debug block first");
    // gci i33/ms: the walk states HotSpot's statistics.
    let walk = after.find("metaspace_stats_now(shared)").expect("a walk last");
    assert!(debug < walk);
}
