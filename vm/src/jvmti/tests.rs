use super::*;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

#[test]
fn test_create_jvmti_env() {
    let env = create_jvmti_env();
    assert!(!env.event_manager.is_enabled(JvmtiEvent::VMInit));
    assert!(env.c_table.agent_registry.agents().is_empty());
}

#[test]
fn test_notify_vm_init() {
    let mut env = create_jvmti_env();
    let counter = Arc::new(AtomicU32::new(0));
    let c = counter.clone();
    env.event_manager.callbacks.on_vm_init = Some(Box::new(move |data| {
        if let EventData::VMInit { thread_id } = data {
            assert_eq!(*thread_id, 1);
            c.fetch_add(1, Ordering::SeqCst);
        }
    }));
    env.event_manager
        .set_event_notification_mode(JvmtiEvent::VMInit, true);

    notify_vm_init(&env, 1);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[test]
fn test_notify_vm_death_no_listener() {
    let env = create_jvmti_env();
    // Should not crash even with no listeners.
    notify_vm_death(&env);
}

#[test]
fn test_notify_thread_start() {
    let mut env = create_jvmti_env();
    let counter = Arc::new(AtomicU32::new(0));
    let c = counter.clone();
    env.event_manager.callbacks.on_thread_start = Some(Box::new(move |data| {
        if let EventData::ThreadStart { thread_id, name } = data {
            assert_eq!(*thread_id, 5);
            assert_eq!(name, "worker-1");
            c.fetch_add(1, Ordering::SeqCst);
        }
    }));
    env.event_manager
        .set_event_notification_mode(JvmtiEvent::ThreadStart, true);

    notify_thread_start(&env, 5, "worker-1");
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[test]
fn test_full_workflow() {
    // Create env, load agent, set capabilities, enable events, fire.
    let mut env = create_jvmti_env();

    // Missing native agents fail closed and leave the registry unchanged.
    let err = env
        .c_table
        .agent_registry
        .load_agent("myagent.so", Some("debug=true"))
        .expect_err("missing native agent should fail closed");
    assert!(matches!(err, JvmtiError::AgentLibraryLoadFailed { .. }));
    assert!(env.c_table.agent_registry.agents().is_empty());

    // Enable events and fire. (The older model's capability request
    // step went with `JvmtiCapabilities` in wave 125.)
    let counter = Arc::new(AtomicU32::new(0));
    let c = counter.clone();
    env.event_manager.callbacks.on_garbage_collection_start = Some(Box::new(move |_| {
        c.fetch_add(1, Ordering::SeqCst);
    }));
    env.event_manager
        .set_event_notification_mode(JvmtiEvent::GarbageCollectionStart, true);

    notify_gc_start(&env);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[test]
fn test_notify_thread_end() {
    let mut env = create_jvmti_env();
    let counter = Arc::new(AtomicU32::new(0));
    let c = counter.clone();
    env.event_manager.callbacks.on_thread_end = Some(Box::new(move |data| {
        if let EventData::ThreadEnd { thread_id } = data {
            assert_eq!(*thread_id, 7);
            c.fetch_add(1, Ordering::SeqCst);
        }
    }));
    env.event_manager
        .set_event_notification_mode(JvmtiEvent::ThreadEnd, true);
    notify_thread_end(&env, 7);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[test]
fn test_notify_class_load() {
    let mut env = create_jvmti_env();
    let counter = Arc::new(AtomicU32::new(0));
    let c = counter.clone();
    env.event_manager.callbacks.on_class_load = Some(Box::new(move |data| {
        if let EventData::ClassLoad { class_id, name } = data {
            assert_eq!(*class_id, 42);
            assert_eq!(name, "java/lang/String");
            c.fetch_add(1, Ordering::SeqCst);
        }
    }));
    env.event_manager
        .set_event_notification_mode(JvmtiEvent::ClassLoad, true);
    notify_class_load(&env, 42, "java/lang/String");
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[test]
fn test_notify_method_exit() {
    let mut env = create_jvmti_env();
    let counter = Arc::new(AtomicU32::new(0));
    let c = counter.clone();
    env.event_manager.callbacks.on_method_exit = Some(Box::new(move |data| {
        if let EventData::MethodExit { return_value, .. } = data {
            assert_eq!(*return_value, Some(99));
            c.fetch_add(1, Ordering::SeqCst);
        }
    }));
    env.event_manager
        .set_event_notification_mode(JvmtiEvent::MethodExit, true);
    notify_method_exit(&env, 1, 10, 5, Some(99));
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[test]
fn test_notify_exception() {
    let mut env = create_jvmti_env();
    let counter = Arc::new(AtomicU32::new(0));
    let c = counter.clone();
    env.event_manager.callbacks.on_exception = Some(Box::new(move |data| {
        if let EventData::Exception {
            exception_class, ..
        } = data
        {
            assert_eq!(exception_class, "java/lang/NullPointerException");
            c.fetch_add(1, Ordering::SeqCst);
        }
    }));
    env.event_manager
        .set_event_notification_mode(JvmtiEvent::Exception, true);
    notify_exception(&env, 1, 10, 5, 42, "java/lang/NullPointerException");
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[test]
fn test_notify_monitor_wait() {
    let mut env = create_jvmti_env();
    let counter = Arc::new(AtomicU32::new(0));
    let c = counter.clone();
    env.event_manager.callbacks.on_monitor_wait = Some(Box::new(move |data| {
        if let EventData::MonitorWait { timeout, .. } = data {
            assert_eq!(*timeout, 5000);
            c.fetch_add(1, Ordering::SeqCst);
        }
    }));
    env.event_manager
        .set_event_notification_mode(JvmtiEvent::MonitorWait, true);
    notify_monitor_wait(&env, 1, 100, 5000);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[test]
fn test_notify_object_free() {
    let mut env = create_jvmti_env();
    let counter = Arc::new(AtomicU32::new(0));
    let c = counter.clone();
    env.event_manager.callbacks.on_object_free = Some(Box::new(move |data| {
        if let EventData::ObjectFree { tag } = data {
            assert_eq!(*tag, 77);
            c.fetch_add(1, Ordering::SeqCst);
        }
    }));
    env.event_manager
        .set_event_notification_mode(JvmtiEvent::ObjectFree, true);
    notify_object_free(&env, 77);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[test]
fn test_get_class_methods_trait() {
    struct MockProvider;
    impl ClassMethodProvider for MockProvider {
        fn get_methods(&self, class_id: u32) -> Option<Vec<JvmtiMethodInfo>> {
            if class_id == 1 {
                Some(vec![
                    JvmtiMethodInfo {
                        method_id: 100,
                        name: "toString".to_string(),
                        descriptor: "()Ljava/lang/String;".to_string(),
                        access_flags: 0x0001, // public
                    },
                    JvmtiMethodInfo {
                        method_id: 101,
                        name: "hashCode".to_string(),
                        descriptor: "()I".to_string(),
                        access_flags: 0x0001,
                    },
                ])
            } else {
                None
            }
        }
    }
    let provider = MockProvider;
    let methods = get_class_methods(&provider, 1).unwrap();
    assert_eq!(methods.len(), 2);
    assert_eq!(methods[0].name, "toString");
    assert_eq!(methods[1].name, "hashCode");

    let err = get_class_methods(&provider, 999);
    assert!(err.is_err());
}

#[test]
fn test_vm_class_method_provider() {
    // Test using a ClassManager with empty classpaths
    let cm = crate::classloading::ClassManager::new(&[], &[], &[]);
    let lock = parking_lot::RwLock::new(cm);
    let provider = VmClassMethodProvider {
        class_manager: &lock,
    };
    // Non-existent class returns None
    assert!(provider.get_methods(999999).is_none());
}

#[test]
fn test_notify_monitor_contended_enter() {
    let mut env = create_jvmti_env();
    let counter = Arc::new(AtomicU32::new(0));
    let c = counter.clone();
    env.event_manager.callbacks.on_monitor_contended_enter = Some(Box::new(move |data| {
        if let EventData::MonitorContendedEnter {
            thread_id,
            object_id,
        } = data
        {
            assert_eq!(*thread_id, 3);
            assert_eq!(*object_id, 0x5000);
            c.fetch_add(1, Ordering::SeqCst);
        }
    }));
    env.event_manager
        .set_event_notification_mode(JvmtiEvent::MonitorContendedEnter, true);
    notify_monitor_contended_enter(&env, 3, 0x5000);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}
