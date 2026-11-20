use super::*;

fn read(rel: &str) -> String {
    let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{path}: {e}"))
        .replace("\r\n", "\n")
}

fn body(src: &str, sig: &str) -> String {
    let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &src[start..];
    let stop = rest
        .find("\n}\n")
        .unwrap_or_else(|| panic!("end of `{sig}` not found"));
    rest[..stop].to_string()
}

fn at(text: &str, needle: &str, what: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("{what}: `{needle}` not found"))
}

/// Only JDK 25's TIMED_PARKING is probed: PARKING (3) allocates nothing
/// after the yield, YIELDING (10) / BLOCKING (12) submit with OOME retry.
#[test]
fn gci_i24vl_only_a_timed_parking_yield_is_probed() {
    assert_eq!(VTHREAD_TIMED_PARKING, 6);
    assert!(yield_needs_post_yield_probe(&Value::Int(6)));
    for other in [0, 1, 2, 3, 4, 5, 7, 9, 10, 11, 12, 99, 6 | 256] {
        assert!(
            !yield_needs_post_yield_probe(&Value::Int(other)),
            "state {other} must not be probed"
        );
    }
    assert!(!yield_needs_post_yield_probe(&Value::Object(None)));
    // A pin every JDK's `Continuation.pinnedReason` accepts (2..=4).
    assert!((PINNED_CRITICAL_SECTION..=PINNED_MONITOR).contains(&YIELD_REFUSED_PIN_REASON));
    assert_ne!(YIELD_REFUSED_PIN_REASON, 0, "0 is \"yielded\"");
}

/// The runner's half: a refused remount leaves `VtYield::Park`, so
/// `remount_action` pushes no `0` on top of the pin; an ordinary one is
/// untouched.
#[test]
fn gci_i24vl_a_refused_remount_skips_the_yield_result() {
    let mut thread = JvmThread::new(ThreadId(24), "gci-i24vl");
    thread.vt_yield = VtYield::DoYield;
    assert!(refuse_deposited_yield(&mut thread, false).is_ok());
    assert!(
        matches!(thread.vt_yield, VtYield::DoYield),
        "not refused: unchanged"
    );
    assert!(refuse_deposited_yield(&mut thread, true).is_ok());
    assert!(matches!(thread.vt_yield, VtYield::Park));
    let interp = read("src/runtime/interpreter/resume_continuation.rs");
    let remount = body(&interp, "fn remount_action(");
    assert!(
        remount.contains("VtYield::Park => return RemountAction::Run,"),
        "a Park record must resume without a push"
    );
    assert!(remount.contains("leaf.stack.push(Value::Int(0))"));
}

/// The carrier probes after the slice's wait and before `cont` is re-read
/// and its root dropped; a refused yield re-runs as a resumed slice.
#[test]
fn gci_i24vl_the_carrier_probes_between_the_wait_and_the_outcome() {
    let src = read("src/vm/jdk_continuation.rs");
    let enter = body(&src, "pub(crate) fn enter_special(");
    let wait = at(&enter, "ctx.end_blocking_region();", "end of the wait");
    let refuse = at(
        &enter,
        "refuse_yield_without_headroom(ctx, &outcome, vt_tid, carrier)",
        "the probe",
    );
    let hosted = at(&enter, "runners.hosted.lock().remove(&carrier.0);", "hosted");
    let reread = at(&enter, "let cont = ctx.resolve_global_root(cont_root)", "cont re-read");
    let dropped = at(&enter, "ctx.remove_global_root(cont_root);", "root dropped");
    let arm = at(&enter, "SliceOutcome::Yielded => match mark_tail(", "yield arm");
    assert!(wait < refuse && refuse < hosted && hosted < reread && reread < arm);
    assert!(refuse < enter.rfind("ctx.remove_global_root(cont_root);").unwrap_or(0));
    assert!(dropped > 0);
    let rerun = &enter[refuse..hosted];
    assert!(rerun.contains("first: false,") && rerun.contains("refuse_yield: true,"));
    assert!(rerun.contains("None => break outcome,"));
    // The probe is the collecting ladder, then the take-back.
    let probe = body(&src, "fn refuse_yield_without_headroom(");
    let state = at(&probe, "yield_needs_post_yield_probe(", "state test");
    let alloc = at(
        &probe,
        "crate::runtime::interpreter::gc_alloc_array_unsampled(",
        "probe",
    );
    let take = at(&probe, ".take_runtime_for_mount(vt_tid.0, carrier.0)", "take-back");
    assert!(state < alloc && alloc < take);
    assert!(!probe.contains("new_array("), "not the no-GC native door");
    assert!(probe.contains("if !matches!(outcome, SliceOutcome::Yielded)"));
    // The runner applies it after the drain, before the frames run.
    let slice = body(&src, "fn run_slice(");
    let drain = at(&slice, ".check_post_block_gc();", "drain");
    let apply = at(&slice, "refuse_deposited_yield(&mut thread, refuse_yield)", "apply");
    let resume = at(&slice, "resume_continuation(shared, &mut thread)", "resume");
    assert!(drain < apply && apply < resume);
}
