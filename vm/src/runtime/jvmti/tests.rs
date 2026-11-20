use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

fn make_test_env() -> JvmtiEnv {
    JvmtiEnv::new()
}

// ---- Observability audit (2026-07-26) contract pins -------------------

/// `GetLocalVariable*` reads a side table that no production code writes.
/// This test pins the current, documented behaviour: on a fresh env there
/// are no frames to read. If someone wires locals to real interpreter
/// frames, this test SHOULD fail — and the gap note above the
/// local-variable section must be updated in the same change.
///
/// obsaudit D2 (2026-07-26): the capability gate was removed from these
/// methods (see the gap note) since `can_access_local_variables` can no
/// longer be granted through `add_capabilities` — see
/// `obsaudit_local_variable_capability_is_honestly_unavailable` for that
/// half. This test now exercises the side table directly, with no
/// capability negotiation step.
#[test]
fn obsaudit_get_local_reads_side_table_not_real_frames() {
    let env = make_test_env();

    // No `set_local_*` call has happened, and nothing else populates the
    // table, so there is no frame at any depth for any thread.
    assert_eq!(env.get_local_int(1, 0, 0), Err(JvmtiError::NoMoreFrames));
    assert_eq!(env.get_local_object(1, 0, 0), Err(JvmtiError::NoMoreFrames));

    // The table is writable, and reads see exactly what was written —
    // confirming it is a side table rather than a frame view.
    env.set_local_int(1, 0, 0, 0x5A5A).unwrap();
    assert_eq!(env.get_local_int(1, 0, 0), Ok(0x5A5A));
    // Typed access is enforced against the side table's own tag, which is
    // the one JVMTI-conformant behaviour that survives here.
    assert_eq!(env.get_local_long(1, 0, 0), Err(JvmtiError::TypeMismatch));
}

/// obsaudit D2: a caller that checks `GetPotentialCapabilities` before
/// `AddCapabilities` (the JVMTI-spec-correct order) must be told
/// up front that local-variable access is unavailable, and a caller
/// that requests it anyway must be refused — not granted and then left
/// to discover empty frames on its own.
#[test]
fn owned_monitor_capabilities_are_honestly_unavailable() {
    // Interpreter round i1 wave 25, lane L1: no function of this env
    // answers them, so neither may `AddCapabilities` grant them.
    let potential = JvmtiCapabilities::potentially_available();
    assert!(!potential.can_get_owned_monitor_info);
    assert!(!potential.can_get_owned_monitor_stack_depth_info);
    assert!(!potential.can_get_current_contended_monitor);
    let env = make_test_env();
    let result = env.add_capabilities(&JvmtiCapabilities {
        can_get_owned_monitor_info: true,
        ..Default::default()
    });
    assert_eq!(result, Err(JvmtiError::NotAvailable));
}

#[test]
fn obsaudit_local_variable_capability_is_honestly_unavailable() {
    assert!(!JvmtiCapabilities::potentially_available().can_access_local_variables);

    let env = make_test_env();
    let result = env.add_capabilities(&JvmtiCapabilities {
        can_access_local_variables: true,
        ..Default::default()
    });
    assert_eq!(result, Err(JvmtiError::NotAvailable));
    assert!(!env.get_capabilities().unwrap().can_access_local_variables);

    // Requesting other, genuinely-available capabilities alongside it
    // must still work — the rejection is specific to this one field.
    env.add_capabilities(&JvmtiCapabilities {
        can_suspend: true,
        ..Default::default()
    })
    .unwrap();
    assert!(env.get_capabilities().unwrap().can_suspend);
}

fn make_thread_info(name: &str) -> ThreadInfo {
    ThreadInfo {
        name: name.to_string(),
        priority: 5,
        is_daemon: false,
        thread_group_name: "main".to_string(),
        state: ThreadState(ThreadState::ALIVE | ThreadState::RUNNABLE),
    }
}

fn make_test_class(id: ClassId) -> ClassInfo {
    ClassInfo {
        class_id: id,
        name: format!("TestClass{}", id),
        bytecode: vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x34],
        is_prepared: true,
        fields: vec![
            FieldInfo {
                field_id: id * 100 + 1,
                name: "field1".to_string(),
                signature: "I".to_string(),
                modifiers: 1,
            },
            FieldInfo {
                field_id: id * 100 + 2,
                name: "field2".to_string(),
                signature: "Ljava/lang/String;".to_string(),
                modifiers: 1,
            },
        ],
        methods: vec![
            MethodInfo {
                method_id: id * 100 + 10,
                name: "method1".to_string(),
                signature: "()V".to_string(),
                modifiers: 1,
                declaring_class: id,
            },
            MethodInfo {
                method_id: id * 100 + 11,
                name: "method2".to_string(),
                signature: "(I)I".to_string(),
                modifiers: 1,
                declaring_class: id,
            },
        ],        }    }

#[test]
fn test_version_number() {
    let env = make_test_env();
    let version = env.get_version_number();
    assert_eq!(version, JVMTI_VERSION_11);
}

#[test]
fn test_error_names() {
    let env = make_test_env();
    assert_eq!(env.get_error_name(JvmtiError::None), "JVMTI_ERROR_NONE");
    assert_eq!(
        env.get_error_name(JvmtiError::InvalidThread),
        "JVMTI_ERROR_INVALID_THREAD"
    );
    assert_eq!(
        env.get_error_name(JvmtiError::OutOfMemory),
        "JVMTI_ERROR_OUT_OF_MEMORY"
    );
    assert_eq!(
        env.get_error_name(JvmtiError::MustPossessCapability),
        "JVMTI_ERROR_MUST_POSSESS_CAPABILITY"
    );
}

#[test]
fn test_error_from_code_roundtrip() {
    assert_eq!(JvmtiError::from_code(0), JvmtiError::None);
    assert_eq!(JvmtiError::from_code(10), JvmtiError::InvalidThread);
    assert_eq!(JvmtiError::from_code(99), JvmtiError::MustPossessCapability);
    assert_eq!(JvmtiError::from_code(9999), JvmtiError::Internal);
}

#[test]
fn test_event_notification_mode_global() {
    let env = make_test_env();
    assert!(!env
        .event_manager
        .is_event_enabled(JvmtiEventKind::VmInit, None));
    env.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::VmInit, None)
        .unwrap();
    assert!(env
        .event_manager
        .is_event_enabled(JvmtiEventKind::VmInit, None));
    env.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::VmInit, None)
        .unwrap();
    assert!(!env
        .event_manager
        .is_event_enabled(JvmtiEventKind::VmInit, None));
}

#[test]
fn test_event_notification_mode_per_thread() {
    let env = make_test_env();
    let tid = 42;
    env.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, Some(tid))
        .unwrap();
    assert!(env
        .event_manager
        .is_event_enabled(JvmtiEventKind::MethodEntry, Some(tid)));
    assert!(!env
        .event_manager
        .is_event_enabled(JvmtiEventKind::MethodEntry, Some(99)));
    assert!(!env
        .event_manager
        .is_event_enabled(JvmtiEventKind::MethodEntry, None));
}

#[test]
fn test_thread_lifecycle() {
    let env = make_test_env();
    env.register_thread(1, make_thread_info("main")).unwrap();
    env.register_thread(2, make_thread_info("worker")).unwrap();

    let threads = env.get_all_threads().unwrap();
    assert_eq!(threads.len(), 2);

    let info = env.get_thread_info(1).unwrap();
    assert_eq!(info.name, "main");

    env.unregister_thread(1).unwrap();
    assert!(env.get_thread_info(1).is_err());
    assert_eq!(env.get_all_threads().unwrap().len(), 1);
}

#[test]
fn test_thread_suspend_resume() {
    let env = make_test_env();
    env.add_capabilities(&JvmtiCapabilities {
        can_suspend: true,
        ..Default::default()
    })
    .unwrap();
    env.register_thread(1, make_thread_info("main")).unwrap();

    env.suspend_thread(1).unwrap();
    let state = env.get_thread_state(1).unwrap();
    assert_ne!(state.0 & ThreadState::SUSPENDED, 0);

    // Double suspend should fail
    assert_eq!(env.suspend_thread(1), Err(JvmtiError::ThreadSuspended));

    env.resume_thread(1).unwrap();
    let state = env.get_thread_state(1).unwrap();
    assert_eq!(state.0 & ThreadState::SUSPENDED, 0);

    // Resume without suspend should fail
    assert_eq!(env.resume_thread(1), Err(JvmtiError::ThreadNotSuspended));
}

#[test]
fn test_suspend_requires_capability() {
    let env = make_test_env();
    env.register_thread(1, make_thread_info("main")).unwrap();
    assert_eq!(
        env.suspend_thread(1),
        Err(JvmtiError::MustPossessCapability)
    );
}

#[test]
fn test_stack_trace() {
    let env = make_test_env();
    env.register_thread(1, make_thread_info("main")).unwrap();

    let frames = vec![
        FrameInfo {
            method_id: 100,
            class_id: 1,
            location: 0,
            method_name: "main".to_string(),
            class_name: "App".to_string(),
        },
        FrameInfo {
            method_id: 101,
            class_id: 1,
            location: 5,
            method_name: "run".to_string(),
            class_name: "App".to_string(),
        },
        FrameInfo {
            method_id: 102,
            class_id: 2,
            location: 10,
            method_name: "execute".to_string(),
            class_name: "Executor".to_string(),
        },
    ];
    env.set_stack_trace(1, frames).unwrap();

    assert_eq!(env.get_frame_count(1).unwrap(), 3);

    let trace = env.get_stack_trace(1, 0, 2).unwrap();
    assert_eq!(trace.len(), 2);
    assert_eq!(trace[0].method_name, "main");
    assert_eq!(trace[1].method_name, "run");

    let trace = env.get_stack_trace(1, 2, 10).unwrap();
    assert_eq!(trace.len(), 1);
    assert_eq!(trace[0].method_name, "execute");
}

#[test]
fn test_breakpoints() {
    let env = make_test_env();
    env.add_capabilities(&JvmtiCapabilities {
        can_generate_breakpoint_events: true,
        ..Default::default()
    })
    .unwrap();

    let loc = BreakpointLocation {
        class_id: 1,
        method_id: 100,
        location: 5,
    };
    env.set_breakpoint(loc.clone()).unwrap();
    assert!(env.has_breakpoint(&loc));

    // Duplicate should fail
    assert_eq!(env.set_breakpoint(loc.clone()), Err(JvmtiError::Duplicate));

    env.clear_breakpoint(&loc).unwrap();
    assert!(!env.has_breakpoint(&loc));

    // Clear non-existent should fail
    assert_eq!(env.clear_breakpoint(&loc), Err(JvmtiError::NotFound));
}

#[test]
fn test_field_watches() {
    let env = make_test_env();
    env.add_capabilities(&JvmtiCapabilities {
        can_generate_field_access_events: true,
        can_generate_field_modification_events: true,
        ..Default::default()
    })
    .unwrap();

    let watch = FieldWatch {
        class_id: 1,
        field_id: 10,
    };
    env.set_field_access_watch(watch.clone()).unwrap();
    assert_eq!(
        env.set_field_access_watch(watch.clone()),
        Err(JvmtiError::Duplicate)
    );
    env.clear_field_access_watch(&watch).unwrap();

    env.set_field_modification_watch(watch.clone()).unwrap();
    assert_eq!(
        env.set_field_modification_watch(watch.clone()),
        Err(JvmtiError::Duplicate)
    );
    env.clear_field_modification_watch(&watch).unwrap();
}

#[test]
fn test_local_variables() {
    // obsaudit D2: no capability negotiation needed — see the gap note
    // above `set_local_variable_table`.
    let env = make_test_env();

    let mut vars = HashMap::new();
    vars.insert(0, LocalValue::Int(42));
    vars.insert(1, LocalValue::Long(123456789));
    vars.insert(2, LocalValue::Float(3.14));
    vars.insert(3, LocalValue::Double(2.718281828));
    vars.insert(4, LocalValue::Object(Some(0xDEAD)));
    env.set_local_variable_table(1, 0, vars).unwrap();

    assert_eq!(env.get_local_int(1, 0, 0).unwrap(), 42);
    assert_eq!(env.get_local_long(1, 0, 1).unwrap(), 123456789);
    // The int, long and object slots on the lines around these are
    // asserted with `assert_eq!`; these two were the only slots in the
    // same round trip given a window, and 0.001 on 3.14 is a 0.03% one —
    // wide enough for a slot that stored the float as fixed point.
    assert_eq!(
        env.get_local_float(1, 0, 2).unwrap().to_bits(),
        3.14f32.to_bits()
    );
    assert_eq!(
        env.get_local_double(1, 0, 3).unwrap().to_bits(),
        2.718281828f64.to_bits()
    );
    assert_eq!(env.get_local_object(1, 0, 4).unwrap(), Some(0xDEAD));

    // Type mismatch
    assert_eq!(env.get_local_int(1, 0, 1), Err(JvmtiError::TypeMismatch));
    // Invalid slot
    assert_eq!(env.get_local_int(1, 0, 99), Err(JvmtiError::InvalidSlot));
}

#[test]
fn test_local_variable_set() {
    // obsaudit D2: no capability negotiation needed — see the gap note
    // above `set_local_variable_table`.
    let env = make_test_env();

    env.set_local_int(1, 0, 0, 99).unwrap();
    assert_eq!(env.get_local_int(1, 0, 0).unwrap(), 99);

    env.set_local_long(1, 0, 1, 999).unwrap();
    assert_eq!(env.get_local_long(1, 0, 1).unwrap(), 999);

    env.set_local_float(1, 0, 2, 1.5).unwrap();
    assert_eq!(
        env.get_local_float(1, 0, 2).unwrap().to_bits(),
        1.5f32.to_bits()
    );

    env.set_local_double(1, 0, 3, 2.5).unwrap();
    assert_eq!(
        env.get_local_double(1, 0, 3).unwrap().to_bits(),
        2.5f64.to_bits()
    );

    env.set_local_object(1, 0, 4, None).unwrap();
    assert_eq!(env.get_local_object(1, 0, 4).unwrap(), None);
}

#[test]
fn test_local_variables_no_longer_require_capability() {
    // obsaudit D2 (2026-07-26): renamed from
    // test_local_variables_require_capability, which pinned the
    // opposite behaviour. can_access_local_variables can no longer be
    // granted (see obsaudit_local_variable_capability_is_honestly_
    // unavailable), so gating these methods behind it would make the
    // side-table embedder/test surface permanently unusable. The gate
    // was removed instead — these methods now work with no capability
    // negotiation step, same as `set_local_variable_table` already did.
    let env = make_test_env(); // no capabilities granted
    assert_eq!(env.get_local_int(1, 0, 0), Err(JvmtiError::NoMoreFrames));
    assert_eq!(env.set_local_int(1, 0, 0, 1), Ok(()));
    assert_eq!(env.get_local_int(1, 0, 0), Ok(1));
}

#[test]
fn test_capabilities_add_and_relinquish() {
    let env = make_test_env();
    let caps = env.get_capabilities().unwrap();
    assert!(caps.is_empty());

    let requested = JvmtiCapabilities {
        can_generate_breakpoint_events: true,
        can_suspend: true,
        ..Default::default()
    };
    env.add_capabilities(&requested).unwrap();

    let caps = env.get_capabilities().unwrap();
    assert!(caps.can_generate_breakpoint_events);
    assert!(caps.can_suspend);
    assert!(!caps.can_redefine_classes);

    let to_drop = JvmtiCapabilities {
        can_suspend: true,
        ..Default::default()
    };
    env.relinquish_capabilities(&to_drop).unwrap();

    let caps = env.get_capabilities().unwrap();
    assert!(caps.can_generate_breakpoint_events);
    assert!(!caps.can_suspend);
}

#[test]
fn test_class_introspection() {
    let env = make_test_env();
    env.register_class(make_test_class(1)).unwrap();

    let fields = env.get_class_fields(1).unwrap();
    assert_eq!(fields.len(), 2);

    let methods = env.get_class_methods(1).unwrap();
    assert_eq!(methods.len(), 2);

    assert_eq!(env.get_method_name(110).unwrap(), "method1");
    assert_eq!(env.get_field_name(101).unwrap(), "field1");
    assert_eq!(env.get_method_declaring_class(110).unwrap(), 1);

    assert_eq!(env.get_method_name(999), Err(JvmtiError::InvalidMethodId));
    assert_eq!(env.get_field_name(999), Err(JvmtiError::InvalidFieldId));
    assert_eq!(env.get_class_fields(999), Err(JvmtiError::InvalidClass));
}

#[test]
fn test_redefine_classes() {
    let env = make_test_env();
    env.add_capabilities(&JvmtiCapabilities {
        can_redefine_classes: true,
        ..Default::default()
    })
    .unwrap();
    env.register_class(make_test_class(1)).unwrap();

    let new_bytes = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x37, 0xFF];
    env.redefine_classes(&[(1, new_bytes.clone())]).unwrap();

    let classes = env.classes.read().unwrap();
    assert_eq!(classes.get(&1).unwrap().bytecode, new_bytes);
}

#[test]
fn test_redefine_invalid_classfile() {
    let env = make_test_env();
    env.add_capabilities(&JvmtiCapabilities {
        can_redefine_classes: true,
        ..Default::default()
    })
    .unwrap();
    env.register_class(make_test_class(1)).unwrap();

    // Invalid magic number
    let bad_bytes = vec![0x00, 0x00, 0x00, 0x00];
    assert_eq!(
        env.redefine_classes(&[(1, bad_bytes)]),
        Err(JvmtiError::InvalidClassFormat)
    );

    // Too short
    let short_bytes = vec![0xCA, 0xFE];
    assert_eq!(
        env.redefine_classes(&[(1, short_bytes)]),
        Err(JvmtiError::InvalidClassFormat)
    );
}

#[test]
fn test_retransform_classes() {
    let env = make_test_env();
    env.add_capabilities(&JvmtiCapabilities {
        can_retransform_classes: true,
        ..Default::default()
    })
    .unwrap();
    env.register_class(make_test_class(1)).unwrap();

    // Set a transformer that appends a byte
    env.set_retransform_hook(|_class_id, bytes| {
        let mut new = bytes.to_vec();
        new.push(0xAA);
        new
    })
    .unwrap();

    env.retransform_classes(&[1]).unwrap();

    let classes = env.classes.read().unwrap();
    let bytecode = &classes.get(&1).unwrap().bytecode;
    assert_eq!(bytecode.last(), Some(&0xAA));
}

#[test]
fn test_system_properties() {
    let env = make_test_env();
    env.set_system_property("java.version", "11.0.1").unwrap();
    env.set_system_property("os.name", "Linux").unwrap();

    assert_eq!(env.get_system_property("java.version").unwrap(), "11.0.1");
    assert_eq!(env.get_system_property("os.name").unwrap(), "Linux");
    assert_eq!(
        env.get_system_property("nonexistent"),
        Err(JvmtiError::NotFound)
    );

    env.set_system_property("java.version", "17.0.1").unwrap();
    assert_eq!(env.get_system_property("java.version").unwrap(), "17.0.1");
}

#[test]
fn test_system_properties_bulk_load() {
    let env = make_test_env();
    env.load_system_properties(&[
        ("a".to_string(), "1".to_string()),
        ("b".to_string(), "2".to_string()),
    ])
    .unwrap();
    assert_eq!(env.get_system_property("a").unwrap(), "1");
    assert_eq!(env.get_system_property("b").unwrap(), "2");
}

#[test]
fn test_event_callbacks_fire() {
    let env = make_test_env();
    env.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::VmInit, None)
        .unwrap();
    env.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::ThreadStart, None)
        .unwrap();

    let init_count = Arc::new(AtomicU32::new(0));
    let thread_count = Arc::new(AtomicU32::new(0));
    let ic = init_count.clone();
    let tc = thread_count.clone();

    let cbs = EventCallbacks {
        vm_init: Some(Box::new(move || {
            ic.fetch_add(1, Ordering::SeqCst);
        })),
        thread_start: Some(Box::new(move |_tid| {
            tc.fetch_add(1, Ordering::SeqCst);            })),
        ..Default::default()
    };
    env.event_manager.set_event_callbacks(cbs).unwrap();

    env.event_manager.fire_vm_init();
    env.event_manager.fire_vm_init();
    env.event_manager.fire_thread_start(1);

    assert_eq!(init_count.load(Ordering::SeqCst), 2);
    assert_eq!(thread_count.load(Ordering::SeqCst), 1);
    assert_eq!(env.event_manager.event_count(JvmtiEventKind::VmInit), 2);
    assert_eq!(
        env.event_manager.event_count(JvmtiEventKind::ThreadStart),
        1
    );
}

#[test]
fn test_event_disabled_does_not_fire() {
    let env = make_test_env();
    // Do NOT enable VmDeath
    let count = Arc::new(AtomicU32::new(0));
    let c = count.clone();
    let cbs = EventCallbacks {
        vm_death: Some(Box::new(move || {
            c.fetch_add(1, Ordering::SeqCst);            })),
        ..Default::default()
    };
    env.event_manager.set_event_callbacks(cbs).unwrap();
    env.event_manager.fire_vm_death();
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(env.event_manager.event_count(JvmtiEventKind::VmDeath), 0);
}

#[test]
fn test_class_file_load_hook_transform() {
    let env = make_test_env();
    env.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::ClassFileLoadHook, None)
        .unwrap();

    let cbs = EventCallbacks {
        class_file_load_hook: Some(Box::new(|_cid, _name, bytes| {
            let mut new = bytes.to_vec();
            new.push(0xBB);
            Some(new)            })),
        ..Default::default()
    };
    env.event_manager.set_event_callbacks(cbs).unwrap();

    let original = vec![1, 2, 3];
    let result = env
        .event_manager
        .fire_class_file_load_hook(1, "TestClass", &original);
    assert_eq!(result, Some(vec![1, 2, 3, 0xBB]));
}

#[test]
fn test_force_gc_with_trigger() {
    let env = make_test_env();
    let triggered = Arc::new(AtomicU32::new(0));
    let t = triggered.clone();
    env.set_gc_trigger(move || {
        t.fetch_add(1, Ordering::SeqCst);
        true
    })
    .unwrap();

    env.force_garbage_collection().unwrap();
    assert_eq!(triggered.load(Ordering::SeqCst), 1);
}

#[test]
fn test_force_gc_without_trigger() {
    let env = make_test_env();
    // No GC trigger registered — should succeed (best-effort)
    assert!(env.force_garbage_collection().is_ok());
}

#[test]
fn test_capabilities_potentially_available() {
    let all = JvmtiCapabilities::potentially_available();
    assert!(all.can_redefine_classes);
    assert!(all.can_retransform_classes);
    // obsaudit D2: NOT potentially available — see
    // obsaudit_local_variable_capability_is_honestly_unavailable.
    assert!(!all.can_access_local_variables);
    assert!(all.can_suspend);
    assert!(all.can_generate_breakpoint_events);
}

#[test]
fn test_capabilities_union_and_subtract() {
    let a = JvmtiCapabilities {
        can_suspend: true,
        can_redefine_classes: true,
        ..Default::default()
    };
    let b = JvmtiCapabilities {
        can_suspend: true,
        can_retransform_classes: true,
        ..Default::default()
    };

    let union = a.union(&b);
    assert!(union.can_suspend);
    assert!(union.can_redefine_classes);
    assert!(union.can_retransform_classes);

    let diff = union.subtract(&b);
    assert!(!diff.can_suspend);
    assert!(diff.can_redefine_classes);
    assert!(!diff.can_retransform_classes);
}

#[test]
fn test_event_kind_from_raw() {
    assert_eq!(JvmtiEventKind::from_raw(50), Some(JvmtiEventKind::VmInit));
    assert_eq!(JvmtiEventKind::from_raw(51), Some(JvmtiEventKind::VmDeath));
    assert_eq!(
        JvmtiEventKind::from_raw(62),
        Some(JvmtiEventKind::Breakpoint)
    );
    assert_eq!(JvmtiEventKind::from_raw(0), None);
    assert_eq!(JvmtiEventKind::from_raw(9999), None);
}

/// Wave 13: every kind carries its `jvmti.h` `jvmtiEvent` number, and no
/// two share one.
#[test]
fn event_kinds_carry_the_jvmti_h_numbers() {
    use JvmtiEventKind as K;
    let expected = [
        (K::VmInit, 50),
        (K::VmDeath, 51),
        (K::ThreadStart, 52),
        (K::ThreadEnd, 53),
        (K::ClassFileLoadHook, 54),
        (K::ClassLoad, 55),
        (K::ClassPrepare, 56),
        (K::Exception, 58),
        (K::ExceptionCatch, 59),
        (K::SingleStep, 60),
        (K::FramePop, 61),
        (K::Breakpoint, 62),
        (K::FieldAccess, 63),
        (K::FieldModification, 64),
        (K::MethodEntry, 65),
        (K::MethodExit, 66),
        (K::NativeMethodBind, 67),
        (K::CompiledMethodLoad, 68),
        (K::CompiledMethodUnload, 69),
        (K::DynamicCodeGenerated, 70),
        (K::DataDumpRequest, 71),
        (K::MonitorWait, 73),
        (K::MonitorWaited, 74),
        (K::MonitorContendedEnter, 75),
        (K::MonitorContendedEntered, 76),
        (K::GarbageCollectionStart, 81),
        (K::GarbageCollectionFinish, 82),
        (K::ObjectFree, 83),
        (K::VMObjectAlloc, 84),
        (K::SampledObjectAlloc, 86),
    ];
    assert_eq!(expected.len(), JvmtiEventKind::ALL.len());
    for (kind, number) in expected {
        assert_eq!(kind as u32, number, "{kind:?}");
        assert_eq!(JvmtiEventKind::from_raw(number), Some(kind));
    }
}

#[test]
fn test_event_kind_all_count() {
    // Verify ALL contains every variant
    assert_eq!(JvmtiEventKind::ALL.len(), 30);
}

#[test]
fn test_jvmti_env_debug_format() {
    let env = make_test_env();
    let debug = format!("{:?}", env);
    assert!(debug.contains("JvmtiEnv"));
    assert!(debug.contains("version"));
}

#[test]
fn test_multiple_events_independently_tracked() {
    let em = JvmtiEventManager::new();
    em.set_event_notification_mode(
        EventMode::Enable,
        JvmtiEventKind::GarbageCollectionStart,
        None,
    )
    .unwrap();
    em.set_event_notification_mode(
        EventMode::Enable,
        JvmtiEventKind::GarbageCollectionFinish,
        None,
    )
    .unwrap();

    let cbs = EventCallbacks {
        gc_start: Some(Box::new(|| {})),
        gc_finish: Some(Box::new(|| {})),
        ..Default::default()
    };
    em.set_event_callbacks(cbs).unwrap();

    em.fire_gc_start();
    em.fire_gc_start();
    em.fire_gc_finish();

    assert_eq!(em.event_count(JvmtiEventKind::GarbageCollectionStart), 2);
    assert_eq!(em.event_count(JvmtiEventKind::GarbageCollectionFinish), 1);
}

// ----------------------------------------------------------------
// T6.3.1 — Tests for the four newly added event kinds and the
// global manager wiring used by the vm/gc/classloading crates.
// ----------------------------------------------------------------

#[test]
fn test_new_event_kinds_in_all() {
    // The four new kinds must appear in the ALL iteration set.
    assert!(JvmtiEventKind::ALL.contains(&JvmtiEventKind::ObjectFree));
    assert!(JvmtiEventKind::ALL.contains(&JvmtiEventKind::VMObjectAlloc));
    assert!(JvmtiEventKind::ALL.contains(&JvmtiEventKind::SampledObjectAlloc));
    assert!(JvmtiEventKind::ALL.contains(&JvmtiEventKind::DataDumpRequest));
}

#[test]
fn test_new_event_kinds_from_raw_roundtrip() {
    // Round-trip the four new kinds through the raw u32 encoding.
    assert_eq!(
        JvmtiEventKind::from_raw(83),
        Some(JvmtiEventKind::ObjectFree)
    );
    assert_eq!(
        JvmtiEventKind::from_raw(84),
        Some(JvmtiEventKind::VMObjectAlloc)
    );
    assert_eq!(
        JvmtiEventKind::from_raw(86),
        Some(JvmtiEventKind::SampledObjectAlloc)
    );
    assert_eq!(
        JvmtiEventKind::from_raw(71),
        Some(JvmtiEventKind::DataDumpRequest)
    );

    assert_eq!(JvmtiEventKind::ObjectFree as u32, 83);
    assert_eq!(JvmtiEventKind::VMObjectAlloc as u32, 84);
    assert_eq!(JvmtiEventKind::SampledObjectAlloc as u32, 86);
    assert_eq!(JvmtiEventKind::DataDumpRequest as u32, 71);
}

#[test]
fn test_new_event_kinds_debug_eq_hash() {
    use std::collections::HashSet;
    let mut set: HashSet<JvmtiEventKind> = HashSet::new();
    set.insert(JvmtiEventKind::ObjectFree);
    set.insert(JvmtiEventKind::VMObjectAlloc);
    set.insert(JvmtiEventKind::SampledObjectAlloc);
    set.insert(JvmtiEventKind::DataDumpRequest);
    assert_eq!(set.len(), 4);
    // Debug round-trip sanity.
    assert_eq!(format!("{:?}", JvmtiEventKind::ObjectFree), "ObjectFree");
    assert_eq!(
        format!("{:?}", JvmtiEventKind::DataDumpRequest),
        "DataDumpRequest"
    );
}

#[test]
fn test_fire_object_free() {
    let em = JvmtiEventManager::new();
    em.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::ObjectFree, None)
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::<i64>::new()));
    let s = seen.clone();
    let cbs = EventCallbacks {
        object_free: Some(Box::new(move |tag| s.lock().unwrap().push(tag))),
        ..Default::default()
    };
    em.set_event_callbacks(cbs).unwrap();
    em.fire_object_free(42);
    em.fire_object_free(7);
    assert_eq!(*seen.lock().unwrap(), vec![42, 7]);
    assert_eq!(em.event_count(JvmtiEventKind::ObjectFree), 2);
}

#[test]
fn test_fire_vm_object_alloc() {
    let em = JvmtiEventManager::new();
    let tid: ThreadId = 1;
    em.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::VMObjectAlloc, Some(tid))
        .unwrap();
    let count = Arc::new(AtomicU32::new(0));
    let total_size = Arc::new(Mutex::new(0usize));
    let c = count.clone();
    let t = total_size.clone();
    let cbs = EventCallbacks {
        vm_object_alloc: Some(Box::new(move |_tid, _addr, _cls, sz| {
            c.fetch_add(1, Ordering::SeqCst);
            *t.lock().unwrap() += sz;            })),
        ..Default::default()
    };
    em.set_event_callbacks(cbs).unwrap();
    em.fire_vm_object_alloc(tid, 0xdead, 10, 32);
    em.fire_vm_object_alloc(tid, 0xbeef, 11, 48);
    assert_eq!(count.load(Ordering::SeqCst), 2);
    assert_eq!(*total_size.lock().unwrap(), 80);
}

#[test]
fn test_sampled_alloc_threshold_rate_limits() {
    let em = JvmtiEventManager::new();
    let tid: ThreadId = 1;
    em.set_event_notification_mode(
        EventMode::Enable,
        JvmtiEventKind::SampledObjectAlloc,
        Some(tid),
    )
    .unwrap();
    em.set_sampling_interval(1024);
    let count = Arc::new(AtomicU32::new(0));
    let c = count.clone();
    let cbs = EventCallbacks {
        sampled_object_alloc: Some(Box::new(move |_t, _a, _c, _s| {
            c.fetch_add(1, Ordering::SeqCst);            })),
        ..Default::default()
    };
    em.set_event_callbacks(cbs).unwrap();

    // 8 × 128 B = 1024 B — crosses threshold exactly once.
    let mut fired = 0u32;
    for _ in 0..8 {
        if em.record_allocation_sample(tid, 0x1000, 1, 128) {
            fired += 1;
        }
    }
    assert_eq!(
        fired, 1,
        "threshold of 1024 should fire exactly once for 8×128 B"
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);

    // Reset counter — 4 × 256 B hits threshold once more.
    for _ in 0..4 {
        em.record_allocation_sample(tid, 0x2000, 2, 256);
    }
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

#[test]
fn test_fire_data_dump_request() {
    let em = JvmtiEventManager::new();
    em.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::DataDumpRequest, None)
        .unwrap();
    let count = Arc::new(AtomicU32::new(0));
    let c = count.clone();
    let cbs = EventCallbacks {
        data_dump_request: Some(Box::new(move || {
            c.fetch_add(1, Ordering::SeqCst);            })),
        ..Default::default()
    };
    em.set_event_callbacks(cbs).unwrap();
    em.fire_data_dump_request();
    em.fire_data_dump_request();
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

#[test]
fn test_no_listener_fast_path() {
    let em = JvmtiEventManager::new();
    // No agents, no events enabled, no callbacks set — has_any_listener
    // must be false and fire_ methods must be cheap no-ops.
    assert!(!em.has_any_listener());
    em.fire_vm_object_alloc(1, 0, 5, 32);
    em.fire_object_free(99);
    em.fire_data_dump_request();
    assert_eq!(em.event_count(JvmtiEventKind::VMObjectAlloc), 0);
    assert_eq!(em.event_count(JvmtiEventKind::ObjectFree), 0);
    assert_eq!(em.event_count(JvmtiEventKind::DataDumpRequest), 0);
}

#[test]
fn test_env_registration_broadcasts_callbacks() {
    let em = Arc::new(JvmtiEventManager::new());
    em.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::ObjectFree, None)
        .unwrap();

    // Attached env with its own ObjectFree callback, enabled on its own
    // state (wave 14: enabling is per env).
    let env = Arc::new(JvmtiEnv::new());
    let agent_tags = Arc::new(Mutex::new(Vec::<i64>::new()));
    let t = agent_tags.clone();
    env.event_manager
        .set_event_callbacks(EventCallbacks {
            object_free: Some(Box::new(move |tag| t.lock().unwrap().push(tag))),
            ..Default::default()
        })
        .unwrap();
    env.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::ObjectFree, None)
        .unwrap();

    em.register_env(&env).unwrap();
    em.fire_object_free(123);
    em.fire_object_free(456);

    // The attached env's callback should have received both tags.
    assert_eq!(*agent_tags.lock().unwrap(), vec![123, 456]);

    // Unregister — further fires should not reach the env.
    em.unregister_env(&env).unwrap();
    em.fire_object_free(789);
    assert_eq!(*agent_tags.lock().unwrap(), vec![123, 456]);
}

/// ClassLoad / ClassPrepare must reach attached envs, not just the
/// manager's own callback table.
///
/// Both events used to dispatch only to `self.callbacks`, so an agent
/// holding its own `JvmtiEnv` silently received neither — no error, no
/// diagnostic, just nothing. That contradicted the delivery contract
/// documented on `JvmtiEventManager`. This pins the fix; see the same doc
/// comment for the events that still lack per-env delivery.
#[test]
fn test_env_receives_class_load_and_prepare() {
    let em = Arc::new(JvmtiEventManager::new());
    let tid: ThreadId = 3;
    let env = Arc::new(JvmtiEnv::new());
    env.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::ClassLoad, Some(tid))
        .unwrap();
    env.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::ClassPrepare, Some(tid))
        .unwrap();
    let loaded = Arc::new(Mutex::new(Vec::<ClassId>::new()));
    let prepared = Arc::new(Mutex::new(Vec::<ClassId>::new()));
    let (l, p) = (loaded.clone(), prepared.clone());
    env.event_manager
        .set_event_callbacks(EventCallbacks {
            class_load: Some(Box::new(move |_t, c| l.lock().unwrap().push(c))),
            class_prepare: Some(Box::new(move |_t, c| p.lock().unwrap().push(c))),
            ..Default::default()
        })
        .unwrap();

    em.register_env(&env).unwrap();
    em.fire_class_load(tid, 11);
    em.fire_class_load(tid, 22);
    em.fire_class_prepare(tid, 11);

    assert_eq!(*loaded.lock().unwrap(), vec![11, 22]);
    assert_eq!(*prepared.lock().unwrap(), vec![11]);

    // Unregistering must stop delivery, same as every other per-env event.
    em.unregister_env(&env).unwrap();
    em.fire_class_load(tid, 33);
    em.fire_class_prepare(tid, 33);
    assert_eq!(*loaded.lock().unwrap(), vec![11, 22]);
    assert_eq!(*prepared.lock().unwrap(), vec![11]);
}

/// Wave 13: `VMInit` reaches the attached envs as well as the manager's
/// own callback.
#[test]
fn vm_init_reaches_every_attached_env() {
    let em = Arc::new(JvmtiEventManager::new());
    em.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::VmInit, None)
        .unwrap();
    let own = Arc::new(AtomicU32::new(0));
    let own_seen = own.clone();
    em.set_event_callbacks(EventCallbacks {
        vm_init: Some(Box::new(move || {
            own_seen.fetch_add(1, Ordering::SeqCst);            })),
        ..Default::default()
    })
    .unwrap();
    let env = Arc::new(JvmtiEnv::new());
    let inits = Arc::new(AtomicU32::new(0));
    let seen = inits.clone();
    env.event_manager
        .set_event_callbacks(EventCallbacks {
            vm_init: Some(Box::new(move || {
                seen.fetch_add(1, Ordering::SeqCst);                })),
            ..Default::default()
        })
        .unwrap();
    env.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::VmInit, None)
        .unwrap();
    em.register_env(&env).unwrap();
    em.fire_vm_init();
    assert_eq!(inits.load(Ordering::SeqCst), 1);
    em.unregister_env(&env).unwrap();
    em.fire_vm_init();
    assert_eq!(inits.load(Ordering::SeqCst), 1);
    assert_eq!(own.load(Ordering::SeqCst), 2);
}

/// Wave 14: enabling is per listener. An attached env receives only what
/// its own state enables, the manager's own callback only what the
/// manager's state enables; the fast-path flags and `is_event_enabled`
/// are the union and fall again when the env disables the event or is
/// detached.
#[test]
fn each_listener_receives_only_the_events_it_enabled() {
    let em = Arc::new(JvmtiEventManager::new());
    let own = Arc::new(AtomicU32::new(0));
    let own_seen = own.clone();
    // The manager's own table has an `Exception` callback but never
    // enables the event.
    em.set_event_callbacks(EventCallbacks {
        exception: Some(Box::new(move |_, _, _, _, _, _| {
            own_seen.fetch_add(1, Ordering::SeqCst);            })),
        ..Default::default()
    })
    .unwrap();
    let (first, second) = (Arc::new(JvmtiEnv::new()), Arc::new(JvmtiEnv::new()));
    let hits = [Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0))];
    for (env, count) in [(&first, &hits[0]), (&second, &hits[1])] {
        let count = count.clone();
        env.event_manager
            .set_event_callbacks(EventCallbacks {
                exception: Some(Box::new(move |_, _, _, _, _, _| {
                    count.fetch_add(1, Ordering::SeqCst);
                })),
                ..Default::default()
            })
            .unwrap();
        em.register_env(env).unwrap();
    }
    assert!(!em.has_exception_listener(), "nothing enabled yet");
    assert!(!em.has_any_listener(), "attached envs enabled nothing");

    // Only the first env enables it, and only for thread 7.
    first
        .set_event_notification_mode(EventMode::Enable, JvmtiEventKind::Exception, Some(7))
        .unwrap();
    assert!(
        em.has_exception_listener(),
        "the env's enable joins the union"
    );
    assert!(em.is_event_enabled(JvmtiEventKind::Exception, Some(7)));
    assert!(!em.is_event_enabled(JvmtiEventKind::Exception, Some(8)));
    em.fire_exception(7, 1, 0, 0, 0, -1);
    em.fire_exception(8, 1, 0, 0, 0, -1);
    assert_eq!(hits[0].load(Ordering::SeqCst), 1, "thread 7 only");
    assert_eq!(hits[1].load(Ordering::SeqCst), 0, "never enabled it");
    assert_eq!(
        own.load(Ordering::SeqCst),
        0,
        "the manager never enabled it"
    );

    // Disabled on the env: the union falls.
    first
        .set_event_notification_mode(EventMode::Disable, JvmtiEventKind::Exception, Some(7))
        .unwrap();
    assert!(!em.has_exception_listener());
    assert!(!em.is_event_enabled(JvmtiEventKind::Exception, Some(7)));
    em.fire_exception(7, 1, 0, 0, 0, -1);
    assert_eq!(hits[0].load(Ordering::SeqCst), 1);

    // Enabled on the second env globally, then that env is detached.
    second
        .event_manager
        .set_event_enablement(JvmtiEventKind::Exception, true, &[])
        .unwrap();
    assert!(em.has_exception_listener());
    em.fire_exception(9, 1, 0, 0, 0, -1);
    assert_eq!(hits[1].load(Ordering::SeqCst), 1);
    em.unregister_env(&second).unwrap();
    assert!(!em.has_exception_listener(), "detaching drops its enable");
    em.fire_exception(9, 1, 0, 0, 0, -1);
    assert_eq!(hits[1].load(Ordering::SeqCst), 1);
    assert_eq!(own.load(Ordering::SeqCst), 0);
    em.unregister_env(&first).unwrap();
}

/// `set_event_enablement` replaces the whole enable state of one kind.
#[test]
fn event_enablement_replaces_the_global_and_thread_state_of_one_kind() {
    let em = JvmtiEventManager::new();
    em.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::Breakpoint, Some(1))
        .unwrap();
    em.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::SingleStep, Some(1))
        .unwrap();
    em.set_event_enablement(JvmtiEventKind::Breakpoint, false, &[2])
        .unwrap();
    assert!(!em.is_event_enabled(JvmtiEventKind::Breakpoint, Some(1)));
    assert!(em.is_event_enabled(JvmtiEventKind::Breakpoint, Some(2)));
    assert!(
        em.is_event_enabled(JvmtiEventKind::SingleStep, Some(1)),
        "another kind is untouched"
    );
    em.set_event_enablement(JvmtiEventKind::Breakpoint, true, &[])
        .unwrap();
    assert!(em.is_event_enabled(JvmtiEventKind::Breakpoint, Some(5)));
    em.set_event_enablement(JvmtiEventKind::Breakpoint, false, &[])
        .unwrap();
    assert!(!em.is_event_enabled(JvmtiEventKind::Breakpoint, Some(2)));
    assert!(em.has_single_step_listener());
    em.set_event_enablement(JvmtiEventKind::SingleStep, false, &[])
        .unwrap();
    assert!(!em.has_single_step_listener());
    assert!(!em.has_any_listener(), "nothing enabled on any listener");
}

#[test]
fn test_env_dropped_envs_pruned() {
    let em = Arc::new(JvmtiEventManager::new());
    em.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::DataDumpRequest, None)
        .unwrap();

    {
        let env = Arc::new(JvmtiEnv::new());
        em.register_env(&env).unwrap();
        // Env drops here.
    }
    // Snapshot after drop should be empty; no panic.
    let snap = em.snapshot_envs();
    assert!(
        snap.is_empty(),
        "dropped envs must be pruned from attached list"
    );

    em.fire_data_dump_request(); // no listener callback, just no panic.
}

// ----------------------------------------------------------------
// Global manager & wired-safepoint tests
// ----------------------------------------------------------------

/// Serialize test access to the unattributed row and the process-wide
/// union mirrors. Delegates to the module-scope
/// [`super::jvmti_registry_test_lock`] so that `runtime::interpreter`'s
/// delivery tests — which move the same union mirrors — serialise against
/// these tests and not merely against each other.
fn global_test_lock() -> std::sync::MutexGuard<'static, ()> {
    super::jvmti_registry_test_lock()
}

/// Ensure the process-wide manager exists, and return a handle. Tests
/// that mutate global state must call `reset_global_manager_for_tests`
/// after acquiring the test lock to get a clean slate.
fn ensure_global_manager() -> Arc<JvmtiEventManager> {
    install_global_manager(Arc::new(JvmtiEventManager::new()));
    // `install_global_manager` is idempotent: if another test got
    // there first, `global_manager()` returns the existing one.
    global_manager().expect("global manager must be installed")
}

#[test]
fn test_global_manager_no_install_is_noop() {
    // Even without a manager being touched, the free wrappers must not
    // panic and any_listener_active should return false OR true depending
    // on prior tests. We assert only the no-panic and the fires being
    // harmless.
    fire_gc_start();
    fire_gc_finish();
    fire_vm_init();
    fire_vm_death();
    fire_class_load(1, 1);
    fire_class_prepare(1, 1);
    fire_exception(1, 1, 0, 0, 0, -1);
    fire_exception_catch(1, 1, 0, 0);
    fire_object_free(0);
    fire_vm_object_alloc(1, 0, 1, 0);
    fire_data_dump_request();
    let _ = record_allocation_sample(1, 0, 1, 0);
    let _ = any_listener_active();
}

#[test]
fn test_global_wired_class_load_fires() {
    let _lock = global_test_lock();
    let mgr = ensure_global_manager();
    reset_global_manager_for_tests();

    let tid: ThreadId = 7;
    let cid: ClassId = 42;
    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::ClassLoad, Some(tid))
        .unwrap();

    let seen = Arc::new(Mutex::new(Vec::<(ThreadId, ClassId)>::new()));
    let s = seen.clone();
    mgr.set_event_callbacks(EventCallbacks {
        class_load: Some(Box::new(move |t, c| s.lock().unwrap().push((t, c)))),
        ..Default::default()
    })
    .unwrap();

    // Free function drives through the global manager.
    fire_class_load(tid, cid);
    assert_eq!(*seen.lock().unwrap(), vec![(tid, cid)]);
    assert_eq!(mgr.event_count(JvmtiEventKind::ClassLoad), 1);
}

#[test]
fn test_global_wired_gc_pair_fires_in_order() {
    let _lock = global_test_lock();
    let mgr = ensure_global_manager();
    reset_global_manager_for_tests();

    mgr.set_event_notification_mode(
        EventMode::Enable,
        JvmtiEventKind::GarbageCollectionStart,
        None,
    )
    .unwrap();
    mgr.set_event_notification_mode(
        EventMode::Enable,
        JvmtiEventKind::GarbageCollectionFinish,
        None,
    )
    .unwrap();

    let log = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let lg1 = log.clone();
    let lg2 = log.clone();
    mgr.set_event_callbacks(EventCallbacks {
        gc_start: Some(Box::new(move || lg1.lock().unwrap().push("start"))),
        gc_finish: Some(Box::new(move || lg2.lock().unwrap().push("finish"))),
        ..Default::default()
    })
    .unwrap();

    fire_gc_start();
    fire_gc_finish();

    assert_eq!(*log.lock().unwrap(), vec!["start", "finish"]);
}

#[test]
fn test_global_wired_vm_init_vm_death_one_shot() {
    let _lock = global_test_lock();
    let mgr = ensure_global_manager();
    reset_global_manager_for_tests();

    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::VmInit, None)
        .unwrap();
    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::VmDeath, None)
        .unwrap();
    let init = Arc::new(AtomicU32::new(0));
    let death = Arc::new(AtomicU32::new(0));
    let ic = init.clone();
    let dc = death.clone();
    mgr.set_event_callbacks(EventCallbacks {
        vm_init: Some(Box::new(move || {
            ic.fetch_add(1, Ordering::SeqCst);
        })),
        vm_death: Some(Box::new(move || {
            dc.fetch_add(1, Ordering::SeqCst);            })),
        ..Default::default()
    })
    .unwrap();

    fire_vm_init();
    fire_vm_death();
    assert_eq!(init.load(Ordering::SeqCst), 1);
    assert_eq!(death.load(Ordering::SeqCst), 1);
}

#[test]
fn test_global_wired_exception_catch_fires() {
    let _lock = global_test_lock();
    let mgr = ensure_global_manager();
    reset_global_manager_for_tests();

    let tid: ThreadId = 3;
    mgr.set_event_notification_mode(
        EventMode::Enable,
        JvmtiEventKind::ExceptionCatch,
        Some(tid),
    )
    .unwrap();
    let seen = Arc::new(Mutex::new(Vec::<(ThreadId, MethodId, i64)>::new()));
    let s = seen.clone();
    mgr.set_event_callbacks(EventCallbacks {
        exception_catch: Some(Box::new(move |t, m, l, _exc| {
            s.lock().unwrap().push((t, m, l))            })),
        ..Default::default()
    })
    .unwrap();
    fire_exception_catch(tid, 99, 10, 0);
    fire_exception_catch(tid, 99, 11, 0);
    assert_eq!(*seen.lock().unwrap(), vec![(tid, 99, 10), (tid, 99, 11)]);
}

// ----------------------------------------------------------------
// T17.Δ — interpreter event-firing tests
// ----------------------------------------------------------------

/// T17.Δ.1 — registering and firing `MethodEntry` through the free
/// function must deliver exactly one callback with the given method id.
#[test]
fn t17_d_method_entry_fires_on_invocation() {
    let _lock = global_test_lock();
    let mgr = ensure_global_manager();
    reset_global_manager_for_tests();

    let tid: ThreadId = 7;
    let mid: MethodId = 0x12345;
    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, Some(tid))
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::<(ThreadId, MethodId)>::new()));
    let s = seen.clone();
    mgr.set_event_callbacks(EventCallbacks {
        method_entry: Some(Box::new(move |t, m| s.lock().unwrap().push((t, m)))),
        ..Default::default()
    })
    .unwrap();

    assert!(any_method_entry_listener_active());
    fire_method_entry(tid, mid);
    assert_eq!(*seen.lock().unwrap(), vec![(tid, mid)]);
    assert_eq!(mgr.event_count(JvmtiEventKind::MethodEntry), 1);
}

/// T17.Δ.2 — MethodExit on a normal return fires with
/// `was_popped_by_exception=false` and the correct return value.
#[test]
fn t17_d_method_exit_fires_on_normal_return() {
    let _lock = global_test_lock();
    let mgr = ensure_global_manager();
    reset_global_manager_for_tests();

    let tid: ThreadId = 8;
    let mid: MethodId = 0x22222;
    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodExit, Some(tid))
        .unwrap();
    let seen = Arc::new(Mutex::new(
        Vec::<(ThreadId, MethodId, bool, LocalValue)>::new(),
    ));
    let s = seen.clone();
    mgr.set_event_callbacks(EventCallbacks {
        method_exit: Some(Box::new(move |t, m, exc, rv| {
            s.lock().unwrap().push((t, m, exc, rv))            })),
        ..Default::default()
    })
    .unwrap();

    fire_method_exit(tid, mid, false, LocalValue::Int(42));
    assert_eq!(
        *seen.lock().unwrap(),
        vec![(tid, mid, false, LocalValue::Int(42))]
    );
    assert_eq!(mgr.event_count(JvmtiEventKind::MethodExit), 1);
}

/// T17.Δ.2 — MethodExit on exception unwind fires with
/// `was_popped_by_exception=true`.
#[test]
fn t17_d_method_exit_fires_on_exception_unwind() {
    let _lock = global_test_lock();
    let mgr = ensure_global_manager();
    reset_global_manager_for_tests();

    let tid: ThreadId = 9;
    let mid: MethodId = 0x33333;
    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodExit, Some(tid))
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::<(ThreadId, MethodId, bool)>::new()));
    let s = seen.clone();
    mgr.set_event_callbacks(EventCallbacks {
        method_exit: Some(Box::new(move |t, m, exc, _rv| {
            s.lock().unwrap().push((t, m, exc))            })),
        ..Default::default()
    })
    .unwrap();

    fire_method_exit(tid, mid, true, LocalValue::Object(None));
    let got = seen.lock().unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, tid);
    assert_eq!(got[0].1, mid);
    assert!(got[0].2, "abrupt-completion indicator must be true");
}

/// T17.Δ.3 — SingleStep fires once per bytecode dispatched when the
/// corresponding event is enabled.
#[test]
fn t17_d_single_step_fires_per_bytecode() {
    let _lock = global_test_lock();
    let mgr = ensure_global_manager();
    reset_global_manager_for_tests();

    let tid: ThreadId = 10;
    let mid: MethodId = 0x44444;
    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::SingleStep, Some(tid))
        .unwrap();
    let count = Arc::new(AtomicU32::new(0));
    let c = count.clone();
    mgr.set_event_callbacks(EventCallbacks {
        single_step: Some(Box::new(move |_t, _m, _l| {
            c.fetch_add(1, Ordering::SeqCst);            })),
        ..Default::default()
    })
    .unwrap();

    for pc in 0..10 {
        fire_single_step(tid, mid, pc);
    }
    assert_eq!(count.load(Ordering::SeqCst), 10);
    assert!(any_single_step_listener_active());
}

/// T17.Δ.4 — registering a FieldAccess/FieldModification watchpoint on
/// (class, field) must cause the corresponding event to fire when the
/// `fire_field_*_if_watched` helper is invoked.
#[test]
fn t17_d_field_access_watch() {
    let _lock = global_test_lock();
    let mgr = ensure_global_manager();
    reset_global_manager_for_tests();

    let tid: ThreadId = 11;
    let mid: MethodId = 0x55555;
    let class_id: u64 = 99;
    let field_index: usize = 3;

    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::FieldAccess, Some(tid))
        .unwrap();
    mgr.set_event_notification_mode(
        EventMode::Enable,
        JvmtiEventKind::FieldModification,
        Some(tid),
    )
    .unwrap();

    set_field_watchpoint(class_id, field_index, true, true).unwrap();
    assert!(any_field_watchpoint_active());
    assert_eq!(
        field_watchpoint_for(class_id, field_index),
        Some(FieldWatchpoint {
            class_id,
            field_index,
            access_watched: true,
            modification_watched: true,
        })
    );

    let accesses = Arc::new(AtomicU32::new(0));
    let mods = Arc::new(AtomicU32::new(0));
    let a = accesses.clone();
    let m = mods.clone();
    mgr.set_event_callbacks(EventCallbacks {
        field_access: Some(Box::new(move |_t, _m, _f| {
            a.fetch_add(1, Ordering::SeqCst);
        })),
        field_modification: Some(Box::new(move |_t, _m, _f| {
            m.fetch_add(1, Ordering::SeqCst);            })),
        ..Default::default()
    })
    .unwrap();

    fire_field_access_if_watched(tid, mid, class_id, field_index);
    fire_field_modification_if_watched(tid, mid, class_id, field_index);
    // Unwatched tuple — must not fire.
    fire_field_access_if_watched(tid, mid, class_id, field_index + 1);
    fire_field_modification_if_watched(tid, mid, class_id + 1, field_index);

    assert_eq!(accesses.load(Ordering::SeqCst), 1);
    assert_eq!(mods.load(Ordering::SeqCst), 1);

    clear_field_watchpoint(class_id, field_index, true, true).unwrap();
    assert_eq!(field_watchpoint_for(class_id, field_index), None);
    assert!(!any_field_watchpoint_active());
}

/// T17.Δ.4 — watchpoint validation helper rejects out-of-range indices.
#[test]
fn t17_d_field_watchpoint_bounds_check() {
    assert!(field_watchpoint_is_valid(0, 1));
    assert!(field_watchpoint_is_valid(4, 5));
    assert!(!field_watchpoint_is_valid(5, 5));
    assert!(!field_watchpoint_is_valid(100, 0));
}

/// T17.Δ.4 — encode/decode round-trip for FieldId.
#[test]
fn t17_d_field_id_encode_decode_roundtrip() {
    let fid = encode_field_id(0xCAFE_BABE, 17);
    assert_eq!(decode_field_id(fid), (0xCAFE_BABE, 17));
}

/// T17.Δ.5 — FramePop fires exactly once for the target depth.
#[test]
fn t17_d_frame_pop_fires_at_target_depth() {
    let _lock = global_test_lock();
    let mgr = ensure_global_manager();
    reset_global_manager_for_tests();

    let tid: ThreadId = 12;
    let mid: MethodId = 0x66666;
    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::FramePop, Some(tid))
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::<(ThreadId, MethodId, bool)>::new()));
    let s = seen.clone();
    mgr.set_event_callbacks(EventCallbacks {
        frame_pop: Some(Box::new(move |t, m, exc| {
            s.lock().unwrap().push((t, m, exc))            })),
        ..Default::default()
    })
    .unwrap();

    // Fire once for a normal return and once for an exception unwind.
    fire_frame_pop(tid, mid, false);
    fire_frame_pop(tid, mid, true);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![(tid, mid, false), (tid, mid, true)]
    );
    assert_eq!(mgr.event_count(JvmtiEventKind::FramePop), 2);
}

/// T17.Δ.∗ — the no-agent hot path must be a single Acquire load + one
/// predicted branch for each event kind. We can't directly measure
/// that in a test, but we can assert the per-event flags are false on
/// a fresh manager — proving the fast path short-circuits correctly.
#[test]
fn t17_d_no_agent_zero_cost() {
    let _lock = global_test_lock();
    let mgr = ensure_global_manager();
    reset_global_manager_for_tests();

    // All flags must start false.
    assert!(!mgr.has_method_entry_listener());
    assert!(!mgr.has_method_exit_listener());
    assert!(!mgr.has_single_step_listener());
    assert!(!mgr.has_field_access_listener());
    assert!(!mgr.has_field_modification_listener());
    assert!(!mgr.has_frame_pop_listener());
    assert!(!any_method_entry_listener_active());
    assert!(!any_method_exit_listener_active());
    assert!(!any_single_step_listener_active());
    assert!(!any_field_access_listener_active());
    assert!(!any_field_modification_listener_active());
    assert!(!any_frame_pop_listener_active());

    // Firing with no agents attached must be harmless and record 0.
    for i in 0..10_000u32 {
        fire_method_entry(1, i as u64);
        fire_method_exit(1, i as u64, false, LocalValue::Int(0));
        fire_single_step(1, i as u64, i as i64);
        fire_frame_pop(1, i as u64, false);
    }
    assert_eq!(mgr.event_count(JvmtiEventKind::MethodEntry), 0);
    assert_eq!(mgr.event_count(JvmtiEventKind::MethodExit), 0);
    assert_eq!(mgr.event_count(JvmtiEventKind::SingleStep), 0);
    assert_eq!(mgr.event_count(JvmtiEventKind::FramePop), 0);
}

/// T17.Δ.∗ — per-event flag flips to true on Enable, back to false on
/// Disable.
#[test]
fn t17_d_per_event_flag_lifecycle() {
    let _lock = global_test_lock();
    let mgr = ensure_global_manager();
    reset_global_manager_for_tests();

    let tid: ThreadId = 13;
    // MethodEntry
    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, Some(tid))
        .unwrap();
    assert!(mgr.has_method_entry_listener());
    mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodEntry, Some(tid))
        .unwrap();
    assert!(!mgr.has_method_entry_listener());

    // MethodExit
    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodExit, None)
        .unwrap();
    assert!(mgr.has_method_exit_listener());
    mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodExit, None)
        .unwrap();
    assert!(!mgr.has_method_exit_listener());

    // SingleStep (per-thread)
    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::SingleStep, Some(tid))
        .unwrap();
    assert!(mgr.has_single_step_listener());
    mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::SingleStep, Some(tid))
        .unwrap();
    assert!(!mgr.has_single_step_listener());
}

/// T17.Δ.∗ — agent callbacks that panic must not bring down the VM.
/// The panic is caught inside each fire_* path; subsequent fires
/// keep delivering events.
#[test]
fn t17_d_agent_panic_does_not_crash_vm() {
    let _lock = global_test_lock();
    let mgr = ensure_global_manager();
    reset_global_manager_for_tests();

    let tid: ThreadId = 14;
    let mid: MethodId = 0x77777;
    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, Some(tid))
        .unwrap();

    let ok_count = Arc::new(AtomicU32::new(0));
    let c = ok_count.clone();
    mgr.set_event_callbacks(EventCallbacks {
        method_entry: Some(Box::new(move |_t, m| {
            c.fetch_add(1, Ordering::SeqCst);
            if m == 999 {
                panic!("agent panic");                }            })),
        ..Default::default()
    })
    .unwrap();

    fire_method_entry(tid, mid); // normal
    fire_method_entry(tid, 999); // agent panics
    fire_method_entry(tid, mid + 1); // must still deliver
    assert_eq!(ok_count.load(Ordering::SeqCst), 3);
}

/// A panicking ClassLoad / ClassPrepare callback must not propagate out
/// of the fire_* path.
///
/// This matters more than for the other events: both fire from
/// `ClassManager::define_class_shared_with_options` while the caller
/// holds the L10 `class_manager` **write** guard. `parking_lot::RwLock`
/// does not poison, so an escaping unwind would release that guard
/// silently and publish a half-built `ClassManager`. See the re-entrancy
/// notes above `install_class_load_hook` in
/// `classloading/src/class_manager.rs`.
#[test]
fn class_load_prepare_agent_panic_is_contained() {
    let _lock = global_test_lock();
    let mgr = ensure_global_manager();
    reset_global_manager_for_tests();

    let tid: ThreadId = 21;
    let poison: ClassId = 999;
    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::ClassLoad, Some(tid))
        .unwrap();
    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::ClassPrepare, Some(tid))
        .unwrap();

    let calls = Arc::new(AtomicU32::new(0));
    let (cl, cp) = (calls.clone(), calls.clone());
    mgr.set_event_callbacks(EventCallbacks {
        class_load: Some(Box::new(move |_t, c| {
            cl.fetch_add(1, Ordering::SeqCst);
            if c == poison {
                panic!("agent panic in ClassLoad");
            }
        })),
        class_prepare: Some(Box::new(move |_t, c| {
            cp.fetch_add(1, Ordering::SeqCst);
            if c == poison {
                panic!("agent panic in ClassPrepare");                }            })),
        ..Default::default()
    })
    .unwrap();

    // Each of these would unwind into the caller before the fix. Reaching
    // the assertion below at all is the property under test.
    fire_class_load(tid, 1);
    fire_class_load(tid, poison);
    fire_class_load(tid, 2);
    fire_class_prepare(tid, 1);
    fire_class_prepare(tid, poison);
    fire_class_prepare(tid, 2);

    // 3 ClassLoad + 3 ClassPrepare: dispatch survives the panic and keeps
    // delivering, rather than the callback being torn down or skipped.
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    assert_eq!(mgr.event_count(JvmtiEventKind::ClassLoad), 3);
    assert_eq!(mgr.event_count(JvmtiEventKind::ClassPrepare), 3);
}

/// T17.Δ.4 — registering a watchpoint idempotently sets access and
/// modification flags additively.
#[test]
fn t17_d_field_watch_additive_flags() {
    let _lock = global_test_lock();
    let _mgr = ensure_global_manager();
    reset_global_manager_for_tests();

    set_field_watchpoint(5, 2, true, false).unwrap();
    let wp1 = field_watchpoint_for(5, 2).unwrap();
    assert!(wp1.access_watched);
    assert!(!wp1.modification_watched);

    set_field_watchpoint(5, 2, false, true).unwrap();
    let wp2 = field_watchpoint_for(5, 2).unwrap();
    assert!(wp2.access_watched);
    assert!(wp2.modification_watched);

    clear_field_watchpoint(5, 2, true, false).unwrap();
    let wp3 = field_watchpoint_for(5, 2).unwrap();
    assert!(!wp3.access_watched);
    assert!(wp3.modification_watched);

    clear_field_watchpoint(5, 2, false, true).unwrap();
    assert!(field_watchpoint_for(5, 2).is_none());
}

// -----------------------------------------------------------------------
// C2 review remediation (2026-08-01) — one JVMTI environment per VM
//
// Every test below gives itself its OWN `vm_identity`. The block above
// shares one process-wide manager and is only safe because each test takes
// `global_test_lock()`; these take the same lock (they read and reset the
// unattributed row, and they move the process-wide union mirrors) but they
// never share a row with each other.
// -----------------------------------------------------------------------

/// A `vm_identity` no other test and no real VM can collide with.
///
/// Real identities come from `NEXT_VM_IDENTITY` (`vm/src/vm/vm_init.rs`),
/// a counter starting at 1, so small integers are NOT safe to fake with in
/// a binary that also constructs real `SharedVm`s. gcg g2/k: drawn from
/// that same counter (`vm_init::test_vm_identity`), which makes it unique
/// against both by construction; it used a private base (`0x7000_0000`).
fn scoped_test_vm() -> usize {
    crate::vm::vm_init::test_vm_identity()
}

/// Install a fresh, empty manager owned by `vm` and return it.
fn manager_owned_by(vm: usize) -> Arc<JvmtiEventManager> {
    install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
    let m = manager_for_vm(vm).expect("just installed");
    assert_eq!(
        m.vm_identity(),
        vm,
        "the row must hold the VM's own manager"
    );
    m
}

/// A watchpoint set by an agent in VM A must not exist in VM B. `class_id`
/// is only unique *within* a VM, so the old flat `(class_id, field_index)`
/// map made VM A's watch fire on an unrelated field of an unrelated class
/// in VM B.
#[test]
fn watchpoints_are_per_vm() {
    let _lock = global_test_lock();
    let _mgr = ensure_global_manager();
    reset_global_manager_for_tests();

    let a = scoped_test_vm();
    let b = scoped_test_vm();

    set_field_watchpoint_for_vm(a, 99, 3, true, true).unwrap();

    assert!(field_watchpoint_for_vm(a, 99, 3).is_some());
    assert!(
        field_watchpoint_for_vm(b, 99, 3).is_none(),
        "VM B must not inherit VM A's watchpoint for the same (class_id, field_index)"
    );
    assert!(any_field_watchpoint_active_for_vm(a));
    assert!(!any_field_watchpoint_active_for_vm(b));

    // The process-wide mirror is a deliberate superset: B pays a branch,
    // but the per-VM lookup above is what decides whether anything fires.
    assert!(any_field_watchpoint_active());

    forget_vm_jvmti_state(a);
    forget_vm_jvmti_state(b);
    assert!(!any_field_watchpoint_active());
}

/// The whole point of doing this at environment scope rather than re-keying
/// the watchpoint map alone: a watchpoint hit must reach the owning VM's
/// callbacks and nobody else's.
#[test]
fn watchpoint_hits_reach_only_the_owning_vms_callbacks() {
    let _lock = global_test_lock();
    let _mgr = ensure_global_manager();
    reset_global_manager_for_tests();

    let a = scoped_test_vm();
    let b = scoped_test_vm();
    let mgr_a = manager_owned_by(a);
    let mgr_b = manager_owned_by(b);

    let tid: ThreadId = 21;
    let mid: MethodId = 0x1234;
    for m in [&mgr_a, &mgr_b] {
        m.set_event_notification_mode(
            EventMode::Enable,
            JvmtiEventKind::FieldAccess,
            Some(tid),
        )
        .unwrap();
    }

    let hits_a = Arc::new(AtomicU32::new(0));
    let hits_b = Arc::new(AtomicU32::new(0));
    let ha = hits_a.clone();
    let hb = hits_b.clone();
    mgr_a
        .set_event_callbacks(EventCallbacks {
            field_access: Some(Box::new(move |_t, _m, _f| {
                ha.fetch_add(1, Ordering::SeqCst);                })),
            ..Default::default()
        })
        .unwrap();
    mgr_b
        .set_event_callbacks(EventCallbacks {
            field_access: Some(Box::new(move |_t, _m, _f| {
                hb.fetch_add(1, Ordering::SeqCst);                })),
            ..Default::default()
        })
        .unwrap();

    // Only VM A watches the field.
    set_field_watchpoint_for_vm(a, 77, 1, true, false).unwrap();

    fire_field_access_if_watched_for_vm(a, tid, mid, 77, 1);
    // Same class id and field index, but in VM B, where nothing is watched.
    fire_field_access_if_watched_for_vm(b, tid, mid, 77, 1);

    assert_eq!(hits_a.load(Ordering::SeqCst), 1);
    assert_eq!(
        hits_b.load(Ordering::SeqCst),
        0,
        "VM B's agent must not see a hit for a watchpoint VM A installed"
    );

    forget_vm_jvmti_state(a);
    forget_vm_jvmti_state(b);
}

/// Listener flags: exact per VM, superset across the process.
#[test]
fn listener_flags_are_per_vm_and_the_union_is_only_a_guard() {
    let _lock = global_test_lock();
    let _mgr = ensure_global_manager();
    reset_global_manager_for_tests();

    let a = scoped_test_vm();
    let b = scoped_test_vm();
    let mgr_a = manager_owned_by(a);
    let _mgr_b = manager_owned_by(b);

    assert!(!any_method_entry_listener_active());

    mgr_a
        .set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, None)
        .unwrap();

    assert!(any_method_entry_listener_active_for_vm(a));
    assert!(
        !any_method_entry_listener_active_for_vm(b),
        "VM B must not report a listener because VM A attached one"
    );
    assert!(
        any_method_entry_listener_active(),
        "the union guard must be true so no VM's event is ever missed"
    );

    mgr_a
        .set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodEntry, None)
        .unwrap();
    assert!(!any_method_entry_listener_active());

    forget_vm_jvmti_state(a);
    forget_vm_jvmti_state(b);
}

/// Delivery is exact even when the union guard is true for the other VM.
#[test]
fn events_are_delivered_only_to_the_owning_vms_manager() {
    let _lock = global_test_lock();
    let _mgr = ensure_global_manager();
    reset_global_manager_for_tests();

    let a = scoped_test_vm();
    let b = scoped_test_vm();
    let mgr_a = manager_owned_by(a);
    let mgr_b = manager_owned_by(b);

    let tid: ThreadId = 22;
    for m in [&mgr_a, &mgr_b] {
        m.set_event_notification_mode(
            EventMode::Enable,
            JvmtiEventKind::MethodEntry,
            Some(tid),
        )
        .unwrap();
    }

    let seen_a = Arc::new(AtomicU32::new(0));
    let seen_b = Arc::new(AtomicU32::new(0));
    let sa = seen_a.clone();
    let sb = seen_b.clone();
    mgr_a
        .set_event_callbacks(EventCallbacks {
            method_entry: Some(Box::new(move |_t, _m| {
                sa.fetch_add(1, Ordering::SeqCst);                })),
            ..Default::default()
        })
        .unwrap();
    mgr_b
        .set_event_callbacks(EventCallbacks {
            method_entry: Some(Box::new(move |_t, _m| {
                sb.fetch_add(1, Ordering::SeqCst);                })),
            ..Default::default()
        })
        .unwrap();

    fire_method_entry_for_vm(a, tid, 0x900);
    assert_eq!(seen_a.load(Ordering::SeqCst), 1);
    assert_eq!(seen_b.load(Ordering::SeqCst), 0);
    assert_eq!(mgr_b.event_count(JvmtiEventKind::MethodEntry), 0);

    fire_method_entry_for_vm(b, tid, 0x901);
    assert_eq!(seen_a.load(Ordering::SeqCst), 1);
    assert_eq!(seen_b.load(Ordering::SeqCst), 1);

    forget_vm_jvmti_state(a);
    forget_vm_jvmti_state(b);
}

/// The migration seam: a VM that never installed a manager of its own
/// resolves through the unattributed row, so converting a call site to
/// `*_for_vm` before converting `SharedVm::new` does not silently stop
/// delivering events. Once the VM installs its own, that one wins.
#[test]
fn an_unclaimed_vm_falls_back_to_the_unattributed_row() {
    let _lock = global_test_lock();
    let unattributed = ensure_global_manager();
    reset_global_manager_for_tests();

    let vm = scoped_test_vm();
    let fallback = manager_for_vm(vm).expect("must fall back, not answer None");
    assert!(
        Arc::ptr_eq(&fallback, &unattributed),
        "an unclaimed VM must resolve to the unattributed manager"
    );
    assert_eq!(fallback.vm_identity(), UNATTRIBUTED_VM);

    let own = manager_owned_by(vm);
    assert!(!Arc::ptr_eq(&own, &unattributed));
    assert!(Arc::ptr_eq(&manager_for_vm(vm).unwrap(), &own));

    forget_vm_jvmti_state(vm);
    // Back to the fallback once the VM's row is gone.
    assert!(Arc::ptr_eq(&manager_for_vm(vm).unwrap(), &unattributed));
}

/// Teardown. Without this, a disposed VM's manager keeps its agent's
/// callback closures alive forever, its listener flags keep every other
/// VM's interpreter on the slow path, and its watchpoints keep matching a
/// `class_id` that now means something else.
#[test]
fn forget_vm_jvmti_state_drops_everything_and_is_idempotent() {
    let _lock = global_test_lock();
    let _mgr = ensure_global_manager();
    reset_global_manager_for_tests();

    let vm = scoped_test_vm();
    let mgr = manager_owned_by(vm);
    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodExit, None)
        .unwrap();
    set_field_watchpoint_for_vm(vm, 5, 0, true, true).unwrap();
    assert!(any_method_exit_listener_active());
    assert!(any_field_watchpoint_active());

    // Row counts are not asserted: other test modules in this binary build
    // real `Vm`s in parallel, and each of those registers a row. Membership
    // of *this* VM's row is the property that matters and is stable.
    assert!(manager_for_vm_exact(vm).is_some());

    forget_vm_jvmti_state(vm);
    assert!(manager_for_vm_exact(vm).is_none(), "the row must be gone");
    assert!(!any_method_exit_listener_active_for_vm(vm));
    assert!(!any_field_watchpoint_active_for_vm(vm));
    assert!(
        !any_method_exit_listener_active(),
        "the union mirror must be recomputed on teardown, not left latched"
    );
    assert!(!any_field_watchpoint_active());

    // Second call must be a no-op — `release_vm_native_state` is invoked
    // from two places, either of which may run first or alone.
    forget_vm_jvmti_state(vm);
    assert!(manager_for_vm_exact(vm).is_none());
}

/// `None` bridge (never installed) and `Some(dead Weak)` (VM gone) must not
/// be conflated: pruning on `strong_count() == 0` alone would delete a live
/// row that simply has no bridge yet.
#[test]
fn a_dead_bridge_is_pruned_but_a_bridgeless_row_is_kept() {
    let _lock = global_test_lock();
    let _mgr = ensure_global_manager();
    reset_global_manager_for_tests();

    let dead = scoped_test_vm();
    let bridgeless = scoped_test_vm();
    let trigger = scoped_test_vm();

    let _ = manager_owned_by(bridgeless);
    {
        let mut guard = environments_write();
        let map = guard.get_or_insert_with(HashMap::new);
        // A `Weak::new()` never upgrades — exactly what a torn-down VM's
        // bridge looks like.
        map.entry(dead)
            .or_insert_with(VmJvmtiEnvironment::empty)
            .bridge = Some(Weak::new());
    }
    assert!(bridge_for_vm(dead).is_none());
    assert_ne!(
        sole_live_bridge().map(|s| s.vm_identity),
        Some(dead),
        "a dead bridge must never be resolved as the sole live VM"
    );

    // Any registry write prunes provably-dead rows.
    forget_vm_jvmti_state(trigger);
    assert!(
        manager_for_vm_exact(bridgeless).is_some(),
        "a live row with no bridge installed must survive the prune"
    );
    {
        let guard = environments_read();
        assert!(
            !guard.as_ref().unwrap().contains_key(&dead),
            "a row whose bridge expired must be pruned"
        );
    }

    forget_vm_jvmti_state(bridgeless);
}

/// The headline bug. `REAL_AGENT_ENV_BRIDGE` was one
/// `OnceLock<Weak<SharedVm>>`, first-writer-wins:
///
/// * two live VMs — VM B's `set` failed, so VM B's bridged events went into
///   VM A's `shared.debug.jvmti_env`, carrying VM B's `ClassId`s;
/// * **sequentially** — after VM A was dropped the stored `Weak` stopped
///   upgrading and VM B's `set` *still* failed, so VM B's native agent
///   received nothing at all, for the life of the process. That breaks
///   sequential embedding, not just concurrency.
///
/// Both halves are asserted here.
#[test]
fn bridge_is_per_vm_and_a_second_vm_is_not_silently_dropped() {
    use crate::config::VmConfig;
    use crate::vm::SharedVm;

    let _lock = global_test_lock();

    let a = Arc::new(SharedVm::new(VmConfig::default()));
    let b = Arc::new(SharedVm::new(VmConfig::default()));
    let (a_id, b_id) = (a.vm_identity, b.vm_identity);
    assert_ne!(a_id, b_id);

    install_real_agent_env_bridge(&a);
    install_real_agent_env_bridge(&b);

    assert!(
        Arc::ptr_eq(&bridge_for_vm(a_id).expect("VM A must have its bridge"), &a),
        "VM A's bridge must resolve to VM A"
    );
    assert!(
        Arc::ptr_eq(
            &bridge_for_vm(b_id).expect("VM B's bridge must not be dropped"),
            &b
        ),
        "the second VM must get its own bridge, not silently lose it"
    );
    assert!(
        sole_live_bridge().is_none(),
        "with two live VMs an unattributed event must be dropped, not guessed"
    );

    // Sequential embedding: VM A goes away, VM B keeps working.
    forget_vm_jvmti_state(a_id);
    drop(a);
    assert!(bridge_for_vm(a_id).is_none());
    assert!(
        Arc::ptr_eq(
            &bridge_for_vm(b_id).expect("VM B must outlive VM A's teardown"),
            &b
        ),
        "dropping the first VM must not take the second VM's bridge with it"
    );
    assert_ne!(
        sole_live_bridge().map(|s| s.vm_identity),
        Some(a_id),
        "a released VM must never be resolved as the sole live bridge"
    );
    // `sole_live_bridge() == Some(b_id)` is the property we actually want
    // here, but it cannot be asserted in this binary: other test modules
    // build real `Vm`s in parallel and each registers a live bridge, so
    // the "exactly one" precondition is not ours to control. The
    // `bridge_for_vm(b_id)` assertion above is the load-bearing one — under
    // the old single `OnceLock<Weak<SharedVm>>` it returned `None`, because
    // VM B was never registered at all.

    forget_vm_jvmti_state(b_id);
    drop(b);
}

/// Interpreter round i1 wave 9: a VM entering interpreter-only mode has
/// its JIT inline caches flushed, once per false→true edge, and nothing
/// retired. A second interpreter-only event while the mode already holds
/// flushes nothing; leaving and re-entering the mode flushes again.
#[test]
fn entering_interp_only_mode_flushes_the_vms_inline_caches_once() {
    use crate::config::VmConfig;
    use crate::vm::SharedVm;

    let _lock = global_test_lock();
    let vm = Arc::new(SharedVm::new(VmConfig::default()));
    install_real_agent_env_bridge(&vm);
    let id = vm.vm_identity;
    let mgr = manager_for_vm(id).expect("SharedVm::new installs the VM's own manager");

    // An unregistered target, like a native: publishable without an owner.
    const TARGET: u64 = 0x0CAFE_4000;
    let class: Arc<str> = Arc::from("jvmti/IcFlushProbe");
    let method: Arc<str> = Arc::from("run");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(4714);
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("alloc executable");
    buf.emit(&[0xC3]);
    let mut body = cratonvm_jit::CompiledMethod::new(buf);
    body._jit_mic_slots
        .push(Box::new(cratonvm_jit::JitMICSlot::new()));
    vm.jit
        .jit_cache
        .put(class.clone(), method.clone(), desc.clone(), cid, body);
    let published = vm
        .jit
        .jit_cache
        .get(&class, &method, &desc, cid)
        .expect("body published");
    let mic = &published._jit_mic_slots[0];
    mic.update(41, "jvmti/IcFlushReceiver", TARGET, false, false);
    assert_eq!(mic.cached_entry().0, TARGET);

    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, None)
        .unwrap();
    assert!(interp_only_events_active_for_vm(id));
    assert_eq!(
        mic.cached_entry(),
        (0, false),
        "the edge must flush the MIC"
    );
    // Interpreter round i1 wave 46 (lane L1): an `experimental-debug`
    // build also withdraws every compiled body of the VM on this edge
    // (wave 37, `publish_union_listener_flags` ->
    // `interpreter::note_every_method_needs_the_interpreter`,
    // `WITHDRAWAL_BY_JVMTI`; a fresh VM has no earlier withdrawal to
    // hold, so the whole cache goes). The body is evicted there, by
    // design; the inline-cache flush alone, which retires nothing, is the
    // build without the debugger gates. The assertion below fails in an
    // `experimental-debug` build since wave 37.
    if cfg!(feature = "experimental-debug") {
        assert!(
            vm.jit.jit_cache.get(&class, &method, &desc, cid).is_none(),
            "the interpreter-only edge withdraws every body (wave 37)"
        );
        mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodEntry, None)
            .unwrap();
        drop(published);
        forget_vm_jvmti_state(id);
        drop(vm);
        return;
    }
    assert!(
        vm.jit.jit_cache.get(&class, &method, &desc, cid).is_some(),
        "a flush retires nothing"
    );

    // Already interpreter-only: a second event is not an edge.
    mic.update(41, "jvmti/IcFlushReceiver", TARGET, false, false);
    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodExit, None)
        .unwrap();
    assert_eq!(mic.cached_entry().0, TARGET, "no edge, no flush");

    // Leave the mode, refill, and re-enter it: a new edge.
    mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodEntry, None)
        .unwrap();
    mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodExit, None)
        .unwrap();
    assert!(!interp_only_events_active_for_vm(id));
    assert_eq!(mic.cached_entry().0, TARGET);
    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::SingleStep, None)
        .unwrap();
    assert_eq!(
        mic.cached_entry(),
        (0, false),
        "re-entering the mode flushes again"
    );

    mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::SingleStep, None)
        .unwrap();
    drop(published);
    forget_vm_jvmti_state(id);
    drop(vm);
}

/// Wave 15: a VM's own manager gets `VMInit` once, when the VM enters
/// the live phase (init level 3, after `System.initPhase2`). It was sent
/// at the end of `SharedVm::new`, where nothing could have enabled it yet
/// (the manager had just been created there).
#[test]
fn a_vm_manager_gets_vm_init_once_at_the_live_phase() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let vm = shared.vm_identity;
    let manager = manager_for_vm(vm).expect("SharedVm::new installs its manager");
    let inits = Arc::new(AtomicU32::new(0));
    let seen = inits.clone();
    manager
        .set_event_callbacks(EventCallbacks {
            vm_init: Some(Box::new(move || {
                seen.fetch_add(1, Ordering::SeqCst);                })),
            ..Default::default()
        })
        .unwrap();
    manager
        .set_event_notification_mode(EventMode::Enable, JvmtiEventKind::VmInit, None)
        .unwrap();
    // The per-VM half of `set_init_level`: the process-wide level is
    // shared by every test of the binary and must not be raised here.
    shared.raise_vm_init_level(1);
    shared.raise_vm_init_level(2);
    assert_eq!(inits.load(Ordering::SeqCst), 0, "not live yet");
    shared.raise_vm_init_level(3);
    shared.raise_vm_init_level(4);
    shared.raise_vm_init_level(3);
    assert_eq!(inits.load(Ordering::SeqCst), 1, "once, at level 3");
    forget_vm_jvmti_state(vm);
}

/// Interpreter round i1 wave 18, lane L1 (page
/// `interpreter-L2-jvmti-event-object-address-stale-for-the-next-listener-FIXED-20260925`): of
/// two listeners, the first collects; the second gets the event object's
/// address as the collection left it, for `Exception` and for a
/// `MethodExit` reference result. Every listener used to get the address
/// read before the first one ran.
#[test]
fn a_later_listener_gets_the_event_object_where_an_earlier_collection_put_it() {
    use crate::config::VmConfig;
    use crate::threading::jvm_thread::{JvmThread, ThreadId as VmThreadId};
    use crate::vm::SharedVm;

    fn collect(vm: &Weak<SharedVm>) {
        if let Some(vm) = vm.upgrade() {
            let mut t = JvmThread::new(VmThreadId(0), "w18-event-object");
            crate::runtime::interpreter::maybe_gc_forced_pub_at(&vm, &mut t, "w18-jvmti");
        }
    }

    let _lock = global_test_lock();
    let vm = Arc::new(SharedVm::new(VmConfig::default()));
    install_real_agent_env_bridge(&vm);
    let id = vm.vm_identity;
    let mgr = manager_for_vm(id).expect("SharedVm::new installs the VM's own manager");
    let class = vm
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class("cratonvm/test/W18EventObject", 0)
        .expect("Compatible mode fabricates");
    let object = vm.mem.heap.alloc_object(class, 0);
    // The test's own root, to learn where a collection put the object.
    let keep = vm.natives.jni_global_refs.lock().add(object);
    let current = |vm: &SharedVm| {
        vm.natives
            .jni_global_refs
            .lock()
            .resolve(keep)
            // Cast: the heap address, as the events carry it
            .map_or(0, |o| o.as_ptr() as usize as u64)
    };

    // The first listener, the manager's own table: records, then collects.
    let first = Arc::new(Mutex::new(Vec::<u64>::new()));
    let (first_exc, first_exit) = (Arc::clone(&first), Arc::clone(&first));
    let (weak_exc, weak_exit) = (Arc::downgrade(&vm), Arc::downgrade(&vm));
    mgr.set_event_callbacks(EventCallbacks {
        exception: Some(Box::new(move |_, _, _, exception, _, _| {
            first_exc.lock().expect("first").push(exception);
            collect(&weak_exc);
        })),
        method_exit: Some(Box::new(move |_, _, _, value| {
            if let LocalValue::Object(Some(address)) = value {
                first_exit.lock().expect("first").push(address);                }
            collect(&weak_exit);            })),
        ..Default::default()
    })
    .unwrap();
    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::Exception, None)
        .unwrap();
    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodExit, None)
        .unwrap();

    // The second listener, an attached env: records.
    let later = Arc::new(Mutex::new(Vec::<u64>::new()));
    let (later_exc, later_exit) = (Arc::clone(&later), Arc::clone(&later));
    let env = Arc::new(JvmtiEnv::new());
    env.event_manager
        .set_event_callbacks(EventCallbacks {
            exception: Some(Box::new(move |_, _, _, exception, _, _| {
                later_exc.lock().expect("later").push(exception);
            })),
            method_exit: Some(Box::new(move |_, _, _, value| {
                if let LocalValue::Object(Some(address)) = value {
                    later_exit.lock().expect("later").push(address);                    }                })),
            ..Default::default()
        })
        .unwrap();
    mgr.register_env(&env).unwrap();
    env.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::Exception, None)
        .unwrap();
    env.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodExit, None)
        .unwrap();

    let before = current(&vm);
    fire_exception_for_vm(id, 0, 1, 0, before, 0, -1);
    let after = current(&vm);
    assert_eq!(*first.lock().expect("first"), vec![before]);
    assert_eq!(
        *later.lock().expect("later"),
        vec![after],
        "the second listener gets the object where the collection put it"
    );
    fire_method_exit_for_vm(id, 0, 1, false, LocalValue::Object(Some(after)));
    let moved = current(&vm);
    assert_eq!(*first.lock().expect("first"), vec![before, after]);
    assert_eq!(*later.lock().expect("later"), vec![after, moved]);
    // SAFETY: the live object's current address, read from `keep`.
    let obj = unsafe { crate::types::ObjectRef::from_raw(moved as usize as *mut u8) }; // Cast: the address read back
    assert_eq!(vm.mem.heap.class_id_of(obj), class);

    mgr.unregister_env(&env).unwrap();
    let _ = vm.natives.jni_global_refs.lock().remove(keep);
    forget_vm_jvmti_state(id);
    drop(vm);
}

/// Interpreter round i1 wave 18, lane L1 (page
/// `interpreter-L1-jvmti-exception-events-lost-for-throws-caught-in-compiled-code-FIXED-20260925`):
/// an `Exception` listener holds its VM off compiled code like the six
/// events only the interpreter posts — enabled globally on the manager or
/// for one thread on an attached env — and only its own VM; the mode ends
/// with the listener. The unwinder's own pre-filter is unchanged.
#[test]
fn an_exception_listener_holds_its_vm_off_compiled_code() {
    let _lock = global_test_lock();
    // VM identities no real `SharedVm` and no other test is ever handed
    // (gcg g2/k: from the real counter; they were `0x7218_0000` and `+ 1`).
    let vm = scoped_test_vm();
    let other_vm = scoped_test_vm();
    install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
    install_manager_for_vm(other_vm, Arc::new(JvmtiEventManager::new_for_vm(other_vm)));
    let mgr = manager_for_vm(vm).expect("row was just installed");
    assert!(!interp_only_events_active_for_vm(vm));

    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::Exception, None)
        .unwrap();
    assert!(any_exception_listener_active());
    assert_eq!(
        interp_only_events_active_for_vm(vm),
        mgr.exception_events_need_the_interpreter() || mgr.local_throw_edges,
        "compiled code cannot post Exception"
    );
    assert!(
        !interp_only_events_active_for_vm(other_vm),
        "another VM's agent must not force this VM interpreted"
    );
    mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::Exception, None)
        .unwrap();
    assert!(
        !interp_only_events_active_for_vm(vm),
        "the mode ends with it"
    );

    // One thread's enable on an attached env counts as well.
    let env = Arc::new(JvmtiEnv::new());
    mgr.register_env(&env).unwrap();
    env.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::Exception, Some(3))
        .unwrap();
    assert_eq!(
        interp_only_events_active_for_vm(vm),
        mgr.exception_events_need_the_interpreter() || mgr.local_throw_edges
    );
    mgr.unregister_env(&env).unwrap();
    assert!(!interp_only_events_active_for_vm(vm), "detached with it");

    forget_vm_jvmti_state(vm);
    forget_vm_jvmti_state(other_vm);
}

/// Round nio21 wave 2 (lane L2): while the optimizing tier builds compiled
/// local throw edges, an `ExceptionCatch` listener holds its VM off
/// compiled code and asks for every frame interpreted (the withdrawal),
/// and so does an `Exception` one; with the edges off an `ExceptionCatch`
/// listener changes neither answer (the compiled catch doors post it).
#[test]
fn nio21w2_l2_exception_catch_holds_the_vm_off_compiled_code_with_throw_edges() {
    let _lock = global_test_lock();
    for edges in [true, false] {
        let vm = scoped_test_vm();
        install_manager_for_vm(
            vm,
            Arc::new(JvmtiEventManager::new_for_vm(vm).with_local_throw_edges(edges)),
        );
        let mgr = manager_for_vm(vm).expect("row was just installed");
        assert!(!interp_only_events_active_for_vm(vm));
        assert!(!needs_every_frame_interpreted(&mgr));

        mgr.set_event_notification_mode(
            EventMode::Enable,
            JvmtiEventKind::ExceptionCatch,
            None,
        )
        .unwrap();
        assert_eq!(interp_only_events_active_for_vm(vm), edges, "edges={edges}");
        assert_eq!(needs_every_frame_interpreted(&mgr), edges, "edges={edges}");
        assert_eq!(mgr.local_throw_edges_need_the_interpreter(), edges);
        mgr.set_event_notification_mode(
            EventMode::Disable,
            JvmtiEventKind::ExceptionCatch,
            None,
        )
        .unwrap();
        assert!(
            !interp_only_events_active_for_vm(vm),
            "the mode ends with it"
        );
        assert!(!needs_every_frame_interpreted(&mgr));

        mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::Exception, None)
            .unwrap();
        assert_eq!(needs_every_frame_interpreted(&mgr), edges, "edges={edges}");
        mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::Exception, None)
            .unwrap();
        assert!(!needs_every_frame_interpreted(&mgr));

        forget_vm_jvmti_state(vm);
    }
}

/// Interpreter round i4 wave 92, lane L1
/// (`CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED`): a VM that serves JVMTI
/// `Exception` from compiled code is not held off it by an `Exception`
/// listener, builds no compiled local throw edge, and says so to its
/// compile door; one that does not is held off, whatever its edges. The
/// switch's default is not pinned here: both answers are set explicitly.
#[test]
fn i92_l1_exceptions_from_compiled_keep_the_vm_compiled() {
    let _lock = global_test_lock();
    for from_compiled in [true, false] {
        let vm = scoped_test_vm();
        install_manager_for_vm(
            vm,
            Arc::new(
                JvmtiEventManager::new_for_vm(vm)
                    .with_local_throw_edges(false)
                    .with_exceptions_from_compiled(from_compiled),
            ),
        );
        let mgr = manager_for_vm(vm).expect("row was just installed");
        assert_eq!(exceptions_post_from_compiled_code(vm), from_compiled);
        assert_eq!(mgr.exception_events_need_the_interpreter(), !from_compiled);
        assert!(!mgr.local_throw_edges_need_the_interpreter());

        mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::Exception, None)
            .unwrap();
        assert_eq!(
            interp_only_events_active_for_vm(vm),
            !from_compiled && EXCEPTION_EVENTS_NEED_THE_INTERPRETER,
            "from_compiled={from_compiled}"
        );
        assert!(!needs_every_frame_interpreted(&mgr));
        mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::Exception, None)
            .unwrap();
        assert!(!interp_only_events_active_for_vm(vm));
        forget_vm_jvmti_state(vm);
    }
    // The switch turns the VM's throw edges off with it.
    let vm = scoped_test_vm();
    let mgr = JvmtiEventManager::new_for_vm(vm)
        .with_local_throw_edges(true)
        .with_exceptions_from_compiled(true);
    assert!(!mgr.local_throw_edges);
}

/// Interpreter round i1 wave 19, lane L1: a VM's compiles keep its dead
/// locals observable exactly when a JDWP agent is configured (and built
/// in); no JVMTI env can take `can_access_local_variables`.
#[test]
fn a_debugger_observes_locals_only_under_a_jdwp_agent() {
    let plain = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    assert!(!debugger_observes_locals(&plain));
    let jdwp =
        crate::vm::SharedVm::new(crate::config::VmConfig::default().with_jdwp(5005, false));
    assert_eq!(
        debugger_observes_locals(&jdwp),
        crate::config::JDWP_SERVER_COMPILED_IN
    );
    assert!(!JvmtiCapabilities::potentially_available().can_access_local_variables);
}

/// Interpreter round i7 wave 136, lane L1 (`CRATONVM_JDWP_AGENT_DEBUGGER_FACTS`):
/// `-agentlib:jdwp` (`VmConfig::jdwp_agent`) is a JDWP debugger for the
/// locals too; `=0` asks `jdwp_port` alone.
#[test]
fn i136_l1_an_agentlib_jdwp_observes_locals() {
    let agent = crate::config::JdwpAgentConfig {
        server: true,
        suspend: false,
        host: None,
        port: 0,
        quiet: true,
        timeout_ms: None,
        include_virtual_threads: false,
        allow: None,
        launch: None,
        on_throw: None,
        on_uncaught: false,
    };
    let mut config = crate::config::VmConfig::default();
    config.jdwp_agent = Some(agent);
    config.jdwp_agent_debugger_facts = true;
    assert!(config.jdwp_debugger_configured());
    let on = crate::vm::SharedVm::new(config.clone());
    assert_eq!(
        debugger_observes_locals(&on),
        crate::config::JDWP_SERVER_COMPILED_IN
    );
    config.jdwp_agent_debugger_facts = false;
    assert!(!config.jdwp_debugger_configured());
    let off = crate::vm::SharedVm::new(config);
    assert!(!debugger_observes_locals(&off));
}
