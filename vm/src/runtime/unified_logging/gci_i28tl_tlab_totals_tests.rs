use super::*;

#[test]
fn gci_i28tl_the_line_carries_the_gc_id_prefix() {
    assert_eq!(
        tlab_totals_line(7, "TLAB totals: thrds: 1  refills: 2 max: 2"),
        "GC(7) TLAB totals: thrds: 1  refills: 2 max: 2"
    );
}

/// A collection that does not want the line clears a stale stash, so it
/// never prints under a later collection's id.
#[test]
fn gci_i28tl_an_unwanted_collection_clears_the_stash() {
    let vm = crate::vm::SharedVm::new(crate::config::VmConfig {
        gc_algorithm: crate::config::GcAlgorithm::Generational,
        max_heap_size: 64 * 1024 * 1024,
        ..Default::default()
    });
    let state = &vm.mem.tlab_wedge.tlab_totals;
    state.stash(Some("stale".to_string()));
    stash_tlab_totals(&vm, false);
    assert_eq!(state.take(), None);
    assert!(!tlab_totals_wanted(&vm, false));
}

/// Computed at the collection's start (`gc_event_start`), printed after
/// the before blocks and before the deferred reference lines, on both the
/// plain and the escalated-split writer. gci i30/ob: taken into the start
/// record at once, and printed from it.
#[test]
fn gci_i28tl_the_line_is_stashed_at_the_start_and_printed_after_heap_before() {
    const SRC: &str = include_str!("../interpreter/gc_events.rs");
    let start = SRC.find("pub(super) fn gc_event_start(").expect("gc_event_start");
    let stash = SRC[start..]
        .find("stash_tlab_totals(shared, events)")
        .expect("stashed at the start");
    let take = SRC[start..]
        .find("take_tlab_totals(shared)")
        .expect("taken into the start record");
    let literal = SRC[start..].find("Some(GcEventStart").expect("the start record");
    assert!(stash < take && take < literal);
    let writer = SRC.find("\nfn xlog_collection_lines(").expect("the writer");
    let body = &SRC[writer..];
    let g1_before = body
        .find("start.g1_heap_debug_before.as_ref(),")
        .expect("G1 before blocks");
    let log = body
        .find("log_tlab_totals(start.tlab_totals.as_deref(), gc_id)")
        .expect("printed");
    let refs = body.find("flush_deferred_gc_lines(gc_id)").expect("ref lines");
    assert!(g1_before < log && log < refs);
    let split = SRC
        .find("\nfn xlog_escalated_collection_lines(")
        .expect("the split writer");
    let body = &SRC[split..];
    let log = body
        .find("log_tlab_totals(start.tlab_totals.as_deref(), young_id)")
        .expect("young half");
    let refs = body
        .find("flush_deferred_gc_lines(young_id)")
        .expect("ref lines");
    assert!(log < refs);
}
