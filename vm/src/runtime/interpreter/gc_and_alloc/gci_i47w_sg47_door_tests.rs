//! gci i47w/sg47 (`CRATONVM_GC_SYSTEM_GC_TENURES_ALL`, default ON since the 2026-10-17 flip): the
//! `System.gc()` door arms the tenure-all request with its major request,
//! inside the attempt loop and before the pause, and withdraws it once the
//! loop ends, before G1's marking-cycle and compaction pauses.

fn source() -> String {
    super::GC_AND_ALLOC_SRC.replace("\r\n", "\n")
}

/// The item starting at `sig` at the start of a line, up to the next
/// column-0 closing brace (`\u{7d}`).
fn body(src: &str, sig: &str) -> String {
    let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &src[start..];
    let stop = rest
        .find("\n\u{7d}\n")
        .unwrap_or_else(|| panic!("end of `{sig}` not found"));
    rest[..stop].to_string()
}

#[test]
fn gci_i47w_sg47_the_door_arms_and_withdraws_the_tenure_request() {
    let src = source();
    let f = body(&src, "fn force_full_collection(");
    let first = f.find("for _attempt in 0..FORCE_GC_ATTEMPTS").expect("the loop");
    let major = f
        .find("cratonvm_gc::gc_quiescence::request_major_gc();")
        .expect("the major request");
    let arm = f
        .find("cratonvm_gc::gc_quiescence::request_system_gc_tenure();")
        .expect("the tenure request");
    let pause = f
        .find("run_collection_pause(shared, thread, door)")
        .expect("the pause");
    let take = f
        .find("let _ = cratonvm_gc::gc_quiescence::take_system_gc_tenure_request();")
        .expect("the withdrawal");
    let cycle = f
        .find("g1_force_full_cycle(shared, thread);")
        .expect("the G1 cycle");
    let compaction = f
        .find("g1_full_compaction_pause(shared, thread, door);")
        .expect("the compaction");
    assert!(first < major && major < arm && arm < pause && pause < take);
    assert!(take < cycle && take < compaction);
    assert_eq!(
        f.matches("request_system_gc_tenure()").count(),
        1,
        "armed at one site only"
    );
}

/// No other door arms it, but for the G1 full collections HotSpot runs
/// in place of a young pause: the allocation ladder's last ditch
/// (`CRATONVM_G1_LAST_DITCH_TENURES_ALL`, the near-full proxy-define page)
/// and the starved full (`CRATONVM_G1_STARVED_FULL`, r23 w6). The ladder's
/// other majors keep their young behaviour.
#[test]
fn gci_i47w_sg47_only_the_system_gc_door_arms_the_tenure_request() {
    let src = source();
    let code = src.as_str();
    let needle = ["request_system_gc_", "tenure();"].concat();
    assert_eq!(code.matches(needle.as_str()).count(), 3, "{needle}");
    for sig in [
        "fn force_full_collection(",
        "fn with_last_ditch_tenure<R>(",
        "fn g1_starved_full(",
    ] {
        assert_eq!(body(code, sig).matches(needle.as_str()).count(), 1, "{sig}");
    }
}
