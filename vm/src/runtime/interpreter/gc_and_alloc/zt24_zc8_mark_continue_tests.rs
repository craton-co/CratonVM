//! zt t24/zc8 (`CRATONVM_ZGC_MARK_CONTINUE`): the wiring of the Pause Mark
//! End attempt. The heap half (the switch, the attempt's verdicts, the
//! cap, the urgent reserve, the log) is unit-tested in
//! `gc/src/zgc/mark_end.rs` (`zt24zc8_*`); the `-Xlog:gc,phases` lines in
//! `gc_events.rs`.

use super::ZgcMarkContinueDoor;

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

/// The door is shut by default, a guard opens it for its scope only and
/// restores what it found (a nested shut guard shuts it, and the outer
/// value comes back).
#[test]
fn zt24zc8_the_door_is_scoped_and_restores() {
    assert!(!ZgcMarkContinueDoor::is_open(), "shut by default");
    {
        let _outer = ZgcMarkContinueDoor::open(true);
        assert!(ZgcMarkContinueDoor::is_open());
        {
            let _inner = ZgcMarkContinueDoor::open(false);
            assert!(!ZgcMarkContinueDoor::is_open());
        }
        assert!(ZgcMarkContinueDoor::is_open(), "restored");
    }
    assert!(!ZgcMarkContinueDoor::is_open(), "shut again");
}

/// The attempt sits right after the take-over and before every piece of
/// a collection (its events, its roots, the reference pass, the
/// collection), and a release returns before them; the attempt asks for
/// the allocation door and the open door before it gathers anything, and
/// the release seals inside the world, resumes, reopens with an empty map,
/// disarms, then books.
#[test]
fn zt24zc8_the_attempt_precedes_every_piece_of_the_collection() {
    let src = source();
    let pause = body(&src, "fn run_collection_pause(");
    let at = |needle: &str| {
        pause
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` in run_collection_pause"))
    };
    // Needles split: whole-file scans count the take-over's call sites
    // (`every_take_over_runs_under_an_unwind_guard`) and the registry
    // scan's callers (`memory/roots.rs`).
    let takeover = at(concat!("let taken = stw_take_over", "_and_wait("));
    let attempt = at("zgc_mark_end_attempt(shared, thread, door, &xt_roots)");
    let release = at("zgc_release_mark_end_attempt(shared, taken, unwind_abort, release,");
    let events = at("let mut gc_event = gc_event_start(shared);");
    let roots = at(concat!("collect_roots_registry", "_appended(shared, thread)"));
    let refs = at("weakref_null_referents_pre_gc(shared);");
    assert!(takeover < attempt && attempt < release);
    assert!(release < events && events < roots && roots < refs);
    let job = body(&src, "fn zgc_mark_end_attempt(");
    let gate = job
        .find("GcDoor::AllocationThreshold | GcDoor::AllocationFailure\n    ) || !ZgcMarkContinueDoor::is_open()")
        .expect("the door gate");
    let wanted = job.find("heap.zgc_mark_continue_wanted()").expect("the heap gate");
    let scan = job.find("collect_roots(shared, thread)").expect("the scan");
    let verdict = job.find("heap.try_mark_end(&stw, &roots)").expect("the attempt");
    assert!(gate < wanted && wanted < scan && scan < verdict);
    assert!(
        !job.contains(concat!("collect_roots_registry", "_appended(")),
        "that scan has one caller by rule (memory/roots.rs)"
    );
    let rel = body(&src, "fn zgc_release_mark_end_attempt(");
    let seal = rel.find("non_collection_pause_seal(&mut timer);").expect("seal");
    let resume = rel
        .find("retire_skip_spans_and_resume(shared, taken);")
        .expect("resume");
    let reopen = rel
        .find(".complete_gc(cratonvm_types::PointerMap::default());")
        .expect("reopen");
    let disarm = rel.find("abort.disarm();").expect("disarm");
    let finish = rel.find("non_collection_pause_finish(timer);").expect("finish");
    let booked = rel.find("shared.mem.heap.zgc_mark_continue_released(pause_ns);").expect("booked");
    assert!(seal < booked && booked < resume && resume < reopen && reopen < disarm);
    assert!(disarm < finish);
}

/// Only the occupancy poll, the mark-end service and (zt t31/zc11) ZGC's
/// forced occupancy sites open the door; the poll not for a latched
/// request; the timer tick, `System.gc()` and the allocation-failure doors
/// never do.
#[test]
fn zt24zc8_only_the_poll_and_the_service_open_the_door() {
    let src = source();
    let poll = body(&src, "fn maybe_gc(");
    assert!(poll.contains("ZgcMarkContinueDoor::open(entry_needs && !entry_requested)"));
    let job = body(&src, "fn zgc_mark_end_collection(");
    let open = job.find("ZgcMarkContinueDoor::open(true)").expect("opened");
    let pause = job
        .find("run_collection_pause(shared, thread, GcDoor::AllocationThreshold)")
        .expect("the pause");
    assert!(open < pause);
    // zt t31/zc11 (1f R7): and ZGC's forced occupancy sites (the compiled
    // refill trigger), never a failed allocation or a requester.
    let forced = body(&src, "fn maybe_gc_forced_collected_stamped_on(");
    let open = forced
        .find("ZgcMarkContinueDoor::open(zgc_occupancy && requester.is_none())")
        .expect("the forced occupancy door opens it");
    let pause = forced
        .find("run_collection_pause(shared, thread, door)")
        .expect("the forced pause");
    assert!(open < pause);
    for sig in [
        "fn force_full_collection(",
        "fn run_zgc_tick_collection(",
    ] {
        if let Some(start) = src.find(&format!("\n{sig}")) {
            let b = body(&src[start..], sig);
            assert!(!b.contains("ZgcMarkContinueDoor"), "{sig} never continues");
        }
    }
}
