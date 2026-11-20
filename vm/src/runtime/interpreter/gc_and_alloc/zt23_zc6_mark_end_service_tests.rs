//! zt t23/zc6 (`CRATONVM_ZGC_MARK_END_SERVICE`): the wiring of the
//! director tick's mark-end close. The heap half (the plan, the merge with
//! the timer, the ticket, the census) is unit-tested in
//! `gc/src/zgc/mark_end.rs` (`zt23zc6_*`).

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

/// The loop's close arm runs the attached collection and drops the VM
/// before any sleep; the job asks the heap for a ticket, retires its TLAB
/// and SATB buffer, marks the pause in flight, re-checks the ticket and the
/// director stop, and only then takes the rule-driven door (never
/// `System.gc()`'s), booking every outcome.
#[test]
fn zt23zc6_the_tick_closes_a_caught_up_cycle_through_the_allocation_door() {
    let src = source();
    let tick = body(&src, "fn zgc_director_tick_loop(");
    let arm = tick.find("ZTick::CloseMark =>").expect("a close arm");
    let run = tick[arm..]
        .find("run_zgc_mark_end_collection(&shared)")
        .expect("runs the close");
    let dropped = tick[arm..].find("drop(shared);").expect("drops the VM");
    assert!(run < dropped);
    let spawn = body(&src, "fn run_zgc_mark_end_collection(");
    assert!(spawn.contains("crate::native::jni::run_attached_service_as("));
    assert!(spawn.contains("let jmm_counted = !zgc_tick_thread_uncounted_enabled();"));
    let job = body(&src, "fn zgc_mark_end_collection(");
    let at = |needle: &str| {
        job.find(needle)
            .unwrap_or_else(|| panic!("`{needle}` in the job"))
    };
    let begin = at("heap.mark_end_service_begin()");
    let retire = at("thread.tlab.retire();");
    let satb = at("shared.mem.heap.flush_thread_satb();");
    let guard = at("heap.tick_pause_guard();");
    let recheck = at("heap.mark_end_service_still_due(ticket)");
    let pause = at("run_collection_pause(shared, thread, GcDoor::AllocationThreshold)");
    let end = at("heap.mark_end_service_end(ticket, Some(paused), continued_here);");
    assert!(begin < retire && retire < satb && satb < guard);
    assert!(guard < recheck && recheck < pause && pause < end);
    assert!(job.contains("heap.mark_end_service_end(ticket, None, false);"));
    assert!(
        !job.contains("force_full_collection"),
        "not System.gc()'s door"
    );
    assert!(!job.contains("request_major_gc"), "no explicit-GC request");
    assert!(!job.contains("request_tick_cycle"), "no rule bit is charged");
}
