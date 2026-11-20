use super::trace_innermost_decode_admits;

/// JIT round 21 wave 8 (lane splice): a not-entrant callee whose stub
/// never forwarded is the frame's own body (the stub builds no frame);
/// an entrant callee is admitted as `direct_call_callee` admits it.
#[test]
fn a_not_entrant_callee_that_never_forwarded_is_admitted() {
    const ENTRY: usize = 0x1000;
    assert!(trace_innermost_decode_admits(ENTRY, ENTRY, true, false, false));
    assert!(trace_innermost_decode_admits(ENTRY, ENTRY, false, false, false));
}

/// Every other shape stays refused: a forwarded stub (another body's
/// frame may stand there), a target that is not the callee's entry, a
/// caller whose call site was re-bound, and no target at all.
#[test]
fn forwarded_stubs_foreign_targets_and_rebound_callers_are_refused() {
    const ENTRY: usize = 0x1000;
    assert!(!trace_innermost_decode_admits(ENTRY, ENTRY, true, true, false));
    assert!(!trace_innermost_decode_admits(ENTRY + 8, ENTRY, false, false, false));
    assert!(!trace_innermost_decode_admits(ENTRY, ENTRY, false, false, true));
    assert!(!trace_innermost_decode_admits(0, 0, false, false, false));
}

/// The fallback is the stack walk's alone: the collector's door
/// (`innermost_frame_method`) never reaches it, so no oop map is read
/// through a relaxed answer.
#[test]
fn only_the_trace_walk_takes_the_fallback() {
    let src = concat!(include_str!("../conservative_roots/mod.rs"), "\n", include_str!("../conservative_roots/jit_frame_chain_entry.rs"), "\n", include_str!("../conservative_roots/gc_outgoing_arg_roots_enabled.rs"), "\n", include_str!("../conservative_roots/refresh_moving_young_coverage_for.rs"), "\n", include_str!("../conservative_roots/warn_cross_thread_jit_gap.rs"), "\n", include_str!("../conservative_roots/scan_one_frame.rs"));
    let calls = src.matches("trace_innermost_by_caller_call(\n").count();
    // The definition and the one call in `walk_jit_chain_opts`.
    assert_eq!(calls, 2, "one definition, one call site");
    let gc_door = src
        .split("fn innermost_frame_method(")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("the collector's door");
    assert!(!gc_door.contains("trace_innermost_by_caller_call"));
}

/// JIT round 21 wave 10 (lane splice; review w68 item 2): the raw-walk
/// memo keys the fallback's `ever_forwarded` answer through the
/// generation, so the recording walk must read the generation BEFORE it
/// walks -- read after, a first forward bumped mid-walk would be keyed as
/// seen.
#[test]
fn the_recording_walk_reads_the_generation_before_it_walks() {
    let src = concat!(include_str!("../conservative_roots/mod.rs"), "\n", include_str!("../conservative_roots/jit_frame_chain_entry.rs"), "\n", include_str!("../conservative_roots/gc_outgoing_arg_roots_enabled.rs"), "\n", include_str!("../conservative_roots/refresh_moving_young_coverage_for.rs"), "\n", include_str!("../conservative_roots/warn_cross_thread_jit_gap.rs"), "\n", include_str!("../conservative_roots/scan_one_frame.rs"));
    let body = src
        .split("fn active_compiled_frames_walk_recording_opts(")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("the recording walk");
    let recorded = body.split("record.clear();").nth(1).expect("the recording arm");
    let read = recorded
        .find("let generation = cratonvm_jit::jit_cache_generation();")
        .expect("one generation read");
    let walk = recorded.find("walk_jit_chain_opts(").expect("the walk");
    let store = recorded
        .find("record.generation = generation;")
        .expect("the key takes the early read");
    assert!(read < walk && walk < store);
    assert_eq!(recorded.matches("jit_cache_generation()").count(), 1);
}
