use super::*;
use crate::config::VmConfig;
use crate::threading::jvm_thread::ThreadId;
use cratonvm_jit::deopt::{FrameValue, MonitorInfo, ReconstructedFrame};
use std::sync::Arc;

fn frame_holding(addr: u64, lock_depth: u32, relock: bool) -> ReconstructedFrame {
    ReconstructedFrame {
        class_id: 0,
        method_key: "T.m:()V".to_string(),
        bci: 4,
        locals: vec![FrameValue::Object(addr)],
        stack: vec![],
        monitors: vec![MonitorInfo {
            object: FrameValue::Object(addr),
            lock_depth,
            relock,
        }],
        semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
        caller_frames: Vec::new(),
    }
}

/// HotSpot unlocks the monitors of the frames it pops after a failed
/// reallocation; the abandoned activation's compiled locks must not
/// outlive it, at every level the compiled code took.
#[test]
fn an_abandoned_activation_releases_the_locks_its_compiled_code_took() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let obj = shared
        .mem
        .heap
        .alloc_object(cratonvm_types::ClassId::new(7), 0);
    // Cast: object pointer to integer address
    let addr = obj.as_ptr() as usize as u64;
    shared.threads.monitors.enter(obj, thread.thread_id);
    shared.threads.monitors.enter(obj, thread.thread_id);
    let rf = frame_holding(addr, 2, false);
    let base = pin_compiled_held_monitors(&mut thread, &rf);
    assert_eq!(
        thread.native_pin_roots.len(),
        base + 1,
        "the held lock is pinned"
    );
    release_compiled_held_monitors(&shared, &mut thread, &rf, base);
    assert_eq!(thread.native_pin_roots.len(), base, "the pins are released");
    assert!(
        !shared.threads.monitors.holds(obj, thread.thread_id),
        "both levels the compiled code took are released"
    );
}

/// An elided lock (`relock`) was never taken by the compiled code, so an
/// abandoned activation has nothing of it to release.
#[test]
fn an_elided_lock_is_not_released_for_an_abandoned_activation() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let obj = shared
        .mem
        .heap
        .alloc_object(cratonvm_types::ClassId::new(7), 0);
    // Cast: object pointer to integer address
    let addr = obj.as_ptr() as usize as u64;
    // A lock some OTHER activation of this thread holds.
    shared.threads.monitors.enter(obj, thread.thread_id);
    let rf = frame_holding(addr, 1, true);
    let base = pin_compiled_held_monitors(&mut thread, &rf);
    assert_eq!(thread.native_pin_roots.len(), base, "nothing to pin");
    release_compiled_held_monitors(&shared, &mut thread, &rf, base);
    assert!(shared.threads.monitors.holds(obj, thread.thread_id));
    assert!(shared.threads.monitors.exit(obj, thread.thread_id).is_ok());
}

/// Only the allocator's heap-exhaustion shape is answered with
/// `OutOfMemoryError`; a malformed graph stays an ordinary refusal.
#[test]
fn only_heap_exhaustion_is_the_rematerialisation_oom() {
    assert!(materialisation_failed_for_heap(
        &MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
            message: "Java heap space (alloc_object with 2 fields)".to_string(),
        }))
    ));
    assert!(!materialisation_failed_for_heap(
        &MethodCallFailed::InternalError(VmError::Internal {
            message: "deopt materialize: frame references unknown virtual object id 3"
                .to_string(),
        })
    ));
    assert!(frame_build_heap_failure_is_answered(
        DeoptFrameBail::VirtualMaterialiseHeap
    ));
    assert!(!frame_build_heap_failure_is_answered(
        DeoptFrameBail::VirtualMaterialise
    ));
}

/// The new refusal and the two callee services are census rows a reader
/// can find by name, and a callee re-run is counted under its own cause.
#[test]
fn the_census_names_the_heap_refusal_and_the_callee_services() {
    assert!(deopt_frame_bail_counts()
        .iter()
        .any(|(n, _)| *n == "virtual-object-materialise-heap-exhausted"));
    let unsound = |name: &str| {
        door_rerun_census()
            .into_iter()
            .find(|(n, _, _)| *n == name)
            .map(|(_, _, u)| u)
    };
    let before = unsound("callee-declined").expect("the callee-declined row");
    assert!(
        unsound("callee-frameless").is_some(),
        "the callee-frameless row"
    );
    note_door_rerun(DoorRerunCause::CalleeDeclined, false);
    let after = unsound("callee-declined").expect("the callee-declined row");
    // Other tests of this binary may only add to the row.
    assert!(after > before);
}
