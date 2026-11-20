use super::*;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

#[test]
fn test_event_enable_disable() {
    let mut mgr = EventManager::new();
    assert!(!mgr.is_enabled(JvmtiEvent::VMInit));

    mgr.set_event_notification_mode(JvmtiEvent::VMInit, true);
    assert!(mgr.is_enabled(JvmtiEvent::VMInit));

    mgr.set_event_notification_mode(JvmtiEvent::VMInit, false);
    assert!(!mgr.is_enabled(JvmtiEvent::VMInit));
}

#[test]
fn test_fire_event_invokes_callback() {
    let mut mgr = EventManager::new();
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = counter.clone();

    mgr.callbacks.on_vm_init = Some(Box::new(move |_data| {
        counter_clone.fetch_add(1, Ordering::SeqCst);
    }));
    mgr.set_event_notification_mode(JvmtiEvent::VMInit, true);

    mgr.fire_event(JvmtiEvent::VMInit, &EventData::VMInit { thread_id: 1 });
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[test]
fn test_fire_disabled_event_no_crash() {
    let mgr = EventManager::new();
    // Event not enabled, no callback — should be a silent no-op.
    mgr.fire_event(JvmtiEvent::VMDeath, &EventData::VMDeath);
}

#[test]
fn test_fire_enabled_event_no_callback() {
    let mut mgr = EventManager::new();
    mgr.set_event_notification_mode(JvmtiEvent::ThreadStart, true);
    // Enabled but no callback registered — should not crash.
    mgr.fire_event(
        JvmtiEvent::ThreadStart,
        &EventData::ThreadStart {
            thread_id: 42,
            name: "main".into(),
        },
    );
}

#[test]
fn test_event_data_variants() {
    // Construct every variant to ensure they compile and can be debug-printed.
    let variants: Vec<EventData> = vec![
        EventData::VMInit { thread_id: 1 },
        EventData::VMDeath,
        EventData::ThreadStart {
            thread_id: 2,
            name: "worker".into(),
        },
        EventData::ThreadEnd { thread_id: 2 },
        EventData::ClassLoad {
            class_id: 10,
            name: "java/lang/Object".into(),
        },
        EventData::ClassPrepare {
            class_id: 10,
            name: "java/lang/Object".into(),
        },
        EventData::Breakpoint {
            thread_id: 1,
            class_id: 10,
            method_id: 5,
            location: 0,
        },
        EventData::MethodEntry {
            thread_id: 1,
            class_id: 10,
            method_id: 5,
        },
        EventData::MethodExit {
            thread_id: 1,
            class_id: 10,
            method_id: 5,
            return_value: Some(42),
        },
        EventData::Exception {
            thread_id: 1,
            class_id: 10,
            method_id: 5,
            location: 3,
            exception_class: "java/lang/NullPointerException".into(),
        },
        EventData::GarbageCollectionStart,
        EventData::GarbageCollectionFinish,
        EventData::MonitorWait {
            thread_id: 1,
            object_id: 100,
            timeout: 5000,
        },
        EventData::MonitorContendedEnter {
            thread_id: 1,
            object_id: 100,
        },
        EventData::ObjectFree { tag: 77 },
    ];
    for v in &variants {
        // Just ensure Debug works.
        let _ = format!("{:?}", v);
    }
    assert_eq!(variants.len(), 15);
}

#[test]
fn test_breakpoint_event_data() {
    let data = EventData::Breakpoint {
        thread_id: 7,
        class_id: 42,
        method_id: 3,
        location: 128,
    };
    match &data {
        EventData::Breakpoint {
            thread_id,
            class_id,
            method_id,
            location,
        } => {
            assert_eq!(*thread_id, 7);
            assert_eq!(*class_id, 42);
            assert_eq!(*method_id, 3);
            assert_eq!(*location, 128);
        }
        _ => panic!("expected Breakpoint variant"),
    }
}

#[test]
fn test_gc_event_pair() {
    let mut mgr = EventManager::new();
    let counter = Arc::new(AtomicU32::new(0));

    let c1 = counter.clone();
    mgr.callbacks.on_garbage_collection_start = Some(Box::new(move |_| {
        c1.fetch_add(1, Ordering::SeqCst);
    }));

    let c2 = counter.clone();
    mgr.callbacks.on_garbage_collection_finish = Some(Box::new(move |_| {
        c2.fetch_add(10, Ordering::SeqCst);
    }));

    mgr.set_event_notification_mode(JvmtiEvent::GarbageCollectionStart, true);
    mgr.set_event_notification_mode(JvmtiEvent::GarbageCollectionFinish, true);

    mgr.fire_event(
        JvmtiEvent::GarbageCollectionStart,
        &EventData::GarbageCollectionStart,
    );
    mgr.fire_event(
        JvmtiEvent::GarbageCollectionFinish,
        &EventData::GarbageCollectionFinish,
    );

    assert_eq!(counter.load(Ordering::SeqCst), 11);
}
