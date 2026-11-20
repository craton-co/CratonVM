use super::resume_or_despeculate_stash;
use crate::classloading::resolution::CachedBytecodeMethod;
use crate::config::VmConfig;
use crate::runtime::interpreter::CachedCallResult;
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::types::ObjectRef;
use crate::vm::SharedVm;
use cratonvm_jit::deopt::{FrameValue, MonitorInfo, ReconstructedFrame, ResumeSemantics};
use std::sync::Arc;

/// `static void m()` of class `T`: one `return`, no monitor bytecode.
fn cached() -> Arc<CachedBytecodeMethod> {
    Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: cratonvm_types::ClassId::new(0),
            class_name: Arc::from("T"),
            method_name: Arc::from("m"),
            method_descriptor: Arc::from("()V"),
            source_file: None,
            code: Arc::from(&[0xb1u8, 0, 0][..]),
            exception_table: Arc::from(Vec::new().into_boxed_slice()),
            max_stack: 8,
            max_locals: 4,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
        },
    ))
}

/// Metadata only; the `ret` body never runs. It vouches for a precise
/// resume, so the resumed case does not depend on the sink-resume switch.
fn body() -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
    buf.emit(&[0xC3]); // ret
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.can_deopt_resume = true;
    cm
}

fn addr(obj: ObjectRef) -> u64 {
    // Cast: object pointer to the raw word a stash carries.
    obj.as_ptr() as usize as u64
}

fn taken(obj: ObjectRef, depth: u32) -> MonitorInfo {
    MonitorInfo {
        object: FrameValue::Object(addr(obj)),
        lock_depth: depth,
        relock: false,
    }
}

fn frame(key: &str, locals: Vec<FrameValue>, monitors: Vec<MonitorInfo>) -> ReconstructedFrame {
    ReconstructedFrame {
        class_id: 0,
        method_key: key.to_string(),
        bci: 0,
        locals,
        stack: Vec::new(),
        monitors,
        semantics: ResumeSemantics::REEXECUTE,
        caller_frames: Vec::new(),
    }
}

fn vm_thread_and_objects(n: usize) -> (Arc<SharedVm>, JvmThread, Vec<ObjectRef>) {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let thread = JvmThread::new(ThreadId(0), "i23-l2-locks");
    let objs = (0..n)
        .map(|_| {
            shared
                .mem
                .heap
                .alloc_object(cratonvm_types::ClassId::new(7), 0)
        })
        .collect();
    (shared, thread, objs)
}

/// A nested callee's frame (a foreign stash) holding two compiled locks,
/// one of them twice, is dropped and the method re-runs: every hold the
/// compiled code took is released, and an elided lock (`relock`), which
/// the compiled code never took, is left alone.
#[test]
fn a_refused_foreign_frame_releases_the_locks_its_compiled_code_took() {
    let (shared, mut thread, objs) = vm_thread_and_objects(3);
    let (outer, inner, elided) = (objs[0], objs[1], objs[2]);
    let tid = thread.thread_id;
    // What the callee's `jit_monitor_enter`s did before it trapped.
    shared.threads.monitors.enter(outer, tid);
    shared.threads.monitors.enter(outer, tid);
    shared.threads.monitors.enter(inner, tid);
    // Held on this test's account, not the frame's.
    shared.threads.monitors.enter(elided, tid);
    let rf = frame(
        "Inner/Callee.trap:(I)I",
        vec![FrameValue::Int(7)],
        vec![
            taken(outer, 2),
            taken(inner, 1),
            MonitorInfo {
                object: FrameValue::Object(addr(elided)),
                lock_depth: 1,
                relock: true,
            },
        ],
    );
    let pins_before = thread.native_pin_roots.len();
    let r = resume_or_despeculate_stash(
        &shared,
        &mut thread,
        &body(),
        &cached(),
        &rf,
        None,
        0,
        "i23-l2-test",
    )
    .expect("no re-materialisation heap failure");
    assert!(r.is_none(), "a foreign frame is never resumed here");
    assert!(
        !shared.threads.monitors.holds(outer, tid),
        "both levels of the outer compiled lock are released"
    );
    assert!(
        !shared.threads.monitors.holds(inner, tid),
        "the inner compiled lock is released"
    );
    assert!(
        shared.threads.monitors.holds(elided, tid),
        "an elided lock was never the compiled code's to release"
    );
    assert_eq!(thread.native_pin_roots.len(), pins_before, "no pin leaks");
    assert!(shared.threads.monitors.exit(elided, tid).is_ok());
}

/// The method's OWN frame, refused because it cannot be rebuilt (an
/// unmappable local): the door re-runs the method from entry, which
/// enters the block's monitor again, so the compiled code's hold must be
/// gone first.
#[test]
fn a_refused_own_frame_releases_its_compiled_lock_before_the_rerun() {
    let (shared, mut thread, objs) = vm_thread_and_objects(1);
    let lock = objs[0];
    let tid = thread.thread_id;
    shared.threads.monitors.enter(lock, tid);
    let rf = frame(
        "T.m:()V",
        vec![FrameValue::Unsupported],
        vec![taken(lock, 1)],
    );
    let r = resume_or_despeculate_stash(
        &shared,
        &mut thread,
        &body(),
        &cached(),
        &rf,
        None,
        0,
        "i23-l2-test",
    )
    .expect("no re-materialisation heap failure");
    assert!(r.is_none(), "an unmappable frame is refused");
    assert!(
        !shared.threads.monitors.holds(lock, tid),
        "the refused frame's compiled lock is released before the re-run"
    );
    assert!(thread.frames.is_empty(), "nothing was pushed");
}

/// The resumed frame inherits the hold: nothing is released, nothing is
/// entered again, and the sink's pins are gone.
#[test]
fn a_resumed_frame_keeps_its_compiled_lock() {
    if !cratonvm_jit::deopt_real_enabled() {
        // `CRATONVM_DEOPT_REAL=0`: this sink has no precise resume to test.
        return;
    }
    let (shared, mut thread, objs) = vm_thread_and_objects(1);
    let lock = objs[0];
    let tid = thread.thread_id;
    shared.threads.monitors.enter(lock, tid);
    let rf = frame(
        "T.m:()V",
        vec![FrameValue::Object(addr(lock))],
        vec![taken(lock, 1)],
    );
    let pins_before = thread.native_pin_roots.len();
    let r = resume_or_despeculate_stash(
        &shared,
        &mut thread,
        &body(),
        &cached(),
        &rf,
        None,
        0,
        "i23-l2-test",
    )
    .expect("no re-materialisation heap failure");
    assert!(
        matches!(r, Some(CachedCallResult::FramePushed)),
        "a mappable frame of this body resumes"
    );
    assert_eq!(thread.native_pin_roots.len(), pins_before, "no pin leaks");
    assert!(
        shared.threads.monitors.holds(lock, tid),
        "the resumed frame still holds the compiled code's lock"
    );
    assert!(shared.threads.monitors.exit(lock, tid).is_ok());
    assert!(
        shared.threads.monitors.exit(lock, tid).is_err(),
        "held exactly once"
    );
}
