//! zt t3/mk (X1-9): the wiring of `zgc_full_request_closes_open_cycle`.
//! The behaviour (two collections, HotSpot's count) is the
//! `Zt1MkSystemGcFresh` probe's `collections_per_gc=` line; a VM-level
//! test would need a concurrent cycle open across a `System.gc()`.

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

/// The closing pause runs BEFORE the requested one (and before the major
/// request is armed, so it cannot consume it), on the allocation door,
/// only with a ZGC cycle open.
#[test]
fn zt3mk_the_open_cycle_closes_before_the_requested_pause() {
    let src = source();
    let door = body(&src, "fn force_full_collection(");
    let close = door
        .find("zgc_full_request_closes_open_cycle(shared, thread);")
        .expect("the door closes an open cycle");
    let first = door.find("for _attempt in 0..FORCE_GC_ATTEMPTS").expect("the loop");
    let major = door
        .find("cratonvm_gc::gc_quiescence::request_major_gc();")
        .expect("the major request");
    assert!(close < first && close < major, "{close} {first} {major}");
    let helper = body(&src, "fn zgc_full_request_closes_open_cycle(");
    let gate = helper
        .find("shared.mem.heap.zgc_concurrent_mark_active()")
        .expect("ZGC with a cycle open only");
    let pause = helper
        .find("run_collection_pause(shared, thread, GcDoor::AllocationThreshold)")
        .expect("the allocation door");
    assert!(gate < pause);
    assert!(!helper.contains("request_major_gc"), "the closing pause is not a full request");
}
