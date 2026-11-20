use super::*;
use crate::runtime::jvmti::{self, EventCallbacks, EventMode, JvmtiEventKind};
use crate::threading::jvm_thread::ThreadId;
use cratonvm_reader::attribute::ExceptionTableEntry;

/// Eleven `nop`s and a `return`, so bcis 0..=11 are real offsets.
fn frame(class_id: ClassId, name: &str, table: Vec<ExceptionTableEntry>) -> Frame {
    let mut code = vec![0x00; 11];
    code.push(0xb1);
    Frame::new(
        class_id,
        "cratonvm/test/W21CompiledCatch".to_string(),
        name.to_string(),
        "()V".to_string(),
        None,
        code,
        table,
        4,
        2,
        &[],
    )
}

type ExceptionEvents = Arc<std::sync::Mutex<Vec<(u64, i64, u64, i64)>>>;
type CatchEvents = Arc<std::sync::Mutex<Vec<(u64, i64)>>>;

/// A VM whose manager listens for `Exception` and `ExceptionCatch`.
fn listening_vm() -> (Arc<SharedVm>, ExceptionEvents, CatchEvents) {
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let vm = shared.vm_identity;
    jvmti::install_manager_for_vm(vm, Arc::new(jvmti::JvmtiEventManager::new_for_vm(vm)));
    let mgr = jvmti::manager_for_vm(vm).expect("row was just installed");
    let thrown: ExceptionEvents = Arc::default();
    let caught: CatchEvents = Arc::default();
    let (t_sink, c_sink) = (Arc::clone(&thrown), Arc::clone(&caught));
    mgr.set_event_callbacks(EventCallbacks {
        exception: Some(Box::new(
            move |_, method, location, _exception, catch_method, catch_location| {
                t_sink.lock().expect("events").push((
                    method,
                    location,
                    catch_method,
                    catch_location,
                ));
            },
        )),
        exception_catch: Some(Box::new(move |_, method, location, _exception| {
            c_sink.lock().expect("events").push((method, location));
        })),
        ..Default::default()
    })
    .expect("install callbacks");
    for kind in [JvmtiEventKind::Exception, JvmtiEventKind::ExceptionCatch] {
        mgr.set_event_notification_mode(EventMode::Enable, kind, None)
            .expect("enable");
    }
    (shared, thrown, caught)
}

fn thrown_object(shared: &SharedVm) -> ObjectRef {
    let exc_class = shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class("cratonvm/test/W21Thrown", 0)
        .expect("Compatible mode fabricates");
    shared.mem.heap.alloc_object(exc_class, 0)
}

/// A door's handler frame: `[2, 10) -> 7`, catch-all, the throwable on
/// the operand stack and the pc at the handler, as every door leaves it.
fn handler_frame(exc: ObjectRef) -> Frame {
    let row = ExceptionTableEntry {
        start_pc: 2,
        end_pc: 10,
        handler_pc: 7,
        catch_type: 0,
    };
    let mut f = frame(ClassId::new(0x00F1_2101), "catcher", vec![row]);
    f.stack.push(Value::Object(Some(exc))).expect("push");
    f.pc = 7;
    f
}

#[test]
fn a_compiled_catch_posts_exception_then_exception_catch() {
    let _lock = jvmti::jvmti_registry_test_lock();
    let (shared, thrown, caught) = listening_vm();
    let vm = shared.vm_identity;
    let exc = thrown_object(&shared);
    assert!(exception_events_may_be_armed(&shared));

    let mut t = JvmThread::new(ThreadId(0), "w21-compiled-catch");
    t.frames.push(handler_frame(exc));
    let catcher = jvmti_events::jvmti_method_id(vm, &t.frames[0]);
    report_exception_caught_by_compiled_door(&shared, &mut t, 0, 5, 7);
    assert_eq!(
        *thrown.lock().expect("events"),
        vec![(catcher, 5, catcher, 7)],
        "Exception at the compiled throw bci, caught by this frame's handler"
    );
    assert_eq!(*caught.lock().expect("events"), vec![(catcher, 7)]);
    assert_eq!(t.reported_exception, 0, "caught: forgotten");

    // An unknown throw pc is reported at the start of the protected range
    // whose handler runs. A second catch of the same object is a new throw.
    report_exception_caught_by_compiled_door(&shared, &mut t, 0, usize::MAX, 7);
    assert_eq!(thrown.lock().expect("events")[1], (catcher, 2, catcher, 7));
    assert_eq!(caught.lock().expect("events").len(), 2);

    jvmti::forget_vm_jvmti_state(vm);
}

/// An exception an interpreted callee threw was reported by that callee's
/// unwinder; the compiled door that catches it posts only `ExceptionCatch`.
#[test]
fn an_exception_reported_by_an_interpreted_callee_is_not_reported_again() {
    let _lock = jvmti::jvmti_registry_test_lock();
    let (shared, thrown, caught) = listening_vm();
    let vm = shared.vm_identity;
    let exc = thrown_object(&shared);

    let mut t = JvmThread::new(ThreadId(0), "w21-callee-reported");
    t.frames.push(handler_frame(exc));
    t.frames
        .push(frame(ClassId::new(0x00F1_2102), "callee", Vec::new()));
    let mut idx = 1usize;
    assert!(unwind_to_handler(&shared, &mut t, &mut idx, 1, exc, 3).is_err());
    assert_eq!(
        thrown.lock().expect("events").len(),
        1,
        "the callee's throw"
    );
    assert_ne!(t.reported_exception, 0);
    let _ = t.frames.pop();

    report_exception_caught_by_compiled_door(&shared, &mut t, 0, 5, 7);
    assert_eq!(thrown.lock().expect("events").len(), 1, "not again");
    let catcher = jvmti_events::jvmti_method_id(vm, &t.frames[0]);
    assert_eq!(*caught.lock().expect("events"), vec![(catcher, 7)]);
    assert_eq!(t.reported_exception, 0, "caught: forgotten");

    jvmti::forget_vm_jvmti_state(vm);
}

/// Another VM's listener neither arms this VM's decline nor reaches its
/// doors' events; with no listener the door posts nothing.
#[test]
fn another_vms_listener_does_not_arm_this_vm() {
    let _lock = jvmti::jvmti_registry_test_lock();
    let (loud, thrown, caught) = listening_vm();
    let quiet = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    jvmti::install_manager_for_vm(
        quiet.vm_identity,
        Arc::new(jvmti::JvmtiEventManager::new_for_vm(quiet.vm_identity)),
    );
    assert!(jvmti::any_exception_listener_active(), "the union is up");
    assert!(exception_events_may_be_armed(&loud));
    assert!(!exception_events_may_be_armed(&quiet));

    let exc = thrown_object(&quiet);
    let mut t = JvmThread::new(ThreadId(0), "w21-quiet-vm");
    t.frames.push(handler_frame(exc));
    report_exception_caught_by_compiled_door(&quiet, &mut t, 0, 5, 7);
    assert!(thrown.lock().expect("events").is_empty());
    assert!(caught.lock().expect("events").is_empty());
    assert_eq!(t.reported_exception, 0);

    jvmti::forget_vm_jvmti_state(loud.vm_identity);
    jvmti::forget_vm_jvmti_state(quiet.vm_identity);
}

/// An agent listening for `ExceptionCatch` alone makes the compiled local
/// handler decline too (the doors post the event), for its own VM only,
/// and not the `Exception` post.
#[test]
fn an_exception_catch_listener_alone_arms_the_decline() {
    let _lock = jvmti::jvmti_registry_test_lock();
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let vm = shared.vm_identity;
    jvmti::install_manager_for_vm(vm, Arc::new(jvmti::JvmtiEventManager::new_for_vm(vm)));
    let other = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    jvmti::install_manager_for_vm(
        other.vm_identity,
        Arc::new(jvmti::JvmtiEventManager::new_for_vm(other.vm_identity)),
    );
    let mgr = jvmti::manager_for_vm(vm).expect("row was just installed");
    assert!(!compiled_catch_events_armed(&shared));
    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::ExceptionCatch, None)
        .expect("enable ExceptionCatch");
    assert!(jvmti::any_exception_catch_listener_active());
    assert!(compiled_catch_events_armed(&shared));
    assert!(
        !exception_events_may_be_armed(&shared),
        "no Exception client"
    );
    assert!(!compiled_catch_events_armed(&other), "another VM's agent");
    let _ = mgr.set_event_notification_mode(
        EventMode::Disable,
        JvmtiEventKind::ExceptionCatch,
        None,
    );
    assert!(!compiled_catch_events_armed(&shared));
    jvmti::forget_vm_jvmti_state(vm);
    jvmti::forget_vm_jvmti_state(other.vm_identity);
}

/// The body of the first `fn` whose signature starts with `sig` in `src`,
/// up to the first line that is a lone closing brace.
fn body<'a>(src: &'a str, sig: &str) -> &'a str {
    let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &src[start..];
    let end = rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3);
    &rest[..end]
}

/// Source witness: every compiled catch door posts through
/// `report_exception_caught_by_compiled_door` (the doors of
/// `helpers::every_compiled_catch_door_drains_the_leftover_native_return`
/// plus the first-call door's `jit_early_exception` arm), and the local
/// handler lookup declines while an exception client may be armed before
/// either of its arms can catch. `jvmti::EXCEPTION_EVENTS_NEED_THE_INTERPRETER`
/// may be turned off only while this holds and the remaining gap on the
/// proposal page is closed.
#[test]
fn every_compiled_catch_door_reports_the_catch() {
    let lf = |s: &str| s.replace("\r\n", "\n");
    let interp = lf(concat!(include_str!("../interpreter/mod.rs"), "\n", include_str!("../interpreter/cached_invoke_target.rs"), "\n", include_str!("../interpreter/execute.rs"), "\n", include_str!("../interpreter/resume_continuation.rs"), "\n", include_str!("../interpreter/execute_frame_from_index_loop.rs"), "\n", include_str!("../interpreter/resolve_array_class_loader_aware.rs"), "\n", include_str!("../interpreter/park_for_debugger_under.rs"), "\n", include_str!("../interpreter/double_arith_apply.rs")));
    let dispatch = lf(include_str!("../interpreter/exception_dispatch.rs"));
    let resume = lf(concat!(include_str!("../interpreter/deopt_resume/mod.rs"), "\n", include_str!("../interpreter/deopt_resume/ir_deopt_resume_enabled.rs"), "\n", include_str!("../interpreter/deopt_resume/static_handed_hold_is_the_mirror.rs"), "\n", include_str!("../interpreter/deopt_resume/charge_osr_chain_predicate_exit.rs")));
    let bridge = lf(concat!(include_str!("../interpreter/jit_bridge/mod.rs"), "\n", include_str!("../interpreter/jit_bridge/jit_verdicts.rs"), "\n", include_str!("../interpreter/jit_bridge/osr_continuation.rs"), "\n", include_str!("../interpreter/jit_bridge/compile_osr_body.rs"), "\n", include_str!("../interpreter/jit_bridge/osr_exception_exit.rs"), "\n", include_str!("../interpreter/jit_bridge/try_osr.rs"), "\n", include_str!("../interpreter/jit_bridge/native_shadow_arms.rs"), "\n", include_str!("../interpreter/jit_bridge/compile_optimizing_artifact.rs"), "\n", include_str!("../interpreter/jit_bridge/never_run_static_exit_pcs.rs"), "\n", include_str!("../interpreter/jit_bridge/try_jit_compile_callee_slow.rs"), "\n", include_str!("../interpreter/jit_bridge/byte_array_field_declarer_admitted.rs"), "\n", include_str!("../interpreter/jit_bridge/folded_handle_member_row.rs"), "\n", include_str!("../interpreter/jit_bridge/callee_never_taken_rows.rs"), "\n", include_str!("../interpreter/jit_bridge/mint_ldc_slots_at_offer.rs")));
    let helpers = lf(crate::jit::helpers::HELPERS_SRC);
    const REPORT: &str = "report_exception_caught_by_compiled_door(";
    for (src, sig) in [
        (
            &dispatch,
            "pub(super) fn route_jit_exception_through_method(",
        ),
        (&resume, "pub(crate) fn execute_resumed_handler_frame("),
        (
            &bridge,
            "pub(in crate::runtime::interpreter) fn route_osr_exception_out_of_artifact(",
        ),
    ] {
        assert!(body(src, sig).contains(REPORT), "`{sig}` does not report");
    }
    assert!(
        body(&dispatch, "pub(crate) fn run_jit_callee_handler(")
            .contains("execute_resumed_handler_frame("),
        "the callee handler must run through the reporting push"
    );
    // An escape the OSR door knows the precise bci of is reported there.
    assert!(
        body(
            &bridge,
            "pub(in crate::runtime::interpreter) fn route_osr_exception_out_of_artifact("
        )
        .contains("deliver_exception_event_if_armed("),
        "the OSR door does not report an escape"
    );
    // The first-call door's arm, both outcomes.
    let arm_start = interp
        .find("if let Some(early_exc) = jit_early_exception \x7b")
        .expect("the jit_early_exception arm");
    let arm = &interp[arm_start..];
    let arm = &arm[..arm
        .find("// Execute (with panic protection")
        .expect("arm end")];
    assert!(
        arm.contains(REPORT),
        "the early arm does not report a catch"
    );
    assert!(
        arm.contains("deliver_exception_event_if_armed("),
        "the early arm does not report an escape"
    );
    // The decline precedes both local-handler arms.
    let lookup = body(&helpers, "unsafe fn jit_local_handler_lookup_body(");
    let decline = lookup
        .find("compiled_catch_events_armed(")
        .expect("the lookup declines while armed");
    let implicit = lookup
        .find("local_handler_enter_implicit(")
        .expect("the implicit arm");
    let dispatched = lookup
        .find("take_jit_pending_exception(")
        .expect("the dispatched arm");
    assert!(decline < implicit && decline < dispatched);
}
