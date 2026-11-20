use super::{root_kind_census_takes_door, GcDoor};
use crate::memory::root_kind_census::CensusMode;

#[test]
fn selected_mode_takes_the_failure_and_explicit_doors_only() {
    for door in [
        GcDoor::AllocationFailure,
        GcDoor::G1CompactionPause,
        GcDoor::G1HumongousAllocation,
        GcDoor::SystemGc,
        GcDoor::HeapInspection,
    ] {
        assert!(root_kind_census_takes_door(CensusMode::Selected, door));
        assert!(root_kind_census_takes_door(CensusMode::All, door));
        assert!(!root_kind_census_takes_door(CensusMode::Off, door));
    }
    assert!(!root_kind_census_takes_door(
        CensusMode::Selected,
        GcDoor::AllocationThreshold
    ));
    assert!(root_kind_census_takes_door(
        CensusMode::All,
        GcDoor::AllocationThreshold
    ));
}

/// The census is wired into the collection door before the collection and
/// into both G1 mark pauses before they mark.
#[test]
fn the_census_runs_before_each_pause_marks() {
    let src = super::GC_AND_ALLOC_SRC
    // A CRLF checkout (core.autocrlf) must not hide the "\n}\n" end.
    .replace("\r\n", "\n");
    let body = |sig: &str| {
        let at = src.find(sig).unwrap_or_else(|| panic!("{sig}"));
        let rest = &src[at..];
        rest[..rest.find("\n}\n").expect("end")].to_string()
    };
    let door = body("fn run_collection_pause(");
    let census = door.find("root_kind_census_for_pause(").expect("door census");
    let collect = door
        .find(".collect_garbage_with_finalizers(")
        .expect("the collection");
    assert!(census < collect);
    for (sig, mark) in [
        (
            "fn g1_stw_full_mark_cycle(",
            "shared.mem.heap.g1_mark_roots(&stw, &all_roots);",
        ),
        (
            "fn g1_final_remark_cleanup(",
            ".g1_final_remark_and_cleanup(&stw, &all_roots, Some(&mut process));",
        ),
    ] {
        let b = body(sig);
        let c = b.find("root_kind_census_for_g1_mark(").expect("mark census");
        let m = b.find(mark).expect("the mark");
        assert!(c < m, "{sig}: the census must read the roots before the mark");
        assert!(b.contains("root_kind_census_after_g1_cleanup("));
    }
}

/// JIT r23 w1b L7 (R23W1L7-4): the census also reads the roots of the G1
/// initial mark, the ZGC mark start and the two Generational concurrent
/// pauses before they hand them to their marker.
#[test]
fn r23w1b_the_census_reads_the_remaining_pauses_before_they_mark() {
    let src = super::GC_AND_ALLOC_SRC
    .replace("\r\n", "\n");
    let body = |sig: &str| {
        let at = src.find(sig).unwrap_or_else(|| panic!("{sig}"));
        let rest = &src[at..];
        rest[..rest.find("\n}\n").expect("end")].to_string()
    };
    for (sig, site, next) in [
        (
            "fn g1_concurrent_mark_cycle(",
            "\"g1-initial-mark\"",
            "shared.mem.heap.g1_mark_roots(&stw, &all_roots);",
        ),
        (
            "fn zgc_concurrent_mark_cycle(",
            "\"zgc-mark-start\"",
            "zgc_start_concurrent_mark(&stw, &all_roots)",
        ),
        (
            "fn maybe_concurrent_gc_at(",
            "\"gen-initial-mark\"",
            "let y2o = if y2o_live_seed {",
        ),
        (
            "fn gen_concurrent_remark_pause(",
            "\"gen-remark\"",
            "let y2o = if y2o_live_seed {",
        ),
        (
            "fn zgc_mark_end_attempt(",
            "\"zgc-mark-end\"",
            "heap.try_mark_end(&stw, &roots)",
        ),
    ] {
        let b = body(sig);
        let c = b.find(site).unwrap_or_else(|| panic!("{sig}: {site}"));
        let m = b.find(next).unwrap_or_else(|| panic!("{sig}: {next}"));
        assert!(c < m, "{sig}: the census must read the roots before {next}");
    }
}
