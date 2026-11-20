//! zt t18/zc1 (`CRATONVM_ZGC_SYSTEM_GC_CONCURRENT`): the wiring of
//! `zgc_system_gc_runs_concurrent_cycle`. The heap half (ownership, the
//! held mark end, the owned cycle closed with its mark set) is unit-tested
//! in `gc/src/zgc.rs` (`zt18zc1_*`); the behaviour is the
//! `Zt18Zc1PauseTicker` / `Zt1MkSystemGcFresh` rows of
//! `docs/feature-designs/zgc-architecture.md`.

fn source() -> String {
    super::GC_AND_ALLOC_SRC.replace("\r\n", "\n")
}

fn body(src: &str, sig: &str) -> String {
    let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &src[start..];
    let stop = rest
        .find("\n\u{7d}\n")
        .unwrap_or_else(|| panic!("end of `{sig}` not found"));
    rest[..stop].to_string()
}

/// The cycle opens AFTER an older open cycle was closed (so it starts
/// after the call) and BEFORE the requested pause and its major request;
/// the helper only asks, opens, books and waits -- the caller's loop runs
/// the collection that closes the cycle.
#[test]
fn zt18zc1_the_request_opens_its_cycle_before_the_requested_pause() {
    let src = source();
    let door = body(&src, "fn force_full_collection(");
    let close = door
        .find("zgc_full_request_closes_open_cycle(shared, thread);")
        .expect("the door closes an older cycle");
    let open = door
        .find("zgc_system_gc_runs_concurrent_cycle(shared, thread, door);")
        .expect("the door opens the request's cycle");
    let first = door
        .find("for _attempt in 0..FORCE_GC_ATTEMPTS")
        .expect("the loop");
    let major = door
        .find("cratonvm_gc::gc_quiescence::request_major_gc();")
        .expect("the major request");
    assert!(
        close < open && open < first && open < major,
        "{close} {open} {first} {major}"
    );
    let helper = body(&src, "fn zgc_system_gc_runs_concurrent_cycle(");
    let doors = helper
        .find("GcDoor::JvmtiForceGc\n            | GcDoor::MetadataThreshold") // literal-continuation-ok: a source witness or a HotSpot log table, spaces are the text
        .expect("the doors HotSpot's ZGC answers with a cycle");
    assert!(
        !helper.contains("GcDoor::HeapDump") && !helper.contains("GcDoor::HeapInspection"),
        "the heap dump and inspection doors keep the stop-the-world collection"
    );
    let request = helper
        .find("zgc_request_system_gc_concurrent_cycle()")
        .expect("the heap records the request and its ownership");
    let start = helper
        .find("zgc_concurrent_mark_cycle(shared, thread);")
        .expect("the every-trigger mark start");
    let cause = helper
        .find("super::gc_events::with_zgc_mark_start_door(door, ||")
        .expect("the mark start carries the requester's cause");
    assert!(cause < start);
    let note = helper
        .find("zgc_note_system_gc_concurrent_cycle(opened)")
        .expect("the outcome is booked");
    let wait = helper
        .find("zgc_concurrent_mark_caught_up()")
        .expect("the wait for the fixed point");
    let poll = helper
        .find("safepoint_check(shared, thread);")
        .expect("the wait takes part in pauses");
    assert!(doors < request && request < start && start < note && note < wait && wait < poll);
    assert!(
        !helper.contains("request_major_gc"),
        "the mark start is not a full request"
    );
    assert!(
        !helper.contains("run_collection_pause"),
        "the caller's loop runs the collection"
    );
}
