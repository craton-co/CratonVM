fn source() -> String {
    super::GC_AND_ALLOC_SRC.replace("\r\n", "\n")
}

/// The item starting at `sig` at the START of a line (so the string
/// literals in this module never match) up to the next column-0 `}`.
fn body(src: &str, sig: &str) -> String {
    let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &src[start..];
    // `\u{7d}` is `}`: a bare one in a literal would end this module early
    // for the production-panic scanner, which counts raw braces.
    let stop = rest
        .find("\n\u{7d}\n")
        .unwrap_or_else(|| panic!("end of `{sig}` not found"));
    rest[..stop].to_string()
}

/// `handoff-w4b-takeover-publishes-the-one-thread-field-list`: the frozen
/// peer's JvmThread FIELD roots come from the one per-thread list, not
/// from a hand-written partial copy (which missed `native_alloc_pool`,
/// `handle_slots`, the parked throwables, the mirror, ...).
#[test]
fn the_frozen_peer_publisher_uses_the_one_field_list() {
    let src = source();
    let b = body(&src, "fn stw_take_over_and_wait(");
    assert!(
        b.contains("roots::push_off_frame_thread_roots(peer, xt_roots)"),
        "the frozen-peer block must call push_off_frame_thread_roots"
    );
    for copy in [
        "peer.native_pin_roots",
        "peer.native_pending_return",
        "peer.jit_hashmap_string_node_cache",
        "peer.string_case_cache",
    ] {
        assert!(
            !b.contains(copy),
            "a hand-written field copy (`{copy}`) is back in stw_take_over_and_wait"
        );
    }
}

/// `common-w4f-gc-event-durations-sampled-after-release`: the collection's
/// end is sealed before the frozen peers resume, and reported after the
/// release.
#[test]
fn the_collection_end_is_sealed_inside_the_pause() {
    let src = source();
    let b = body(&src, "fn run_collection_pause(");
    let seal = b
        .find("gc_event_seal(shared, &mut gc_event)")
        .expect("sealed");
    let resume = b
        .find("retire_skip_spans_and_resume(shared, taken)")
        .expect("resumes");
    let release = b
        .find(".complete_gc(result.pointer_map)")
        .expect("releases");
    let finish = b
        .find("gc_event_finish_by(shared, door, gc_event,")
        .expect("reports");
    assert!(seal < resume && resume < release && release < finish);
}

/// `common-w2a-finalizers-queued-by-forced-collections-wait-for-another-door`
/// (the fully-compiled half): the "undrained" verdict is read before this
/// collection queues its own finalizers, and acted on only after the
/// world is released.
#[test]
fn undrained_finalizers_are_judged_before_the_enqueue_and_handed_off_after_release() {
    let src = source();
    let b = body(&src, "fn run_collection_pause(");
    let judged = b
        .find("ref_work_stale(UNDRAINED_REF_WORK_PAUSES)")
        .expect("judged");
    let enqueue = b
        .find("enqueue_resurrected_finalizers(shared, &dead_finalizers)")
        .expect("enqueues");
    let release = b
        .find(".complete_gc(result.pointer_map)")
        .expect("releases");
    let handoff = b
        .find("hand_undrained_finalizers_to_delivery_thread(shared)")
        .expect("hands off");
    assert!(judged < enqueue && release < handoff);
    // Two whole pauses, not one: see the constant's doc.
    assert_eq!(super::UNDRAINED_REF_WORK_PAUSES, 2);
}

/// gci i12/wb (`gci-i12wb-jit-barrier-review-findings-FIXED-20261015` item 1):
/// the G1 concurrent-start pause scans the frozen peers' objects AFTER the
/// roots are seeded (the SATB queue is active by then) and BEFORE the
/// frozen peers resume, behind its kill switch.
#[test]
fn gci_i12wb_g1_initial_mark_scans_frozen_held_objects_before_resume() {
    let src = source();
    let b = body(&src, "fn g1_concurrent_mark_cycle(");
    let start = b
        .find("g1_start_concurrent_mark_with_root_regions(&stw, &ref_objs)")
        .expect("activates the cycle");
    let seed = b
        .find("g1_mark_roots(&stw, &all_roots)")
        .expect("seeds the roots");
    let copy = b
        .find("let frozen_held: Vec<ObjectRef> = xt_roots.clone();")
        .expect("keeps the frozen roots apart");
    let scan = b
        .find("g1_scan_frozen_held_objects(&stw, &frozen_held)")
        .expect("scans the frozen peers' objects");
    let resume = b
        .find("retire_skip_spans_and_resume(shared, taken)")
        .expect("resumes the peers");
    assert!(start < seed && copy < seed && seed < scan && scan < resume);
    assert!(b.contains("G1_CONC_FROZEN_EAGER_SCAN"), "the kill switch gates the scan");
}

/// `common-w4a-non-collection-pauses-have-no-pause-line`: every
/// non-collection pause driver times itself, seals before resuming the
/// frozen peers, and prints after the release.
#[test]
fn every_non_collection_pause_has_a_pause_line() {
    let src = source();
    for (sig, kind) in [
        ("fn maybe_concurrent_gc_at(", "GenInitialMark"),
        ("fn gen_concurrent_remark_pause(", "GenRemark"),
        ("fn zgc_concurrent_mark_cycle(", "ZgcMarkStart"),
        ("fn g1_concurrent_mark_cycle(", "G1InitialMark"),
        ("fn g1_final_remark_cleanup(", "G1Remark"),
    ] {
        let b = body(&src, sig);
        let start = b
            .find(&format!("NonCollectionPause::{kind})"))
            .unwrap_or_else(|| panic!("{sig} starts no `{kind}` pause timer"));
        let seal = b
            .find("non_collection_pause_seal(&mut pause_timer)")
            .unwrap_or_else(|| panic!("{sig} never seals its pause timer"));
        let finish = b
            .find("non_collection_pause_finish(pause_timer)")
            .unwrap_or_else(|| panic!("{sig} never prints its pause line"));
        let resume = b.find("retire_skip_spans_and_resume(shared, taken)");
        assert!(
            start < seal && seal < finish,
            "{sig}: start < seal < finish"
        );
        if let Some(resume) = resume {
            assert!(seal < resume, "{sig}: seal before the peers resume");
        }
    }
    // The HPROF pause seals and prints from its RAII `Drop`.
    let drop_body = body(&src, "impl Drop for NonMovingPause<'_> {");
    let seal = drop_body.find("non_collection_pause_seal(").expect("seals");
    let resume = drop_body
        .find("retire_skip_spans_and_resume")
        .expect("resumes");
    let finish = drop_body
        .find("non_collection_pause_finish(")
        .expect("prints");
    assert!(seal < resume && resume < finish);
    assert!(body(&src, "    pub(crate) fn request(shared: &'a SharedVm")
        .contains("NonCollectionPause::HeapDump"));
    // The frame-trace pause is a handshake pause of its own kind, printed
    // by the same `Drop` (interpreter round i1 wave 14, lane L3).
    let frame_trace = body(&src, "fn stw_publish_frame_traces_with<R>(");
    let raise = frame_trace
        .find("FrameTraceWanted::raise(&shared.mem.diagnostics.frame_trace_wanted)")
        .expect("raises the request first");
    let request = frame_trace
        .find("NonMovingPause::request_handshake(")
        .expect("takes a handshake pause");
    assert!(raise < request);
    assert!(frame_trace.contains("NonCollectionPause::FrameTrace,"));
    assert!(frame_trace.contains("FRAME_TRACE_GRACE,"));
}
