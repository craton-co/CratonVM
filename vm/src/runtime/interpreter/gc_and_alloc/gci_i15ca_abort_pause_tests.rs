//! gci i15/ca: the full collection's abort of a marked open G1 cycle, in
//! the source (a unit VM cannot run a marking cycle). Filter:
//! `cargo test -p cratonvm-vm gci_i15ca`.

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

/// The decision is taken once, from the report half's mark and only with
/// a fresh cycle due; inside the loop the abort is dropped as soon as no
/// cycle marks (so this call's own fresh cycle is never aborted), and it
/// is tried before the remark arm.
#[test]
fn gci_i15ca_the_full_cycle_aborts_a_marked_open_cycle_instead_of_finishing_it() {
    let src = source();
    let c = body(&src, "fn g1_force_full_cycle_with(");
    let decide = c.find("let mut abort_open =").expect("decided once");
    assert!(c.contains(
        "need_fresh && super::gc_events::g1_open_cycle_marked_aborted(shared).is_some()"
    ));
    let looped = c.find("\n    loop ").expect("the loop");
    let dropped = c
        .find("if abort_open && !shared.mem.heap.g1_is_marking_active()")
        .expect("dropped once nothing marks");
    let tried = c
        .find("if abort_open && shared.mem.heap.g1_is_marking_active()")
        .expect("the abort arm");
    let abort = c
        .find("g1_abort_open_cycle_pause(shared, thread)")
        .expect("the abort pause");
    let remark = c
        .find("g1_final_remark_cleanup(shared, thread)")
        .expect("the remark arm");
    assert!(decide < looped && looped < dropped && dropped < tried);
    assert!(tried < abort && abort < remark);
    assert_eq!(c.matches("g1_abort_open_cycle_pause(").count(), 1);
}

/// The abort pause is the remark's protocol without its work: request,
/// guard, take-over, the abort mark before the remark-kind timer, the
/// heap's abort, the cut, seal, resume, release, finish. It gathers no
/// root, processes no reference and sweeps nothing.
#[test]
fn gci_i15ca_the_abort_pause_is_the_remark_protocol_without_its_work() {
    let src = source();
    let b = body(&src, "fn g1_abort_open_cycle_pause(");
    let first = |needle: &str| b.find(needle).unwrap_or_else(|| panic!("`{needle}` missing"));
    let last = |needle: &str| b.rfind(needle).unwrap_or_else(|| panic!("`{needle}` missing"));
    let order = [
        first("request_non_collection_pause(shared, thread.thread_id)?"),
        first("GenConcPauseGuard::arm(shared)"),
        first("stw_take_over_and_wait("),
        first("g1_mark_open_cycle_aborted(shared)"),
        first("non_collection_pause_start(shared, NonCollectionPause::G1Remark)"),
        first("g1_abort_concurrent_mark(&stw)"),
        first("non_collection_pause_cleanup_point("),
        first("non_collection_pause_seal(&mut pause_timer)"),
        last("retire_skip_spans_and_resume(shared, taken)"),
        last(".complete_gc(cratonvm_types::PointerMap::default())"),
        first("non_collection_pause_finish(pause_timer)"),
    ];
    for pair in order.windows(2) {
        assert!(pair[0] < pair[1], "{order:?}");
    }
    for absent in [
        "collect_roots(",
        "g1_final_remark_and_cleanup(",
        "g1_remark_process_references(",
        "sweep_address_keyed_tables(",
        "flush_remark_reference_lines(",
    ] {
        assert!(!b.contains(absent), "the abort pause must not run `{absent}`");
    }
}

/// `System.gc()`: after its report half marked the open cycle, the
/// cycle tick (which on G1 with a cycle open can only run its remark) is
/// skipped while that cycle still marks, so `g1_force_full_cycle` aborts
/// it instead.
#[test]
fn gci_i15ca_system_gc_skips_the_tick_only_for_a_marked_open_cycle() {
    let src = source();
    let f = body(&src, "fn force_full_collection(");
    let mark = f
        .find("g1_full_gc_abort_open_cycle(shared, door)")
        .expect("the abort mark");
    let pending = f
        .find("let abort_pending = shared.mem.heap.g1_is_marking_active()")
        .expect("the gate");
    let tick = f
        .find("maybe_concurrent_gc_at(shared, thread, MarkDoor::SystemGc)")
        .expect("the tick");
    let cycle = f
        .find("g1_force_full_cycle(shared, thread);")
        .expect("the cycle");
    assert!(mark < pending && pending < tick && tick < cycle);
    assert!(f.contains("&& super::gc_events::g1_open_cycle_marked_aborted(shared).is_some();"));
    assert!(f.contains("if !abort_pending"));
}
