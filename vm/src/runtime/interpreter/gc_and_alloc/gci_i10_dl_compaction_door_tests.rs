//! gci i10/dl (`gci-i9dc-g1-full-collection-compaction-pause-reports-allocation-failure-FIXED-20261017`):
//! G1's full-collection compaction pause runs on the requester's door,
//! not on `AllocationFailure`.
use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use cratonvm_gc::gen_heap::oome_ladder as l;
use std::sync::atomic::Ordering;

fn source() -> String {
    super::GC_AND_ALLOC_SRC.replace("\r\n", "\n")
}

/// The item starting at `sig` at the start of a line, up to the next
/// column-0 closing brace (`\u{7d}`, see `w5a_pause_driver_tests::body`).
fn body(src: &str, sig: &str) -> String {
    let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &src[start..];
    let stop = rest
        .find("\n\u{7d}\n")
        .unwrap_or_else(|| panic!("end of `{sig}` not found"));
    rest[..stop].to_string()
}

/// A requester's door wins over the allocation doors; without one the
/// i8/gs choice stands. Every full-collection door keeps its own cause
/// and none of them is judged for the overhead streak.
#[test]
fn gci_i10_dl_the_requester_door_wins() {
    for door in [
        GcDoor::SystemGc,
        GcDoor::MetadataThreshold,
        GcDoor::DiagnosticCommand,
        GcDoor::HeapInspection,
        GcDoor::HeapDump,
    ] {
        for occupancy in [false, true] {
            assert_eq!(forced_pause_door(occupancy, Some(door)), door);
        }
        assert_ne!(door.cause(), GcDoor::AllocationFailure.cause(), "{door:?}");
        assert!(!door_samples_productivity(door), "{door:?}");
    }
    assert_eq!(forced_pause_door(false, None), GcDoor::AllocationFailure);
    assert_eq!(forced_pause_door(true, None), GcDoor::AllocationThreshold);
}

/// The wiring: `force_full_collection` hands its door to the compaction
/// pause, which runs it through `maybe_gc_forced_collected_on`; the
/// last-ditch recovery compaction (an allocation failure, no requester)
/// runs on `G1CompactionPause` since gci i11/go
/// (`gci_i11_go_compaction_door_tests`).
#[test]
fn gci_i10_dl_the_compaction_pause_is_wired_to_the_requester() {
    let src = source();
    let full = body(&src, "fn force_full_collection(");
    assert!(full.contains("g1_full_compaction_pause(shared, thread, door);"));
    let compaction = body(&src, "fn g1_full_compaction_pause(");
    assert!(compaction.contains(
        "maybe_gc_forced_collected_on(shared, thread, \"g1-full-compaction\", Some(door))"
    ));
    assert!(!compaction.contains("maybe_gc_forced_at("));
    let forced = body(&src, "fn maybe_gc_forced_collected(");
    assert!(forced.contains("maybe_gc_forced_collected_on(shared, thread, site, None)"));
    let on = body(&src, "fn maybe_gc_forced_collected_on(");
    assert!(on.contains(
        "maybe_gc_forced_collected_stamped_on(shared, thread, site, None, requester)"
    ));
    let on = body(&src, "fn maybe_gc_forced_collected_stamped_on(");
    assert!(on.contains("pause.filter(|_| door_samples_productivity(door))"));
}

/// Behaviour, on a Generational VM (the function is backend-agnostic): a
/// requester's pause is a full-collection-door pause (it bumps the
/// `System.gc()` coalescing count, as `run_collection_pause` does for
/// every full door) and is not a forced ladder rung; an allocation caller
/// is the opposite.
#[test]
fn gci_i10_dl_a_requester_pause_counts_as_its_door() {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    let census = |s: &SharedVm| match &s.mem.heap {
        _ if s.mem.heap.is_generational() => s.mem.heap.gen_oome_ladder_census().unwrap(),
        _ => [0; l::LEN],
    };
    let full_count = |s: &SharedVm| system_gc_collections(s).load(Ordering::Acquire);
    let mut thread = JvmThread::new(ThreadId(0), "i10dl-door");

    let before = full_count(&shared);
    assert!(maybe_gc_forced_collected_on(
        &shared,
        &mut thread,
        "g1-full-compaction",
        Some(GcDoor::SystemGc)
    ));
    assert_eq!(full_count(&shared), before + 1, "a System.gc() door pause");
    assert_eq!(census(&shared)[l::FORCED], 0, "not an allocation-ladder rung");

    assert!(maybe_gc_forced_collected(&shared, &mut thread, "unlabelled"));
    assert_eq!(full_count(&shared), before + 1, "an allocation pause is not a full door");
    assert_eq!(census(&shared)[l::FORCED], 1);
}

/// gci i41/rs (`gci-i39rv-waves-i35-i38-review-residuals-FIXED-20261016` item
/// 1): the stamp seam. A pause takes the full door's Survivor stamp only
/// when it ran a full compaction, on a full-collection door, for a
/// stop-the-world cycle, with the switch on; the switch is not read for
/// any other pause.
#[test]
fn gci_i41rs_the_door_compaction_pause_stamp_seam() {
    use crate::config::ExplicitGcPolicy as P;
    let full_doors = [
        GcDoor::SystemGc,
        GcDoor::MetadataThreshold,
        GcDoor::DiagnosticCommand,
        GcDoor::HeapInspection,
        GcDoor::HeapDump,
        GcDoor::JvmtiForceGc,
    ];
    let alloc_doors = [
        GcDoor::AllocationThreshold,
        GcDoor::AllocationFailure,
        GcDoor::G1CompactionPause,
        GcDoor::G1HumongousAllocation,
    ];
    for door in full_doors {
        assert!(g1_full_collection_door(door), "{door:?}");
        assert!(g1_door_compaction_pause_stamps_survivor(true, door, P::Full, || true));
        assert!(!g1_door_compaction_pause_stamps_survivor(true, door, P::Full, || false));
        // No compaction in this pause: the switch is not even read.
        assert!(!g1_door_compaction_pause_stamps_survivor(false, door, P::Full, || -> bool {
            panic!("read for a pause without a compaction")
        }));
    }
    for door in alloc_doors {
        assert!(!g1_full_collection_door(door), "{door:?}");
        // A request an allocation pause consumed: no stamp, no read.
        assert!(!g1_door_compaction_pause_stamps_survivor(true, door, P::Full, || -> bool {
            panic!("read for an allocation door")
        }));
    }
    // `-XX:+ExplicitGCInvokesConcurrent`: System.gc() and jcmd GC.run run
    // the concurrent cycle (HotSpot's concurrent-start young pause stamps
    // the Survivor pool as ours does); the other full doors stay STW.
    for door in [GcDoor::SystemGc, GcDoor::DiagnosticCommand] {
        assert!(!g1_door_compaction_pause_stamps_survivor(true, door, P::Concurrent, || true));
    }
    assert!(g1_door_compaction_pause_stamps_survivor(
        true,
        GcDoor::MetadataThreshold,
        P::Concurrent,
        || true
    ));
    // The count moved only when it moved, and never off G1.
    assert!(full_compaction_count_moved(Some(3), Some(4)));
    assert!(!full_compaction_count_moved(Some(3), Some(3)));
    assert!(!full_compaction_count_moved(None, None));
    assert!(!full_compaction_count_moved(None, Some(1)));
    // Off G1 nothing moves.
    let gen = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    assert_eq!(g1_full_compaction_pause_count(&gen), None);
    assert!(!g1_full_compactions_since(&gen, g1_full_compaction_pause_count(&gen)));
    // The door end calls the old stamp only under the `=0` kill.
    let src = source();
    let full = body(&src, "fn force_full_collection(");
    assert!(full.contains("&& !g1_full_door_survivor_stamp_in_pause_enabled()"));
}
