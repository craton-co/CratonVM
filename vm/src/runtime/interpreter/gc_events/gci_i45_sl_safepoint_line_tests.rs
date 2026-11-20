use super::*;

const GEN: (bool, bool) = (true, false);
const G1: (bool, bool) = (false, true);
const ZGC: (bool, bool) = (false, false);

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

/// Serial-shaped Generational and G1 collections carry HotSpot 25's
/// operation names.
#[test]
fn gci_i45_sl_collection_names() {
    use super::GcDoor as D;
    for door in [D::AllocationThreshold, D::AllocationFailure] {
        assert_eq!(
            safepoint_vm_operation(door, GEN, false),
            Some("SerialCollectForAllocation")
        );
    }
    for door in [
        D::AllocationThreshold,
        D::AllocationFailure,
        D::G1CompactionPause,
        D::G1HumongousAllocation,
    ] {
        assert_eq!(
            safepoint_vm_operation(door, G1, false),
            Some("G1CollectForAllocation")
        );
        assert_eq!(
            safepoint_vm_operation(door, G1, true),
            Some("G1CollectForAllocation")
        );
    }
    for door in [D::SystemGc, D::DiagnosticCommand, D::JvmtiForceGc] {
        assert_eq!(safepoint_vm_operation(door, GEN, false), Some("SerialGCCollect"));
        // Serial has no concurrent cycle: the flag changes nothing.
        assert_eq!(safepoint_vm_operation(door, GEN, true), Some("SerialGCCollect"));
        assert_eq!(safepoint_vm_operation(door, G1, false), Some("G1CollectFull"));
    }
    // `-XX:+ExplicitGCInvokesConcurrent`: `VM_G1TryInitiateConcMark`.
    for door in [D::SystemGc, D::DiagnosticCommand] {
        assert_eq!(
            safepoint_vm_operation(door, G1, true),
            Some("G1TryInitiateConcMark")
        );
    }
    // `_jvmti_force_gc` is not a user-requested concurrent cause.
    assert_eq!(
        safepoint_vm_operation(D::JvmtiForceGc, G1, true),
        Some("G1CollectFull")
    );
    for shape in [GEN, G1] {
        assert_eq!(
            safepoint_vm_operation(D::MetadataThreshold, shape, false),
            Some("CollectForMetadataAllocation")
        );
        // Part of the one inspection / dump operation the walk prints.
        assert_eq!(safepoint_vm_operation(D::HeapInspection, shape, false), None);
        assert_eq!(safepoint_vm_operation(D::HeapDump, shape, false), None);
    }
    // ZGC: out of scope, no line.
    assert_eq!(safepoint_vm_operation(D::SystemGc, ZGC, false), None);
    assert_eq!(safepoint_vm_operation(D::AllocationThreshold, ZGC, false), None);
}

/// zt t22/hunt3 (`CRATONVM_ZGC_XLOG_SAFEPOINT_LINES`): a ZGC collection
/// is printed as the operations its JFR pause rows carry, cut at the
/// split's boundaries; a concurrent cycle's collection (no mark start of
/// its own) as two; without a split as the one opening operation.
#[test]
fn zt22hunt3_zgc_collections_name_hotspots_operations() {
    use cratonvm_gc::gc_metrics::ZgcPauseSplit;
    let t0 = std::time::Instant::now();
    let (ms, me) = (
        t0 + std::time::Duration::from_micros(10),
        t0 + std::time::Duration::from_micros(30),
    );
    let full = ZgcPauseSplit::new(Some(ms), me);
    assert_eq!(
        zgc_safepoint_ops(Some(&full), true),
        (
            vec!["ZMarkStartYoungAndOld", "ZMarkEndOld", "ZRelocateStartOld"],
            vec![ms, me]
        )
    );
    assert_eq!(
        zgc_safepoint_ops(Some(&full), false).0,
        vec!["ZMarkStartYoung", "ZMarkEndYoung", "ZRelocateStartYoung"]
    );
    let conc = ZgcPauseSplit::new(None, me);
    assert_eq!(
        zgc_safepoint_ops(Some(&conc), true),
        (vec!["ZMarkEndOld", "ZRelocateStartOld"], vec![me])
    );
    assert_eq!(
        zgc_safepoint_ops(None, true),
        (vec!["ZMarkStartYoungAndOld"], Vec::new())
    );
    assert_eq!(zgc_safepoint_ops(None, false).0, vec!["ZMarkStartYoung"]);
    // The pure non-collection table is unchanged: the mark start is
    // named by `non_collection_safepoint_name_on`, under the tag.
    assert_eq!(
        non_collection_safepoint_name(NonCollectionPause::ZgcMarkStart),
        None
    );
    assert_eq!(ZGC_MARK_START_MAJOR_OPERATION, "ZMarkStartYoungAndOld");
}

/// Non-collection pauses: HotSpot's operation, or no line where HotSpot
/// runs no safepoint of its own.
#[test]
fn gci_i45_sl_non_collection_names() {
    use super::NonCollectionPause as P;
    assert_eq!(non_collection_safepoint_name(P::FrameTrace), Some("ThreadDump"));
    assert_eq!(non_collection_safepoint_name(P::HeapDump), Some("HeapDumper"));
    assert_eq!(
        non_collection_safepoint_name(P::HeapInspection),
        Some("GC_HeapInspection")
    );
    assert_eq!(
        non_collection_safepoint_name(P::Redefinition),
        Some("RedefineClasses")
    );
    assert_eq!(non_collection_safepoint_name(P::G1Remark), Some("G1PauseRemark"));
    for kind in [
        P::LoopExit,
        P::IcGrace,
        P::GenInitialMark,
        P::GenRemark,
        P::G1InitialMark,
        P::ZgcMarkStart,
    ] {
        assert_eq!(non_collection_safepoint_name(kind), None, "{kind:?}");
    }
}

/// The emitter's call sites: opened inside the pause, ended at the seal,
/// printed after the pause's own lines.
#[test]
fn gci_i45_sl_call_sites() {
    let src = include_str!("../gc_events.rs").replace("\r\n", "\n");
    let start = body(&src, "pub(super) fn gc_event_start(");
    assert!(start.contains("safepoint_line_begin_for(shared, None);"));
    let seal = body(&src, "pub(super) fn gc_event_seal(");
    let sealed = seal
        .find("s.sealed = Some((end, after, committed));")
        .expect("the seal");
    let sp = seal
        .find("unified_logging::safepoint_line_seal();")
        .expect("the line's end");
    assert!(sealed < sp);
    // gci i46/lg: `gc_event_finish_by` prints (or holds) it through
    // `safepoint_line_finish_by`.
    for (sig, needle) in [
        (
            "pub(super) fn gc_event_finish_by(",
            "safepoint_line_finish_by(shared, door, requester);",
        ),
        (
            "pub(super) fn gc_event_finish(",
            "safepoint_line_finish(collection_safepoint_name(shared, door));",
        ),
    ] {
        let f = body(&src, sig);
        let lines = f.find("gc_event_finish_with(").expect("the collection's lines");
        let line = f.find(needle).expect("the safepoint line");
        assert!(lines < line, "{sig}");
    }
    let nc_start = body(&src, "pub(super) fn non_collection_pause_start(");
    // gci i46/lg: through `non_collection_safepoint_begin`.
    let open = nc_start
        .find("non_collection_safepoint_begin(shared, kind, fold.is_some() || stw_full_mark);")
        .expect("opened");
    let none = nc_start.rfind("return None;").expect("the no-timer exit");
    assert!(open < none, "opened even when no timer is kept");
    assert!(body(&src, "fn non_collection_safepoint_begin(")
        .contains("safepoint_line_begin_for(shared, name);"));
    assert!(body(&src, "pub(super) fn non_collection_pause_seal(")
        .contains("unified_logging::safepoint_line_seal();"));
    let nc_finish = body(&src, "pub(super) fn non_collection_pause_finish(");
    let guard = nc_finish
        .find("let _safepoint_line = SafepointLineOnDrop;")
        .expect("printed on drop");
    let exit = nc_finish.find("let Some(t) = timer else").expect("the early exit");
    assert!(guard < exit);
    let begin = body(&src, "fn safepoint_line_begin_for(");
    let gate = begin
        .find("xlog_safepoint_lines_wanted()")
        .expect("gated on the tag");
    let read = begin.find("safepoint_stats()").expect("the barrier's figures");
    assert!(gate < read, "nothing read unless the tag is on");
}
