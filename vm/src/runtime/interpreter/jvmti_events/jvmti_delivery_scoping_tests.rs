use super::*;
use crate::runtime::jvmti::{
    self, EventCallbacks, EventMode, JvmtiEventKind, JvmtiEventManager, MethodId,
};
use crate::threading::jvm_thread::ThreadId;
use std::sync::{Arc, Mutex};

/// A `vm_identity` no other test and no real VM can collide with. Real
/// identities come from `NEXT_VM_IDENTITY` (`vm/src/vm/vm_init.rs:9`), a
/// counter starting at 1, so small integers are NOT safe to fake with in a
/// binary that also builds real `SharedVm`s. The base is distinct from
/// `runtime::jvmti`'s own `scoped_test_vm()` base (`0x7000_0000`) so the
/// two modules cannot hand out the same row even by accident. gcg g2/k:
/// both now draw from the real counter (`vm_init::test_vm_identity`),
/// which is unique against every real VM and every test by construction.
fn scoped_vm() -> usize {
    crate::vm::vm_init::test_vm_identity()
}

/// Install an empty manager owned by `vm` and return it.
fn manager_for(vm: usize) -> Arc<JvmtiEventManager> {
    jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
    let mgr = jvmti::manager_for_vm(vm).expect("row was just installed");
    assert_eq!(
        mgr.vm_identity(),
        vm,
        "manager_for_vm must not resolve through the unattributed seam for an installed row"
    );
    mgr
}

/// Subscribe `mgr`'s VM to `kinds` and install `callbacks`.
///
/// `set_event_callbacks` replaces the whole struct, so it is called once
/// with every callback the test needs — calling it per kind would silently
/// drop all but the last, which is exactly the shape of bug that makes a
/// scoping test pass for the wrong reason.
fn watch(mgr: &Arc<JvmtiEventManager>, kinds: &[JvmtiEventKind], callbacks: EventCallbacks) {
    for kind in kinds {
        mgr.set_event_notification_mode(EventMode::Enable, *kind, None)
            .expect("enabling an event on a fresh manager cannot fail");
    }
    mgr.set_event_callbacks(callbacks)
        .expect("installing callbacks on a fresh manager cannot fail");
}

type Seen = Arc<Mutex<Vec<(u64, MethodId)>>>;

fn seen() -> Seen {
    Arc::new(Mutex::new(Vec::new()))
}

fn method_entry_cb(sink: &Seen) -> EventCallbacks {
    let s = sink.clone();
    EventCallbacks {
        method_entry: Some(Box::new(move |t, m| s.lock().unwrap().push((t, m)))),
        ..Default::default()
    }
}

fn method_exit_cb(sink: &Seen) -> EventCallbacks {
    let s = sink.clone();
    EventCallbacks {
        method_exit: Some(Box::new(move |t, m, _exc, _rv| {
            s.lock().unwrap().push((t, m))
        })),
        ..Default::default()
    }
}

fn test_frame(method_name: &str) -> Frame {
    Frame::new(
        ClassId::new(0),
        "T".to_string(),
        method_name.to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        8,
        4,
        &[],
    )
}

fn test_thread(name: &str) -> JvmThread {
    JvmThread::new(ThreadId(1234), name)
}

/// Two overloads of one name must not share an event method id. The
/// synthesized fallback hashed the name only, so `m()V` and `m(I)V` were
/// indistinguishable to an agent.
#[test]
fn overloads_get_distinct_synthesized_method_ids() {
    let overload = |descriptor: &str| {
        Frame::new(
            ClassId::new(7),
            "T".to_string(),
            "m".to_string(),
            descriptor.to_string(),
            None,
            vec![0xb1],
            vec![],
            8,
            4,
            &[],
        )
    };
    let (a, b) = (overload("()V"), overload("(I)V"));
    assert_ne!(synth_method_id(&a), synth_method_id(&b));
    // The class id stays in the upper half.
    assert_eq!(synth_method_id(&a) >> 32, 7);
    // A VM that cannot be reached (no bridge) falls back to the synthesized id.
    let vm = scoped_vm();
    assert_eq!(jvmti_method_id(vm, &a), synth_method_id(&a));
}

/// MethodExit raised with VM A's identity reaches A's agent and never B's.
#[test]
fn method_exit_reaches_only_the_raising_vms_agent() {
    let _lock = jvmti::jvmti_registry_test_lock();
    let (a, b) = (scoped_vm(), scoped_vm());
    let (ma, mb) = (manager_for(a), manager_for(b));
    let (sa, sb) = (seen(), seen());
    watch(&ma, &[JvmtiEventKind::MethodExit], method_exit_cb(&sa));
    watch(&mb, &[JvmtiEventKind::MethodExit], method_exit_cb(&sb));

    let mut thread = test_thread("method-exit-scoping");
    thread.frames.push(test_frame("m"));
    let expected = synth_method_id(&thread.frames[0]);
    fire_jvmti_method_exit_normal(a, &mut thread, 0, &Some(Value::Int(7)));

    assert_eq!(
        *sa.lock().unwrap(),
        vec![(thread.thread_id.0, expected)],
        "the raising VM's agent must see exactly one MethodExit"
    );
    assert!(
        sb.lock().unwrap().is_empty(),
        "a second VM's agent must never see another VM's MethodExit"
    );

    jvmti::forget_vm_jvmti_state(a);
    jvmti::forget_vm_jvmti_state(b);
}

/// `push_frame_and_fire_entry` is the single frame-push chokepoint, so a
/// missed identity there mis-attributes *every* MethodEntry in the VM.
#[test]
fn push_frame_and_fire_entry_attributes_method_entry_to_its_vm() {
    let _lock = jvmti::jvmti_registry_test_lock();
    let (a, b) = (scoped_vm(), scoped_vm());
    let (ma, mb) = (manager_for(a), manager_for(b));
    let (sa, sb) = (seen(), seen());
    watch(&ma, &[JvmtiEventKind::MethodEntry], method_entry_cb(&sa));
    watch(&mb, &[JvmtiEventKind::MethodEntry], method_entry_cb(&sb));

    let mut thread = test_thread("method-entry-scoping");
    let frame = test_frame("entered");
    let expected = synth_method_id(&frame);
    push_frame_and_fire_entry(b, &mut thread, frame);

    assert_eq!(thread.frames.len(), 1, "the frame must still be pushed");
    assert_eq!(
        *sb.lock().unwrap(),
        vec![(thread.thread_id.0, expected)],
        "the pushing VM's agent must see the MethodEntry"
    );
    assert!(
        sa.lock().unwrap().is_empty(),
        "the other VM's agent must not see it"
    );

    jvmti::forget_vm_jvmti_state(a);
    jvmti::forget_vm_jvmti_state(b);
}

/// FramePop consumes the request on the raising VM's thread and delivers
/// to that VM only.
#[test]
fn frame_pop_reaches_only_the_raising_vms_agent() {
    let _lock = jvmti::jvmti_registry_test_lock();
    let (a, b) = (scoped_vm(), scoped_vm());
    let (ma, mb) = (manager_for(a), manager_for(b));
    let (sa, sb) = (seen(), seen());
    let ca = sa.clone();
    let cb = sb.clone();
    watch(
        &ma,
        &[JvmtiEventKind::FramePop],
        EventCallbacks {
            frame_pop: Some(Box::new(move |t, m, _exc| ca.lock().unwrap().push((t, m)))),
            ..Default::default()
        },
    );
    watch(
        &mb,
        &[JvmtiEventKind::FramePop],
        EventCallbacks {
            frame_pop: Some(Box::new(move |t, m, _exc| cb.lock().unwrap().push((t, m)))),
            ..Default::default()
        },
    );

    let mut thread = test_thread("frame-pop-scoping");
    thread.frames.push(test_frame("popping"));
    let expected = synth_method_id(&thread.frames[0]);
    thread.frame_pop_requests.push(0);
    fire_jvmti_frame_pop_if_requested(a, &mut thread, false);

    assert_eq!(*sa.lock().unwrap(), vec![(thread.thread_id.0, expected)]);
    assert!(sb.lock().unwrap().is_empty());
    assert!(
        thread.frame_pop_requests.is_empty(),
        "NotifyFramePop is one-shot: the matching request must be consumed"
    );

    jvmti::forget_vm_jvmti_state(a);
    jvmti::forget_vm_jvmti_state(b);
}

/// Wave 10 (L4): a `NotifyFramePop` request is consumed when its frame
/// pops even if no FramePop listener is active then, so it cannot match a
/// later, unrelated frame at the same depth. A request for another depth
/// is kept.
#[test]
fn a_frame_pop_request_dies_with_its_frame_without_a_listener() {
    let _lock = jvmti::jvmti_registry_test_lock();
    // No manager is installed for this VM: nothing can be delivered.
    let vm = scoped_vm();
    let mut thread = test_thread("frame-pop-no-listener");
    thread.frames.push(test_frame("outer"));
    thread.frames.push(test_frame("popping"));
    thread.frame_pop_requests.push(0);
    thread.frame_pop_requests.push(1);
    fire_jvmti_frame_pop_if_requested(vm, &mut thread, false);
    assert_eq!(
        thread.frame_pop_requests,
        vec![0],
        "the popping frame's request must be consumed, the outer one kept"
    );
}

/// SingleStep reaches only the raising VM's agent, and within it only a
/// thread the event is enabled for: globally, or (wave 22) through the
/// manager's per-thread enable, which is what JVMTI's per-thread
/// `SetEventNotificationMode` sets. (The per-thread gate used to be
/// `JvmThread::single_step_enabled`, which production never raised.)
#[test]
fn single_step_reaches_only_the_raising_vms_agent() {
    let _lock = jvmti::jvmti_registry_test_lock();
    let (a, b) = (scoped_vm(), scoped_vm());
    let (ma, mb) = (manager_for(a), manager_for(b));
    let (sa, sb) = (seen(), seen());
    let ca = sa.clone();
    let cb = sb.clone();
    watch(
        &ma,
        &[JvmtiEventKind::SingleStep],
        EventCallbacks {
            single_step: Some(Box::new(move |t, m, _loc| ca.lock().unwrap().push((t, m)))),
            ..Default::default()
        },
    );
    watch(
        &mb,
        &[JvmtiEventKind::SingleStep],
        EventCallbacks {
            single_step: Some(Box::new(move |t, m, _loc| cb.lock().unwrap().push((t, m)))),
            ..Default::default()
        },
    );

    let mut thread = test_thread("single-step-scoping");
    thread.frames.push(test_frame("stepped"));
    let expected = synth_method_id(&thread.frames[0]);

    // Enabled globally on VM a: its agent hears it, VM b's does not,
    // with no per-thread flag raised anywhere.
    fire_jvmti_single_step(a, &mut thread, 0, 3);
    assert_eq!(*sa.lock().unwrap(), vec![(thread.thread_id.0, expected)]);
    assert!(sb.lock().unwrap().is_empty());

    // Enabled for ANOTHER thread only: this one's steps reach nobody.
    sa.lock().unwrap().clear();
    ma.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::SingleStep, None)
        .expect("disable");
    ma.set_event_notification_mode(
        EventMode::Enable,
        JvmtiEventKind::SingleStep,
        Some(thread.thread_id.0 + 1),
    )
    .expect("enable for one thread");
    fire_jvmti_single_step(a, &mut thread, 0, 4);
    assert!(
        sa.lock().unwrap().is_empty(),
        "the per-thread enable must be honoured"
    );
    // ...and for this thread: it does.
    ma.set_event_notification_mode(
        EventMode::Enable,
        JvmtiEventKind::SingleStep,
        Some(thread.thread_id.0),
    )
    .expect("enable for this thread");
    fire_jvmti_single_step(a, &mut thread, 0, 5);
    assert_eq!(*sa.lock().unwrap(), vec![(thread.thread_id.0, expected)]);
    assert!(sb.lock().unwrap().is_empty());

    jvmti::forget_vm_jvmti_state(a);
    jvmti::forget_vm_jvmti_state(b);
}

/// ExceptionCatch is the one helper that has only a `&Frame` — no thread,
/// no VM — so it is the likeliest to be reverted to the VM-less fire.
#[test]
fn exception_catch_reaches_only_the_raising_vms_agent() {
    let _lock = jvmti::jvmti_registry_test_lock();
    let (a, b) = (scoped_vm(), scoped_vm());
    let (ma, mb) = (manager_for(a), manager_for(b));
    let (sa, sb) = (seen(), seen());
    let ca = sa.clone();
    let cb = sb.clone();
    watch(
        &ma,
        &[JvmtiEventKind::ExceptionCatch],
        EventCallbacks {
            exception_catch: Some(Box::new(move |t, m, _loc, _exc| {
                ca.lock().unwrap().push((t, m))                })),
            ..Default::default()
        },
    );
    watch(
        &mb,
        &[JvmtiEventKind::ExceptionCatch],
        EventCallbacks {
            exception_catch: Some(Box::new(move |t, m, _loc, _exc| {
                cb.lock().unwrap().push((t, m))                })),
            ..Default::default()
        },
    );

    let catching_thread = 4321u64;
    let mut thread = JvmThread::new(ThreadId(catching_thread), "catcher");
    thread.frames.push(test_frame("catcher"));
    let expected = synth_method_id(&thread.frames[0]);
    fire_jvmti_exception_catch(b, &mut thread, 0, 17);

    assert_eq!(
        *sb.lock().unwrap(),
        vec![(catching_thread, expected)],
        "ExceptionCatch carries the catching thread's id (it used to be a constant 0), \
         and the VM must be exact"
    );
    assert!(sa.lock().unwrap().is_empty());

    jvmti::forget_vm_jvmti_state(a);
    jvmti::forget_vm_jvmti_state(b);
}

/// Wave 12: the catch hook resolves the method id only when the VM has
/// `ExceptionCatch` enabled for the thread, not whenever any agent
/// listens for anything (the union guard): an agent that enabled only
/// `Breakpoint` raises the union and must not make every caught
/// exception pay for the id.
#[test]
fn exception_catch_is_resolved_only_when_enabled() {
    let _lock = jvmti::jvmti_registry_test_lock();
    let vm = scoped_vm();
    let mgr = manager_for(vm);
    let seen = seen();
    let sink = seen.clone();
    watch(
        &mgr,
        &[JvmtiEventKind::Breakpoint],
        EventCallbacks {
            exception_catch: Some(Box::new(move |t, m, _loc, _exc| {
                sink.lock().unwrap().push((t, m))                })),
            ..Default::default()
        },
    );
    assert!(jvmti::any_listener_active(), "the union guard is up");
    assert!(!jvmti::exception_catch_enabled_for_vm(vm, 7));
    let mut thread = JvmThread::new(ThreadId(7), "catcher");
    thread.frames.push(test_frame("catcher"));
    fire_jvmti_exception_catch(vm, &mut thread, 0, 3);
    assert!(seen.lock().unwrap().is_empty());
    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::ExceptionCatch, None)
        .expect("enable ExceptionCatch");
    assert!(jvmti::exception_catch_enabled_for_vm(vm, 7));
    fire_jvmti_exception_catch(vm, &mut thread, 0, 3);
    assert_eq!(*seen.lock().unwrap(), vec![(7, synth_method_id(&thread.frames[0]))]);
    jvmti::forget_vm_jvmti_state(vm);
}

/// The load-bearing asymmetry: the `any_*_listener_active()` guards are a
/// process-wide **union**, so a VM with no agent at all still enters the
/// helper body when some *other* VM is listening. Delivery must then find
/// nothing. If a helper trusted the union flag instead of re-resolving the
/// row, this is the test that fails — and the bug it would be hiding is an
/// event delivered to an agent that never asked for it.
#[test]
fn a_union_guard_never_delivers_another_vms_event() {
    let _lock = jvmti::jvmti_registry_test_lock();
    let (quiet, loud) = (scoped_vm(), scoped_vm());
    let _quiet_mgr = manager_for(quiet); // installed, but subscribes to nothing
    let loud_mgr = manager_for(loud);
    let heard = seen();
    let (entry_sink, exit_sink) = (heard.clone(), heard.clone());
    watch(
        &loud_mgr,
        &[JvmtiEventKind::MethodEntry, JvmtiEventKind::MethodExit],
        EventCallbacks {
            method_entry: Some(Box::new(move |t, m| {
                entry_sink.lock().unwrap().push((t, m))
            })),
            method_exit: Some(Box::new(move |t, m, _exc, _rv| {
                exit_sink.lock().unwrap().push((t, m))                })),
            ..Default::default()
        },
    );

    // The union is true because `loud` is listening — that is the whole
    // point of the guard, and it is what puts `quiet`'s interpreter on the
    // slow path.
    assert!(
        jvmti::any_method_entry_listener_active(),
        "the union guard must be set while any VM listens"
    );
    assert!(
        !jvmti::any_method_entry_listener_active_for_vm(quiet),
        "the exact per-VM query must disagree with the union here"
    );

    let mut thread = test_thread("union-guard");
    thread.frames.push(test_frame("m"));
    fire_jvmti_method_exit_normal(quiet, &mut thread, 0, &None);
    push_frame_and_fire_entry(quiet, &mut thread, test_frame("m"));

    assert!(
        heard.lock().unwrap().is_empty(),
        "an over-approximating guard must not turn into an over-approximating delivery"
    );

    jvmti::forget_vm_jvmti_state(quiet);
    jvmti::forget_vm_jvmti_state(loud);
}

/// Field watchpoints: the four getfield/getstatic/putfield/putstatic sites
/// pass `shared.vm_identity`, so a watch armed in one VM must not fire on
/// the same `(class_id, field_index)` in another. `class_id` is only
/// unique *within* a VM, so this pair genuinely aliases.
#[test]
fn field_watchpoints_do_not_alias_across_vms() {
    let _lock = jvmti::jvmti_registry_test_lock();
    let (a, b) = (scoped_vm(), scoped_vm());
    let (ma, mb) = (manager_for(a), manager_for(b));
    let (sa, sb) = (seen(), seen());
    let ca = sa.clone();
    let cb = sb.clone();
    watch(
        &ma,
        &[JvmtiEventKind::FieldAccess],
        EventCallbacks {
            field_access: Some(Box::new(move |t, m, _f| ca.lock().unwrap().push((t, m)))),
            ..Default::default()
        },
    );
    watch(
        &mb,
        &[JvmtiEventKind::FieldAccess],
        EventCallbacks {
            field_access: Some(Box::new(move |t, m, _f| cb.lock().unwrap().push((t, m)))),
            ..Default::default()
        },
    );

    let (class_id, field_index) = (0xDEAD_BEEF_u64, 3usize);
    jvmti::set_field_watchpoint_for_vm(a, class_id, field_index, true, false)
        .expect("arming a watchpoint on a fresh row cannot fail");

    // Exactly the call the getstatic/getfield sites now make.
    jvmti::fire_field_access_if_watched_for_vm(b, 1234, 0x99, class_id, field_index);
    assert!(
        sb.lock().unwrap().is_empty(),
        "VM B has no watch on this (class_id, field_index) — B's ids mean different classes"
    );
    assert!(
        sa.lock().unwrap().is_empty(),
        "and B's access must certainly not be reported to A, which does watch it"
    );

    jvmti::fire_field_access_if_watched_for_vm(a, 1234, 0x99, class_id, field_index);
    assert_eq!(
        sa.lock().unwrap().len(),
        1,
        "A's own access must reach A's agent"
    );
    assert!(sb.lock().unwrap().is_empty());

    jvmti::forget_vm_jvmti_state(a);
    jvmti::forget_vm_jvmti_state(b);
}
