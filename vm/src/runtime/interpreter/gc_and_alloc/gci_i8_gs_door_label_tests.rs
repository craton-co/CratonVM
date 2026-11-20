//! gci i8/gs (`gci-i7orch-g1-young-sizing-and-door-label-review-findings`
//! item 1): occupancy verdicts taken through `maybe_gc_forced_at` run on
//! the `AllocationThreshold` door, and the overhead / census accounting is
//! unchanged.
use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use cratonvm_gc::gen_heap::oome_ladder as l;

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

/// The three occupancy sites, and only they, take the threshold door;
/// both allocation doors sample the sealed productivity figures, the
/// full-collection doors do not.
#[test]
fn gci_i8_gs_occupancy_sites_take_the_threshold_door() {
    for site in [
        "tlab-alloc-shaped",
        "jit-newarray-headroom",
        "jit-anewarray-headroom",
        "jit-newarray-zgc-headroom",
    ] {
        assert!(occupancy_door_site(site), "{site}");
        assert_eq!(forced_door_for(occupancy_door_site(site)), GcDoor::AllocationThreshold);
    }
    for site in ["unlabelled", "vm-exec", "jit-helpers", "tlab-alloc", ""] {
        assert!(!occupancy_door_site(site), "{site}");
        assert_eq!(forced_door_for(occupancy_door_site(site)), GcDoor::AllocationFailure);
    }
    assert!(door_samples_productivity(GcDoor::AllocationFailure));
    assert!(door_samples_productivity(GcDoor::AllocationThreshold));
    assert!(!door_samples_productivity(GcDoor::SystemGc));
    assert!(!door_samples_productivity(GcDoor::MetadataThreshold));
    assert!(!door_samples_productivity(GcDoor::DiagnosticCommand));
    // HotSpot's cause string is the same on both allocation doors.
    assert_eq!(
        GcDoor::AllocationThreshold.cause(),
        GcDoor::AllocationFailure.cause()
    );
}

/// The wiring: the forced door picks its door through `forced_door_for`,
/// and the shared pause samples through `door_samples_productivity` --
/// without the second half an occupancy pause would be judged on figures
/// read after the release.
#[test]
fn gci_i8_gs_the_forced_door_and_the_sample_are_wired() {
    let src = source();
    // gci i10/dl: the body moved to `maybe_gc_forced_collected_on` (a
    // full-collection requester's door wins, `forced_pause_door`).
    // Merge of origin/dev (2026-10-15): and on to
    // `maybe_gc_forced_collected_stamped_on` (zr z3/ja's stamp).
    let forced = body(&src, "fn maybe_gc_forced_collected_stamped_on(");
    assert!(forced.contains("let occupancy_door = occupancy_door_site(site);"));
    // gci i11/go: through `forced_site_door` (G1's recovery compaction).
    assert!(forced.contains("let door = forced_site_door(site, occupancy_door, requester);"));
    assert!(forced.contains("run_collection_pause(shared, thread, door)"));
    assert!(!forced.contains("GcDoor::AllocationFailure)"));
    assert_eq!(forced_pause_door(true, None), forced_door_for(true));
    assert_eq!(forced_pause_door(false, None), forced_door_for(false));
    let pause = body(&src, "fn run_collection_pause(");
    assert!(pause.contains("door_samples_productivity(door).then(|| SealedProductivity::sample(shared))"));
}

/// gci i20/mr finding 1: on ZGC every forced pause runs on dev's
/// `AllocationFailure` door (the occupancy sites included); Generational
/// and G1 keep `forced_site_door`'s answer. Wired after `forced_site_door`.
#[test]
fn gci_i20mr_zgc_forced_pauses_keep_the_failure_door() {
    for site in [
        "tlab-alloc-shaped",
        "jit-newarray-zgc-headroom",
        "jit-helpers",
        "vm-exec",
        "unlabelled",
    ] {
        let door = forced_site_door(site, occupancy_door_site(site), None);
        assert_eq!(forced_door_on_heap(true, door), GcDoor::AllocationFailure, "{site}");
        assert_eq!(forced_door_on_heap(false, door), door, "{site}");
    }
    assert_eq!(
        forced_door_on_heap(false, forced_site_door("tlab-alloc-shaped", true, None)),
        GcDoor::AllocationThreshold
    );
    let src = source();
    let forced = body(&src, "fn maybe_gc_forced_collected_stamped_on(");
    let site_door = forced
        .find("let door = forced_site_door(site, occupancy_door, requester);")
        .expect("the site door");
    let heap_door = forced.find("let door = forced_door_on_heap(").expect("the heap door");
    let pause = forced
        .find("run_collection_pause(shared, thread, door)")
        .expect("the pause");
    assert!(site_door < heap_door && heap_door < pause);
}

/// Generational: the census is what it was -- an unlabelled forced
/// collection counts as FORCED, the compiled refill trigger's occupancy
/// collection does not (and is not wedged on an empty heap).
#[test]
fn gci_i8_gs_generational_census_is_unchanged() {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    let census = |s: &SharedVm| match &s.mem.heap {
        _ if s.mem.heap.is_generational() => s.mem.heap.gen_oome_ladder_census().unwrap(),
        _ => [0; l::LEN],
    };
    let mut thread = JvmThread::new(ThreadId(0), "i8gs-door");
    maybe_gc_forced_at(&shared, &mut thread, "tlab-alloc-shaped");
    let c = census(&shared);
    assert_eq!(c[l::FORCED], 0, "an occupancy verdict is not forced: {c:?}");
    assert_eq!(c[l::THRESHOLD_WEDGED], 0, "an empty heap is not wedged: {c:?}");
    maybe_gc_forced_at(&shared, &mut thread, "unlabelled");
    let c = census(&shared);
    assert_eq!(c[l::FORCED], 1, "{c:?}");
}
