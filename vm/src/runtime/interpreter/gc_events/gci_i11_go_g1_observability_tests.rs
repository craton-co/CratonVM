//! gci i11/go: G1's `-Xlog:gc` / JFR / notification names
//! (`gci-i10rh-g1-humongous-rset-and-mixed-choice-review-findings-FIXED-20261016`
//! findings 4 and 5, `gci-i10dl-g1-full-collection-door-residuals-FIXED-20261016` (a)).
use super::*;

const M: usize = 1024 * 1024;

/// HotSpot 25 G1 never says `Allocation Failure`: both allocation doors
/// are `G1 Evacuation Pause` on G1 and keep Serial's spelling elsewhere;
/// every requester's door keeps its cause on both; the recovery
/// compaction is `G1 Compaction Pause`.
#[test]
fn gci_i11_go_g1_causes_are_hotspots() {
    for door in [GcDoor::AllocationThreshold, GcDoor::AllocationFailure] {
        assert_eq!(g1_door_cause(door, true), "G1 Evacuation Pause");
        assert_eq!(g1_door_cause(door, false), "Allocation Failure");
    }
    for door in [
        GcDoor::SystemGc,
        GcDoor::MetadataThreshold,
        GcDoor::DiagnosticCommand,
        GcDoor::HeapInspection,
        GcDoor::HeapDump,
    ] {
        assert_eq!(g1_door_cause(door, true), door.cause(), "{door:?}");
        assert_eq!(g1_door_cause(door, false), door.cause(), "{door:?}");
    }
    assert_eq!(GcDoor::G1CompactionPause.cause(), "G1 Compaction Pause");
    assert_eq!(
        g1_door_cause(GcDoor::G1HumongousAllocation, true),
        "G1 Humongous Allocation"
    );
    assert_eq!(
        g1_door_cause(GcDoor::G1CompactionPause, true),
        "G1 Compaction Pause"
    );
}

/// `Pause Young (Normal)` unless the mixed count moved; the `-Xlog:gc`
/// line then reads as HotSpot 25 G1's.
#[test]
fn gci_i11_go_g1_pause_labels() {
    assert_eq!(g1_young_pause_type(3, 3, false, false), G1YoungPauseType::Normal);
    assert_eq!(g1_young_pause_type(3, 4, false, false), G1YoungPauseType::Mixed);
    // Precedence: Mixed > Prepare Mixed > Concurrent Start > Normal.
    assert_eq!(g1_young_pause_type(3, 4, true, true), G1YoungPauseType::Mixed);
    assert_eq!(g1_young_pause_type(3, 3, true, true), G1YoungPauseType::PrepareMixed);
    assert_eq!(
        g1_young_pause_type(3, 3, false, true),
        G1YoungPauseType::ConcurrentStart
    );
    assert_eq!(
        G1YoungPauseType::ConcurrentStart.xlog_label(),
        "Pause Young (Concurrent Start)"
    );
    assert_eq!(
        G1YoungPauseType::PrepareMixed.xlog_label(),
        "Pause Young (Prepare Mixed)"
    );
    for (t, j) in [
        (G1YoungPauseType::Normal, "Normal"),
        (G1YoungPauseType::ConcurrentStart, "Concurrent Start"),
        (G1YoungPauseType::PrepareMixed, "Prepare Mixed"),
        (G1YoungPauseType::Mixed, "Mixed"),
    ] {
        assert_eq!(t.jfr_type(), j);
        assert_eq!(t.xlog_label(), format!("Pause Young ({j})"));
    }
    // Only the allocation doors drive a cycle; the forced ones only under
    // `CRATONVM_G1_ALLOC_MARK_DRIVE`.
    assert!(g1_door_drives_mark_cycle(GcDoor::AllocationThreshold, false));
    for door in [
        GcDoor::AllocationFailure,
        GcDoor::G1CompactionPause,
        GcDoor::G1HumongousAllocation,
    ] {
        assert!(g1_door_drives_mark_cycle(door, true), "{door:?}");
        assert!(!g1_door_drives_mark_cycle(door, false), "{door:?}");
    }
    for door in [
        GcDoor::SystemGc,
        GcDoor::MetadataThreshold,
        GcDoor::DiagnosticCommand,
        GcDoor::HeapInspection,
        GcDoor::HeapDump,
    ] {
        assert!(!g1_door_drives_mark_cycle(door, true), "{door:?}");
    }
    assert_eq!(G1YoungPauseType::Normal.xlog_label(), "Pause Young (Normal)");
    assert_eq!(G1YoungPauseType::Mixed.xlog_label(), "Pause Young (Mixed)");
    let line = render_xlog_gc_line(
        5,
        G1YoungPauseType::Normal.xlog_label(),
        g1_door_cause(GcDoor::AllocationFailure, true),
        24 * M + 5,
        3 * M,
        256 * M,
        2_345_000,
    );
    assert_eq!(
        line,
        "GC(5) Pause Young (Normal) (G1 Evacuation Pause) 24M->3M(256M) 2.345ms"
    );
    let mixed = render_xlog_gc_line(
        9,
        G1YoungPauseType::Mixed.xlog_label(),
        g1_door_cause(GcDoor::AllocationThreshold, true),
        60 * M,
        20 * M,
        256 * M,
        7_000_000,
    );
    assert_eq!(
        mixed,
        "GC(9) Pause Young (Mixed) (G1 Evacuation Pause) 60M->20M(256M) 7.000ms"
    );
}

/// HotSpot G1's four `gc,heap` lines: counts at both edges, Eden's
/// capacity the target after minus the survivors after, Survivor's
/// `ceil(target before / 8)`, none for Old and Humongous.
#[test]
fn gci_i11_go_g1_heap_lines_have_hotspots_shape() {
    let census = |eden: usize, survivor: usize, old: usize, humongous: usize| {
        cratonvm_gc::g1::G1RegionCensus {
            eden: (eden * M, eden),
            survivor: (survivor * M, survivor),
            old: (old * M, old),
            humongous: (humongous * M, humongous),
            free: 200,
            region_size: M,
        }
    };
    let before = G1RegionEdge {
        census: census(25, 0, 0, 2),
        young_target_regions: 25,
        survivor_ratio: G1_SURVIVOR_RATIO,
    };
    let after = G1RegionEdge {
        census: census(0, 3, 2, 1),
        young_target_regions: 25,
        survivor_ratio: G1_SURVIVOR_RATIO,
    };
    assert_eq!(
        render_xlog_g1_heap_lines(0, before, after),
        [
            "GC(0) Eden regions: 25->0(22)".to_string(),
            "GC(0) Survivor regions: 0->3(4)".to_string(),
            "GC(0) Old regions: 0->2".to_string(),
            "GC(0) Humongous regions: 2->1".to_string(),
        ]
    );
    // A target smaller than the survivors after never underflows.
    let tiny = G1RegionEdge {
        census: census(0, 3, 2, 1),
        young_target_regions: 1,
        survivor_ratio: G1_SURVIVOR_RATIO,
    };
    let lines = render_xlog_g1_heap_lines(1, tiny, tiny);
    assert_eq!(lines[0], "GC(1) Eden regions: 0->0(0)");
    assert_eq!(lines[1], "GC(1) Survivor regions: 3->3(1)");
}

/// `jdk.ExecuteVMOperation` names: HotSpot 25's per backend and door.
#[test]
fn gci_i11_go_vm_operation_names() {
    for door in [
        GcDoor::AllocationThreshold,
        GcDoor::AllocationFailure,
        GcDoor::G1CompactionPause,
        GcDoor::G1HumongousAllocation,
    ] {
        assert_eq!(vm_operation_name(door, true), "G1CollectForAllocation");
        assert_eq!(vm_operation_name(door, false), "SerialCollectForAllocation");
    }
    for door in [GcDoor::SystemGc, GcDoor::DiagnosticCommand] {
        assert_eq!(vm_operation_name(door, true), "G1CollectFull");
        assert_eq!(vm_operation_name(door, false), "SerialGCCollect");
    }
    for g1 in [true, false] {
        assert_eq!(
            vm_operation_name(GcDoor::MetadataThreshold, g1),
            "CollectForMetadataAllocation"
        );
        assert_eq!(vm_operation_name(GcDoor::HeapInspection, g1), "GC_HeapInspection");
        assert_eq!(vm_operation_name(GcDoor::HeapDump, g1), "HeapDumper");
    }
}

/// The recovery compaction has its own space-free `door=` token, distinct
/// from every other door's.
#[test]
fn gci_i11_go_compaction_door_token() {
    let facts = PauseSafepointFacts {
        ttsp_ns: 0,
        straggler: None,
        xt_post_quota_ns: 0,
        xt_pass_ns: 0,
        xt: (0, 0, 0, 0, 0, 0),
    };
    let line = render_pause_line(2, GcDoor::G1CompactionPause, facts, 0);
    assert!(line.contains(" door=g1-compaction "), "{line}");
    assert!(!render_pause_line(2, GcDoor::AllocationFailure, facts, 0)
        .contains("g1-compaction"));
}
