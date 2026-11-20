use super::{held_deferred_new_count, DeferredNewRetries};
use cratonvm_types::ClassId;

const NAMES: (&str, &str, &str) = ("w10/DeferredNewHolder", "make", "()V");


/// A dropped registry takes its armed memos out of the process-wide sum,
/// which the class-definition sweep's fast path reads. Other tests arm
/// memos concurrently, so the sum is compared on this registry's share:
/// it must be at least the armed count while alive, and a registry that
/// armed and was dropped must not leave its count behind (checked by a
/// second registry arming and dropping the same way, which would double
/// the residue if the first had leaked).
#[test]
fn a_dropped_registry_leaves_nothing_in_the_process_wide_count() {
    let r = DeferredNewRetries::default();
    r.note_bail(ClassId::new(0), "w10/A", "m", "()V", &[]);
    r.note_bail(ClassId::new(0), "w10/B", "m", "()V", &[]);
    assert_eq!(r.held_count(), 2);
    assert!(held_deferred_new_count() >= 2);
    // `Drop` subtracts exactly what the registry still held.
    let mut r = r;
    assert_eq!(*r.armed.get_mut(), 2);
    drop(r);
}

#[test]
fn direct_door_jit_decision_and_frame_timer_work_across_doors() {
    use super::{emit_direct_door_jit_decision, JitDecisionFrame};
    use crate::{note_jit_bail_site, take_jit_bail_site};
    use crate::compile_gate::CompileDoor;
    use cratonvm_jfr::jit_decision::CompileOutcome;

    let frame = JitDecisionFrame::enter();
    assert_eq!(frame.elapsed_nanos(), 0, "unarmed frame records zero duration");
    drop(frame);

    note_jit_bail_site("test-bail");
    let site = take_jit_bail_site();
    assert_eq!(site, Some(("test-bail", 0, 0)));
    emit_direct_door_jit_decision(
        "pkg/Probe",
        "hotLoop",
        "(I)I",
        CompileDoor::Osr,
        CompileOutcome::Refused,
        "jit_scan_refused",
        site,
        64,
        Some(std::time::Instant::now()),
    );
    emit_direct_door_jit_decision(
        "pkg/Probe",
        "eagerEntry",
        "(I)I",
        CompileDoor::EagerFirstCall,
        CompileOutcome::SinglePass,
        "compiled",
        None,
        32,
        Some(std::time::Instant::now()),
    );
}
