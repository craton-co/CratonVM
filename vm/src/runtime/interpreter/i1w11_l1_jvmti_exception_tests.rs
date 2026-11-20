use super::*;
use crate::runtime::jvmti::{self, EventCallbacks, EventMode, JvmtiEventKind};
use crate::threading::jvm_thread::ThreadId;
use cratonvm_reader::attribute::ExceptionTableEntry;

fn frame(class_id: ClassId, name: &str, table: Vec<ExceptionTableEntry>) -> Frame {
    Frame::new(
        class_id,
        "cratonvm/test/W11Events".to_string(),
        name.to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        table,
        4,
        2,
        &[],
    )
}

/// An agent enabled for `Exception` on the VM's manager gets one event
/// per throw, with the throw location, the exception and the handler
/// that will catch it (method 0 / location -1 when none does); the same
/// exception reaching an outer interpreter entry of the same thread is
/// not posted again, a rethrow after a catch is. Nothing is posted or
/// remembered while the event is disabled.
#[test]
fn a_jvmti_exception_event_is_posted_once_per_throw() {
    let _lock = jvmti::jvmti_registry_test_lock();
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let vm = shared.vm_identity;
    jvmti::install_manager_for_vm(vm, Arc::new(jvmti::JvmtiEventManager::new_for_vm(vm)));
    let mgr = jvmti::manager_for_vm(vm).expect("row was just installed");
    let exc_class = shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class("cratonvm/test/W11Thrown", 0)
        .expect("Compatible mode fabricates");
    let exc = shared.mem.heap.alloc_object(exc_class, 0);
    let owner = ClassId::new(0x00F1_0B01);
    let catch_all = ExceptionTableEntry {
        start_pc: 0,
        end_pc: 10,
        handler_pc: 7,
        catch_type: 0,
    };

    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = Arc::clone(&events);
    mgr.set_event_callbacks(EventCallbacks {
        exception: Some(Box::new(
            move |_, method, location, exception, catch_method, catch_location| {
                seen.lock().expect("event list").push((
                    method,
                    location,
                    exception != 0,
                    catch_method,
                    catch_location,
                ));
            },
        )),
        ..Default::default()
    })
    .expect("install callbacks");

    // Not enabled: nothing posted, nothing remembered.
    let mut t = JvmThread::new(ThreadId(0), "w11-jvmti-exceptions");
    t.frames.push(frame(owner, "thrower", vec![catch_all]));
    let mut idx = 0usize;
    unwind_to_handler(&shared, &mut t, &mut idx, 0, exc, 5).expect("caught");
    assert!(events.lock().expect("event list").is_empty());
    assert_eq!(t.reported_exception, 0);

    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::Exception, None)
        .expect("enable Exception");
    assert!(jvmti::any_exception_listener_active());
    let thrower = jvmti_events::jvmti_method_id(vm, &t.frames[0]);

    // Caught at pc 5 by the catch-all [0, 10) -> 7.
    let mut idx = 0usize;
    unwind_to_handler(&shared, &mut t, &mut idx, 0, exc, 5).expect("caught");
    assert_eq!(
        *events.lock().expect("event list"),
        vec![(thrower, 5, true, thrower, 7)]
    );
    assert_eq!(t.reported_exception, 0, "caught: forgotten");
    // Caught, so a rethrow of the same object is a new throw.
    let mut idx = 0usize;
    unwind_to_handler(&shared, &mut t, &mut idx, 0, exc, 5).expect("caught again");
    assert_eq!(events.lock().expect("event list").len(), 2);

    // Uncaught, then the outer interpreter entry of the same thread.
    let mut u = JvmThread::new(ThreadId(1), "w11-jvmti-uncaught");
    u.frames.push(frame(owner, "thrower", Vec::new()));
    let mut idx = 0usize;
    assert!(unwind_to_handler(&shared, &mut u, &mut idx, 0, exc, 5).is_err());
    assert_ne!(u.reported_exception, 0);
    u.frames.push(frame(owner, "outer", Vec::new()));
    let mut idx = 1usize;
    assert!(unwind_to_handler(&shared, &mut u, &mut idx, 1, exc, 2).is_err());
    {
        let events = events.lock().expect("event list");
        assert_eq!(events.len(), 3, "reported once, where thrown");
        assert_eq!(events[2], (thrower, 5, true, 0, -1), "uncaught");
    }

    let _ =
        mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::Exception, None);
    jvmti::forget_vm_jvmti_state(vm);
    // Wave 12: the uncaught thread's exit frees the handle it kept.
    let kept = u.reported_exception;
    release_debugger_thread_state(&shared, &mut u);
    assert_eq!(u.reported_exception, 0);
    assert!(shared.natives.jni_global_refs.lock().kind(kept).is_none());
    // Nothing reported: the exit helper is a no-op.
    release_debugger_thread_state(&shared, &mut t);
    assert_eq!(t.reported_exception, 0);
}
