//! zt t21/zc2 (`CRATONVM_ZGC_CONCURRENT_SWEEP`): the wiring of the
//! deferred sweep's runner and waiter. The heap half (the split, the
//! install, the hook, the retry) is unit-tested in
//! `gc/src/zgc/concurrent_sweep.rs` (`zt21zc2_*`); the behaviour is the
//! rows of `docs/feature-designs/zgc-architecture.md`.

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

/// The runner is armed inside the pause right before the collection, and
/// the parked sweep runs after the release and BEFORE the events, whose
/// figures are re-sampled; the productivity verdict takes the swept live
/// figure.
#[test]
fn zt21zc2_the_door_runs_the_parked_sweep_after_the_release_and_before_the_events() {
    let src = source();
    let door = body(&src, "fn run_collection_pause(");
    let pos = |needle: &str| {
        door.find(needle)
            .unwrap_or_else(|| panic!("`{needle}` not in run_collection_pause"))
    };
    let arm = pos("shared.mem.heap.zgc_arm_concurrent_sweep_runner();");
    let collect = pos("shared.mem.heap.collect_garbage_with_finalizers(");
    let release = pos("shared.mem.gc_barrier.complete_gc(result.pointer_map);");
    let run = pos("shared.mem.heap.zgc_run_pending_sweep_after_release()");
    let reseal = pos("gc_event_reseal_after_concurrent_sweep(shared, &mut gc_event);");
    let live = pos("p.live = live;");
    let events = pos("gc_event_finish_by(shared, door, gc_event, Some(thread.thread_id));");
    assert!(arm < collect, "armed before the collection");
    assert!(release < run, "the sweep runs with the world released");
    assert!(
        run < reseal && reseal < events && live < events,
        "and before the events"
    );
}

/// A `System.gc()` caller returns after the whole cycle: the waiter runs
/// after the attempt loop, on every outcome of it.
#[test]
fn zt21zc2_a_system_gc_caller_waits_for_a_parked_sweep() {
    let src = source();
    let door = body(&src, "fn force_full_collection(");
    let lp = door
        .find("for _attempt in 0..FORCE_GC_ATTEMPTS")
        .expect("the loop");
    let wait = door
        .find("shared.mem.heap.zgc_finish_pending_sweep_for_waiter();")
        .expect("the waiter");
    let withdraw = door
        .find("let _ = cratonvm_gc::gc_quiescence::take_major_gc_request();")
        .expect("the withdraw");
    assert!(lp < withdraw && withdraw < wait, "{lp} {withdraw} {wait}");
    let tail = &door[wait..];
    assert!(
        !tail.contains("if collected"),
        "unconditional on the loop's outcome"
    );
}
