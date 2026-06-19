// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JVMTI event types and callback registration.

use std::collections::HashSet;

/// All JVMTI event types as defined by the specification.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JvmtiEvent {
    VMInit = 50,
    VMDeath = 51,
    ThreadStart = 52,
    ThreadEnd = 53,
    ClassFileLoadHook = 54,
    ClassLoad = 55,
    ClassPrepare = 56,
    VMStart = 57,
    Exception = 58,
    ExceptionCatch = 59,
    SingleStep = 60,
    FramePop = 61,
    Breakpoint = 62,
    FieldAccess = 63,
    FieldModification = 64,
    MethodEntry = 65,
    MethodExit = 66,
    NativeMethodBind = 67,
    CompiledMethodLoad = 68,
    CompiledMethodUnload = 69,
    DynamicCodeGenerated = 70,
    DataDumpRequest = 71,
    MonitorWait = 73,
    MonitorWaited = 74,
    MonitorContendedEnter = 75,
    MonitorContendedEntered = 76,
    ResourceExhausted = 80,
    GarbageCollectionStart = 81,
    GarbageCollectionFinish = 82,
    ObjectFree = 83,
    VMObjectAlloc = 84,
}

/// Data associated with each event type.
#[derive(Debug, Clone)]
pub enum EventData {
    VMInit {
        thread_id: u64,
    },
    VMDeath,
    ThreadStart {
        thread_id: u64,
        name: String,
    },
    ThreadEnd {
        thread_id: u64,
    },
    ClassLoad {
        class_id: u64,
        name: String,
    },
    ClassPrepare {
        class_id: u64,
        name: String,
    },
    Breakpoint {
        thread_id: u64,
        class_id: u64,
        method_id: u64,
        location: u64,
    },
    MethodEntry {
        thread_id: u64,
        class_id: u64,
        method_id: u64,
    },
    MethodExit {
        thread_id: u64,
        class_id: u64,
        method_id: u64,
        return_value: Option<i64>,
    },
    Exception {
        thread_id: u64,
        class_id: u64,
        method_id: u64,
        location: u64,
        exception_class: String,
    },
    GarbageCollectionStart,
    GarbageCollectionFinish,
    MonitorWait {
        thread_id: u64,
        object_id: u64,
        timeout: i64,
    },
    MonitorContendedEnter {
        thread_id: u64,
        object_id: u64,
    },
    ObjectFree {
        tag: i64,
    },
}

/// Callback function type for JVMTI events.
pub type EventCallback = Box<dyn Fn(&EventData) + Send + Sync>;

/// Collection of registered callbacks, one optional callback per event type.
pub struct EventCallbacks {
    pub on_vm_init: Option<EventCallback>,
    pub on_vm_death: Option<EventCallback>,
    pub on_thread_start: Option<EventCallback>,
    pub on_thread_end: Option<EventCallback>,
    pub on_class_file_load_hook: Option<EventCallback>,
    pub on_class_load: Option<EventCallback>,
    pub on_class_prepare: Option<EventCallback>,
    pub on_vm_start: Option<EventCallback>,
    pub on_exception: Option<EventCallback>,
    pub on_exception_catch: Option<EventCallback>,
    pub on_single_step: Option<EventCallback>,
    pub on_frame_pop: Option<EventCallback>,
    pub on_breakpoint: Option<EventCallback>,
    pub on_field_access: Option<EventCallback>,
    pub on_field_modification: Option<EventCallback>,
    pub on_method_entry: Option<EventCallback>,
    pub on_method_exit: Option<EventCallback>,
    pub on_native_method_bind: Option<EventCallback>,
    pub on_compiled_method_load: Option<EventCallback>,
    pub on_compiled_method_unload: Option<EventCallback>,
    pub on_dynamic_code_generated: Option<EventCallback>,
    pub on_data_dump_request: Option<EventCallback>,
    pub on_monitor_wait: Option<EventCallback>,
    pub on_monitor_waited: Option<EventCallback>,
    pub on_monitor_contended_enter: Option<EventCallback>,
    pub on_monitor_contended_entered: Option<EventCallback>,
    pub on_resource_exhausted: Option<EventCallback>,
    pub on_garbage_collection_start: Option<EventCallback>,
    pub on_garbage_collection_finish: Option<EventCallback>,
    pub on_object_free: Option<EventCallback>,
    pub on_vm_object_alloc: Option<EventCallback>,
}

impl Default for EventCallbacks {
    fn default() -> Self {
        Self {
            on_vm_init: None,
            on_vm_death: None,
            on_thread_start: None,
            on_thread_end: None,
            on_class_file_load_hook: None,
            on_class_load: None,
            on_class_prepare: None,
            on_vm_start: None,
            on_exception: None,
            on_exception_catch: None,
            on_single_step: None,
            on_frame_pop: None,
            on_breakpoint: None,
            on_field_access: None,
            on_field_modification: None,
            on_method_entry: None,
            on_method_exit: None,
            on_native_method_bind: None,
            on_compiled_method_load: None,
            on_compiled_method_unload: None,
            on_dynamic_code_generated: None,
            on_data_dump_request: None,
            on_monitor_wait: None,
            on_monitor_waited: None,
            on_monitor_contended_enter: None,
            on_monitor_contended_entered: None,
            on_resource_exhausted: None,
            on_garbage_collection_start: None,
            on_garbage_collection_finish: None,
            on_object_free: None,
            on_vm_object_alloc: None,
        }
    }
}

impl EventCallbacks {
    /// Get the callback for a given event type, if registered.
    pub fn get(&self, event: JvmtiEvent) -> Option<&EventCallback> {
        match event {
            JvmtiEvent::VMInit => self.on_vm_init.as_ref(),
            JvmtiEvent::VMDeath => self.on_vm_death.as_ref(),
            JvmtiEvent::ThreadStart => self.on_thread_start.as_ref(),
            JvmtiEvent::ThreadEnd => self.on_thread_end.as_ref(),
            JvmtiEvent::ClassFileLoadHook => self.on_class_file_load_hook.as_ref(),
            JvmtiEvent::ClassLoad => self.on_class_load.as_ref(),
            JvmtiEvent::ClassPrepare => self.on_class_prepare.as_ref(),
            JvmtiEvent::VMStart => self.on_vm_start.as_ref(),
            JvmtiEvent::Exception => self.on_exception.as_ref(),
            JvmtiEvent::ExceptionCatch => self.on_exception_catch.as_ref(),
            JvmtiEvent::SingleStep => self.on_single_step.as_ref(),
            JvmtiEvent::FramePop => self.on_frame_pop.as_ref(),
            JvmtiEvent::Breakpoint => self.on_breakpoint.as_ref(),
            JvmtiEvent::FieldAccess => self.on_field_access.as_ref(),
            JvmtiEvent::FieldModification => self.on_field_modification.as_ref(),
            JvmtiEvent::MethodEntry => self.on_method_entry.as_ref(),
            JvmtiEvent::MethodExit => self.on_method_exit.as_ref(),
            JvmtiEvent::NativeMethodBind => self.on_native_method_bind.as_ref(),
            JvmtiEvent::CompiledMethodLoad => self.on_compiled_method_load.as_ref(),
            JvmtiEvent::CompiledMethodUnload => self.on_compiled_method_unload.as_ref(),
            JvmtiEvent::DynamicCodeGenerated => self.on_dynamic_code_generated.as_ref(),
            JvmtiEvent::DataDumpRequest => self.on_data_dump_request.as_ref(),
            JvmtiEvent::MonitorWait => self.on_monitor_wait.as_ref(),
            JvmtiEvent::MonitorWaited => self.on_monitor_waited.as_ref(),
            JvmtiEvent::MonitorContendedEnter => self.on_monitor_contended_enter.as_ref(),
            JvmtiEvent::MonitorContendedEntered => self.on_monitor_contended_entered.as_ref(),
            JvmtiEvent::ResourceExhausted => self.on_resource_exhausted.as_ref(),
            JvmtiEvent::GarbageCollectionStart => self.on_garbage_collection_start.as_ref(),
            JvmtiEvent::GarbageCollectionFinish => self.on_garbage_collection_finish.as_ref(),
            JvmtiEvent::ObjectFree => self.on_object_free.as_ref(),
            JvmtiEvent::VMObjectAlloc => self.on_vm_object_alloc.as_ref(),
        }
    }
}

/// Manages event notification modes and dispatches events to callbacks.
pub struct EventManager {
    pub callbacks: EventCallbacks,
    enabled_events: HashSet<JvmtiEvent>,
}

impl EventManager {
    /// Create a new `EventManager` with no callbacks and no enabled events.
    pub fn new() -> Self {
        Self {
            callbacks: EventCallbacks::default(),
            enabled_events: HashSet::new(),
        }
    }

    /// Enable or disable notifications for a specific event type.
    pub fn set_event_notification_mode(&mut self, event: JvmtiEvent, enabled: bool) {
        if enabled {
            self.enabled_events.insert(event);
        } else {
            self.enabled_events.remove(&event);
        }
    }

    /// Check whether an event type is currently enabled.
    pub fn is_enabled(&self, event: JvmtiEvent) -> bool {
        self.enabled_events.contains(&event)
    }

    /// Fire an event, invoking the registered callback if the event is enabled.
    ///
    /// If the event is not enabled or no callback is registered, this is a no-op.
    pub fn fire_event(&self, event: JvmtiEvent, data: &EventData) {
        if !self.is_enabled(event) {
            return;
        }
        if let Some(cb) = self.callbacks.get(event) {
            cb(data);
        }
    }
}

impl Default for EventManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
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
}
