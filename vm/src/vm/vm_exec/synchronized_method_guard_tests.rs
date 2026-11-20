use super::*;
use crate::config::VmConfig;
use crate::threading::jvm_thread::{JvmThread, ThreadId};

fn fixture() -> (SharedVm, JvmThread, ObjectRef) {
    let shared = SharedVm::new(crate::VmConfig::default());
    let thread = JvmThread::new(ThreadId(7), "sync-guard-test");
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    (shared, thread, obj)
}

#[test]
fn drop_releases_the_monitor_and_pops_its_pin() {
    let (shared, mut thread, obj) = fixture();
    let tid = thread.thread_id;
    let pins_before = thread.native_pin_roots.len();
    let mut args = [Value::Object(Some(obj))];
    {
        let _guard = SynchronizedMethodGuard::enter(&shared, &mut thread, obj, &mut args);
        assert!(shared.threads.monitors.holds(obj, tid));
        assert_eq!(thread.native_pin_roots.len(), pins_before + 1);
    }
    assert!(!shared.threads.monitors.holds(obj, tid));
    assert_eq!(thread.native_pin_roots.len(), pins_before);
}

#[test]
fn transfer_hands_the_monitor_over_and_disarms_the_release() {
    let (shared, mut thread, obj) = fixture();
    let tid = thread.thread_id;
    let pins_before = thread.native_pin_roots.len();
    let mut args = [Value::Object(Some(obj))];
    let mut guard = SynchronizedMethodGuard::enter(&shared, &mut thread, obj, &mut args);
    assert_eq!(guard.transfer_to_frame(), Some(obj));
    assert_eq!(guard.transfer_to_frame(), None);
    drop(guard);
    // The frame owns it now: still held, pin gone.
    assert!(shared.threads.monitors.holds(obj, tid));
    assert_eq!(thread.native_pin_roots.len(), pins_before);
    assert!(monitor_exit_and_retract_jmx(&shared, obj, tid).is_ok());
    assert!(!shared.threads.monitors.holds(obj, tid));
}

#[test]
fn nested_guards_balance_a_reentrant_monitor() {
    let (shared, mut thread, obj) = fixture();
    let tid = thread.thread_id;
    let mut args = [Value::Object(Some(obj))];
    let outer = SynchronizedMethodGuard::enter(&shared, &mut thread, obj, &mut args);
    let inner = SynchronizedMethodGuard::enter(&shared, &mut thread, obj, &mut args);
    drop(inner);
    assert!(shared.threads.monitors.holds(obj, tid));
    drop(outer);
    assert!(!shared.threads.monitors.holds(obj, tid));
}
