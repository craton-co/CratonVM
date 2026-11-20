use super::*;
use crate::config::VmConfig;
use crate::threading::jvm_thread::ThreadId;

fn body(src: &str, sig: &str) -> String {
    let start = src
        .find(&format!("\n{sig}"))
        .unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &src[start + 1..];
    let stop = rest
        .find("\n\u{7d}\n")
        .unwrap_or_else(|| panic!("end of `{sig}` not found"));
    rest[..stop].to_string()
}

/// Residual 6: the pauses with no HotSpot operation are no safepoint,
/// but for ZGC's mark start (out of scope; a safepoint there).
#[test]
fn gci_i46_lg_which_pauses_are_no_safepoint() {
    use super::NonCollectionPause as P;
    for kind in [
        P::LoopExit,
        P::IcGrace,
        P::GenInitialMark,
        P::GenRemark,
        P::G1InitialMark,
    ] {
        assert!(
            non_collection_pause_is_no_safepoint(kind, non_collection_safepoint_name(kind)),
            "{kind:?}"
        );
    }
    assert!(!non_collection_pause_is_no_safepoint(P::ZgcMarkStart, None));
    // zt t24/zc8: a failed ZGC Pause Mark End is a safepoint too.
    assert!(!non_collection_pause_is_no_safepoint(
        P::ZgcMarkEndContinue,
        None
    ));
    for kind in [
        P::FrameTrace,
        P::HeapDump,
        P::HeapInspection,
        P::Redefinition,
        P::G1Remark,
    ] {
        assert!(
            !non_collection_pause_is_no_safepoint(kind, non_collection_safepoint_name(kind)),
            "{kind:?}"
        );
    }
    // A remark folded into a G1 full collection's one operation.
    assert!(non_collection_pause_is_no_safepoint(P::G1Remark, None));
}


/// Residual 3: the parked-report probe reads the requester's slot.
#[test]
fn gci_i46_lg_a_parked_report_is_found_by_its_requester() {
    let shared = SharedVm::new(VmConfig::default());
    assert!(!g1_young_report_parked_for(&shared, ThreadId(5)));
    shared
        .mem
        .alloc_ladder
        .g1_deferred_young
        .lock()
        .push(crate::vm::realms::heap_realm::G1DeferredYoungSlot {
            requester: 5,
            report: Box::new(()),
        });
    assert!(g1_young_report_parked_for(&shared, ThreadId(5)));
    assert!(!g1_young_report_parked_for(&shared, ThreadId(6)));
    shared.mem.alloc_ladder.g1_deferred_young.lock().clear();
}

/// Residual 3: the finish holds the line only on G1, under
/// `-Xlog:safepoint`, with the switch on and a report parked, after
/// printing an earlier held line of the requester; and the order in
/// `gc_event_finish_by` (park, then the line) stays.
#[test]
fn gci_i46_lg_the_finish_holds_while_a_report_is_parked() {
    let src = include_str!("../gc_events.rs").replace("\r\n", "\n");
    let f = body(&src, "fn safepoint_line_finish_by(");
    let wanted = f.find("xlog_safepoint_lines_wanted()").expect("gated on the tag");
    let release = f.find("safepoint_line_release_held(id.0);").expect("earlier held first");
    let parked = f.find("g1_young_report_parked_for(shared, id)").expect("the probe");
    let hold = f.find("safepoint_line_hold(name, id.0);").expect("held");
    assert!(f.contains("shared.mem.heap.is_g1()"));
    assert!(f.contains("xlog_safepoint_after_parked_report_enabled()"));
    assert!(wanted < release && release < parked && parked < hold);
    let by = body(&src, "pub(super) fn gc_event_finish_by(");
    let park = by
        .find("Some(id) => g1_park_deferred_young_report(shared, id, report),")
        .expect("the park");
    let line = by
        .find("safepoint_line_finish_by(shared, door, requester);")
        .expect("the line");
    assert!(park < line);
}
