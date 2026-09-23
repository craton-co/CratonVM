// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JVMTI (JVM Tool Interface) foundation.
//!
//! Provides the core types for agent loading, event callbacks, and
//! capability negotiation as defined by the JVMTI specification.

pub mod agent;
pub mod capabilities;
pub mod events;

pub use agent::{parse_agent_arg, AgentInfo, AgentLibrary, AgentPhase, AgentRegistry};
pub use capabilities::JvmtiCapabilities;
pub use events::{EventCallbacks, EventData, EventManager, JvmtiEvent};

use std::collections::HashMap;
use std::sync::RwLock;

/// Errors returned by JVMTI operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JvmtiError {
    /// The specified event type is invalid.
    InvalidEvent,
    /// The requested capability is not available.
    InvalidCapability,
    /// The requested functionality is not available in this VM.
    NotAvailable,
    /// The caller does not have permission to perform this operation.
    AccessDenied,
    /// The operation is not valid in the current VM phase.
    WrongPhase,
    /// An internal error occurred.
    Internal,
    /// A requested startup/attach agent library could not be loaded.
    AgentLibraryLoadFailed { path: String, cause: String },
    /// A loaded agent library did not export the required entry point.
    AgentEntryPointMissing { path: String, symbol: String },
    /// An agent entry point returned a non-zero error code.
    AgentEntryPointFailed {
        path: String,
        symbol: String,
        code: i32,
    },
    /// Agent options could not be passed to the native entry point.
    InvalidAgentOptions { path: String, cause: String },
}

impl std::fmt::Display for JvmtiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JvmtiError::InvalidEvent => write!(f, "invalid JVMTI event"),
            JvmtiError::InvalidCapability => write!(f, "invalid or unavailable JVMTI capability"),
            JvmtiError::NotAvailable => write!(f, "JVMTI functionality not available"),
            JvmtiError::AccessDenied => write!(f, "JVMTI access denied"),
            JvmtiError::WrongPhase => write!(f, "wrong VM phase for JVMTI operation"),
            JvmtiError::Internal => write!(f, "internal JVMTI error"),
            JvmtiError::AgentLibraryLoadFailed { path, cause } => {
                write!(f, "failed to load JVMTI agent library `{path}`: {cause}")
            }
            JvmtiError::AgentEntryPointMissing { path, symbol } => {
                write!(f, "JVMTI agent library `{path}` does not export `{symbol}`")
            }
            JvmtiError::AgentEntryPointFailed { path, symbol, code } => {
                write!(
                    f,
                    "JVMTI agent `{path}` entry point `{symbol}` returned {code}"
                )
            }
            JvmtiError::InvalidAgentOptions { path, cause } => {
                write!(f, "invalid JVMTI options for `{path}`: {cause}")
            }
        }
    }
}

impl std::error::Error for JvmtiError {}

/// The top-level JVMTI environment combining events, capabilities, and agents.
pub struct JvmtiEnv {
    pub event_manager: EventManager,
    pub capabilities: JvmtiCapabilities,
    pub agent_registry: AgentRegistry,
    pub object_tags: ObjectTagMap,
}

/// Create a new JVMTI environment with default (empty) state.
pub fn create_jvmti_env() -> JvmtiEnv {
    JvmtiEnv {
        event_manager: EventManager::new(),
        capabilities: JvmtiCapabilities::default(),
        agent_registry: AgentRegistry::new(),
        object_tags: ObjectTagMap::new(),
    }
}

// ---------------------------------------------------------------------------
// Notification fire-points — called from the VM at appropriate times.
// ---------------------------------------------------------------------------

/// Notify agents that the VM has initialized.
pub fn notify_vm_init(env: &JvmtiEnv, thread_id: u64) {
    env.event_manager
        .fire_event(JvmtiEvent::VMInit, &EventData::VMInit { thread_id });
}

/// Notify agents that the VM is shutting down.
pub fn notify_vm_death(env: &JvmtiEnv) {
    env.event_manager
        .fire_event(JvmtiEvent::VMDeath, &EventData::VMDeath);
}

/// Notify agents that a thread has started.
pub fn notify_thread_start(env: &JvmtiEnv, thread_id: u64, name: &str) {
    env.event_manager.fire_event(
        JvmtiEvent::ThreadStart,
        &EventData::ThreadStart {
            thread_id,
            name: name.to_string(),
        },
    );
}

/// Notify agents that a class has been prepared.
pub fn notify_class_prepare(env: &JvmtiEnv, class_id: u64, name: &str) {
    env.event_manager.fire_event(
        JvmtiEvent::ClassPrepare,
        &EventData::ClassPrepare {
            class_id,
            name: name.to_string(),
        },
    );
}

/// Notify agents that a breakpoint has been hit.
pub fn notify_breakpoint(
    env: &JvmtiEnv,
    thread_id: u64,
    class_id: u64,
    method_id: u64,
    location: u64,
) {
    env.event_manager.fire_event(
        JvmtiEvent::Breakpoint,
        &EventData::Breakpoint {
            thread_id,
            class_id,
            method_id,
            location,
        },
    );
}

/// Notify agents that a method has been entered.
pub fn notify_method_entry(env: &JvmtiEnv, thread_id: u64, class_id: u64, method_id: u64) {
    env.event_manager.fire_event(
        JvmtiEvent::MethodEntry,
        &EventData::MethodEntry {
            thread_id,
            class_id,
            method_id,
        },
    );
}

/// Notify agents that a garbage collection cycle has started.
pub fn notify_gc_start(env: &JvmtiEnv) {
    env.event_manager.fire_event(
        JvmtiEvent::GarbageCollectionStart,
        &EventData::GarbageCollectionStart,
    );
}

/// Notify agents that a garbage collection cycle has finished.
pub fn notify_gc_finish(env: &JvmtiEnv) {
    env.event_manager.fire_event(
        JvmtiEvent::GarbageCollectionFinish,
        &EventData::GarbageCollectionFinish,
    );
}

/// Notify agents that a thread has ended.
pub fn notify_thread_end(env: &JvmtiEnv, thread_id: u64) {
    env.event_manager
        .fire_event(JvmtiEvent::ThreadEnd, &EventData::ThreadEnd { thread_id });
}

/// Notify agents that a class has been loaded.
pub fn notify_class_load(env: &JvmtiEnv, class_id: u64, name: &str) {
    env.event_manager.fire_event(
        JvmtiEvent::ClassLoad,
        &EventData::ClassLoad {
            class_id,
            name: name.to_string(),
        },
    );
}

/// Notify agents that a method has been exited.
pub fn notify_method_exit(
    env: &JvmtiEnv,
    thread_id: u64,
    class_id: u64,
    method_id: u64,
    return_value: Option<i64>,
) {
    env.event_manager.fire_event(
        JvmtiEvent::MethodExit,
        &EventData::MethodExit {
            thread_id,
            class_id,
            method_id,
            return_value,
        },
    );
}

/// Notify agents that an exception was thrown.
pub fn notify_exception(
    env: &JvmtiEnv,
    thread_id: u64,
    class_id: u64,
    method_id: u64,
    location: u64,
    exception_class: &str,
) {
    env.event_manager.fire_event(
        JvmtiEvent::Exception,
        &EventData::Exception {
            thread_id,
            class_id,
            method_id,
            location,
            exception_class: exception_class.to_string(),
        },
    );
}

/// Notify agents that a monitor wait has started.
pub fn notify_monitor_wait(env: &JvmtiEnv, thread_id: u64, object_id: u64, timeout: i64) {
    env.event_manager.fire_event(
        JvmtiEvent::MonitorWait,
        &EventData::MonitorWait {
            thread_id,
            object_id,
            timeout,
        },
    );
}

/// Notify agents about monitor contention.
pub fn notify_monitor_contended_enter(env: &JvmtiEnv, thread_id: u64, object_id: u64) {
    env.event_manager.fire_event(
        JvmtiEvent::MonitorContendedEnter,
        &EventData::MonitorContendedEnter {
            thread_id,
            object_id,
        },
    );
}

/// Notify agents that a tagged object has been freed.
pub fn notify_object_free(env: &JvmtiEnv, tag: i64) {
    env.event_manager
        .fire_event(JvmtiEvent::ObjectFree, &EventData::ObjectFree { tag });
}

// ---------------------------------------------------------------------------
// Object Tagging
// ---------------------------------------------------------------------------

/// Object tag storage for JVMTI tagging API.
/// Maps object addresses (as usize) to user-defined i64 tags.
pub struct ObjectTagMap {
    tags: RwLock<HashMap<usize, i64>>,
}

impl ObjectTagMap {
    pub fn new() -> Self {
        Self {
            tags: RwLock::new(HashMap::new()),
        }
    }

    /// Set a tag on an object. Tag of 0 removes the tag.
    pub fn set_tag(&self, obj_addr: usize, tag: i64) {
        let mut map = self.tags.write().unwrap();
        if tag == 0 {
            map.remove(&obj_addr);
        } else {
            map.insert(obj_addr, tag);
        }
    }

    /// Get the tag for an object. Returns 0 if not tagged.
    pub fn get_tag(&self, obj_addr: usize) -> i64 {
        self.tags
            .read()
            .unwrap()
            .get(&obj_addr)
            .copied()
            .unwrap_or(0)
    }

    /// Remove all tags for objects not in the provided live set.
    /// Called after GC to clean up dead object tags.
    /// Returns freed tags for ObjectFree event notification.
    pub fn sweep_dead(&self, live_addrs: &std::collections::HashSet<usize>) -> Vec<i64> {
        let mut map = self.tags.write().unwrap();
        let mut freed = Vec::new();
        map.retain(|addr, tag| {
            if live_addrs.contains(addr) {
                true
            } else {
                freed.push(*tag);
                false
            }
        });
        freed
    }

    /// Update tags after GC compaction. Applies forwarding map.
    pub fn update_after_gc(&self, forwarding: &cratonvm_types::PointerMap) {
        let mut map = self.tags.write().unwrap();
        let entries: Vec<(usize, i64)> = map.drain().collect();
        for (old_addr, tag) in entries {
            let new_addr = forwarding.get(&old_addr).copied().unwrap_or(old_addr);
            map.insert(new_addr, tag);
        }
    }

    /// Number of tagged objects.
    pub fn count(&self) -> usize {
        self.tags.read().unwrap().len()
    }

    /// Snapshot of all tags (for iteration).
    pub fn snapshot(&self) -> Vec<(usize, i64)> {
        self.tags
            .read()
            .unwrap()
            .iter()
            .map(|(&a, &t)| (a, t))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Heap Iteration
// ---------------------------------------------------------------------------

/// Information about a heap object during iteration.
#[derive(Debug, Clone)]
pub struct HeapObjectInfo {
    /// Raw address of the object.
    pub address: usize,
    /// Total byte size including header.
    pub size: usize,
    /// ClassId of the object.
    pub class_id: u32,
    /// Current tag (0 if not tagged).
    pub tag: i64,
}

/// Heap iteration control returned by callbacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeapVisitControl {
    /// Continue iterating.
    Continue,
    /// Stop iteration early.
    Abort,
}

/// Iterate over all heap objects, calling the visitor for each.
/// The visitor receives object info and can return Abort to stop early.
pub fn iterate_over_heap(
    heap_objects: &[(*mut u8, usize)],
    tag_map: &ObjectTagMap,
    mut visitor: impl FnMut(&HeapObjectInfo) -> HeapVisitControl,
) {
    for &(ptr, size) in heap_objects {
        let addr = ptr as usize;
        let header = unsafe { &*(ptr as *const cratonvm_types::ObjectHeader) };
        let info = HeapObjectInfo {
            address: addr,
            size,
            class_id: header.class_id.as_u32(),
            tag: tag_map.get_tag(addr),
        };
        if visitor(&info) == HeapVisitControl::Abort {
            return;
        }
    }
}

/// Iterate over heap objects of a specific class only.
pub fn iterate_over_instances_of_class(
    heap_objects: &[(*mut u8, usize)],
    tag_map: &ObjectTagMap,
    target_class_id: u32,
    mut visitor: impl FnMut(&HeapObjectInfo) -> HeapVisitControl,
) {
    for &(ptr, size) in heap_objects {
        let header = unsafe { &*(ptr as *const cratonvm_types::ObjectHeader) };
        if header.class_id.as_u32() != target_class_id {
            continue;
        }
        let addr = ptr as usize;
        let info = HeapObjectInfo {
            address: addr,
            size,
            class_id: target_class_id,
            tag: tag_map.get_tag(addr),
        };
        if visitor(&info) == HeapVisitControl::Abort {
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// GetClassMethods
// ---------------------------------------------------------------------------

/// Information about a single method, returned by GetClassMethods.
#[derive(Debug, Clone)]
pub struct JvmtiMethodInfo {
    /// Unique method ID (hash of class + name + descriptor).
    pub method_id: u64,
    /// Method name (e.g., "toString").
    pub name: String,
    /// Method descriptor (e.g., "(I)V").
    pub descriptor: String,
    /// Access flags (public, static, native, etc.).
    pub access_flags: u16,
}

/// Get all methods declared in a class.
/// Uses the class_manager to look up the class by id and enumerate its methods.
pub fn get_class_methods(
    class_manager: &dyn ClassMethodProvider,
    class_id: u32,
) -> Result<Vec<JvmtiMethodInfo>, JvmtiError> {
    class_manager
        .get_methods(class_id)
        .ok_or(JvmtiError::NotAvailable)
}

/// Trait abstracting class method lookup (for testability).
pub trait ClassMethodProvider {
    fn get_methods(&self, class_id: u32) -> Option<Vec<JvmtiMethodInfo>>;
}

// ---------------------------------------------------------------------------
// GetLocalVariable / SetLocalVariable
// ---------------------------------------------------------------------------

/// Get a local variable value from a frame.
/// slot is the local variable index. Returns the raw i64 representation.
pub fn get_local_variable(
    locals_provider: &dyn LocalVariableProvider,
    thread_id: u64,
    frame_depth: usize,
    slot: usize,
) -> Result<i64, JvmtiError> {
    locals_provider
        .get_local(thread_id, frame_depth, slot)
        .ok_or(JvmtiError::NotAvailable)
}

/// Set a local variable value in a frame.
pub fn set_local_variable(
    locals_provider: &mut dyn LocalVariableProvider,
    thread_id: u64,
    frame_depth: usize,
    slot: usize,
    value: i64,
    type_tag: u8,
) -> Result<(), JvmtiError> {
    locals_provider
        .set_local(thread_id, frame_depth, slot, value, type_tag)
        .ok_or(JvmtiError::NotAvailable)
}

/// Trait abstracting local variable access (for testability).
pub trait LocalVariableProvider {
    fn get_local(&self, thread_id: u64, frame_depth: usize, slot: usize) -> Option<i64>;
    fn set_local(
        &mut self,
        thread_id: u64,
        frame_depth: usize,
        slot: usize,
        value: i64,
        type_tag: u8,
    ) -> Option<()>;
}

// ---------------------------------------------------------------------------
// Real ClassMethodProvider backed by ClassManager
// ---------------------------------------------------------------------------

/// A ClassMethodProvider backed by a read-locked ClassManager reference.
/// Wraps a `parking_lot::RwLock<ClassManager>` for thread-safe access.
pub struct VmClassMethodProvider<'a> {
    pub class_manager: &'a parking_lot::RwLock<crate::classloading::ClassManager>,
}

impl<'a> ClassMethodProvider for VmClassMethodProvider<'a> {
    fn get_methods(&self, class_id: u32) -> Option<Vec<JvmtiMethodInfo>> {
        let cm = self.class_manager.read();
        let class = cm.get_class(crate::classloading::ClassId::new(class_id))?;
        let methods = class
            .methods
            .iter()
            .enumerate()
            .map(|(idx, m)| {
                // Generate stable method_id from class_id + method index
                let method_id = ((class_id as u64) << 32) | (idx as u64);
                JvmtiMethodInfo {
                    method_id,
                    name: m.name.to_string(),
                    descriptor: m.descriptor.to_string(),
                    access_flags: m.access_flags.bits(),
                }
            })
            .collect();
        Some(methods)
    }
}

// ---------------------------------------------------------------------------
// Real LocalVariableProvider backed by ThreadRegistry
// ---------------------------------------------------------------------------

/// A LocalVariableProvider backed by a direct reference to a JvmThread.
/// In JVMTI, local variable access requires the target thread to be suspended.
/// This provider works with the current thread's frames directly.
pub struct VmLocalVariableProvider<'a> {
    pub thread: &'a mut crate::threading::jvm_thread::JvmThread,
    pub thread_id: u64,
    /// The heap the returned raw addresses belong to.
    ///
    /// `get_local` on an object local hands an agent that object's raw
    /// address as an `i64` — a long-smuggle mint, which must be registered
    /// in `memory::smuggled_longs` against the heap that owns it (a mint
    /// table is per-heap; see that module). `JvmThread` carries no route to
    /// its `SharedVm`, so the constructor supplies it.
    ///
    /// `None` disables the mint registration, and a provider built that way
    /// will leave a smuggled JVMTI local un-rewritable across a moving
    /// collection. Only the unit tests below construct one; any production
    /// wiring MUST pass `Some(&shared.mem.heap)`.
    pub heap: Option<&'a crate::memory::VmHeap>,
}

impl<'a> LocalVariableProvider for VmLocalVariableProvider<'a> {
    fn get_local(&self, thread_id: u64, frame_depth: usize, slot: usize) -> Option<i64> {
        if thread_id != self.thread_id {
            return None; // Can only access own thread's locals
        }
        if frame_depth >= self.thread.frames.len() {
            return None;
        }
        let frame_idx = self.thread.frames.len() - 1 - frame_depth;
        let frame = &self.thread.frames[frame_idx];
        let val = frame.get_local(slot as u16);
        Some(match val {
            crate::types::Value::Int(v) => v as i64,
            crate::types::Value::Long(v) => v,
            crate::types::Value::Float(v) => f32::to_bits(v) as i64,
            crate::types::Value::Double(v) => f64::to_bits(v) as i64,
            crate::types::Value::Object(Some(r)) => {
                // Long-smuggle mint chokepoint: an agent reading an object
                // local receives its raw address as i64 and may re-inject it
                // (set_local) or hold it in Java-visible longs. Register the
                // exact value (definitionally an object start) so the GC
                // rewrite arm treats it as a genuine handle. The mint table
                // is per-heap, so this needs the owning heap (see `heap`).
                if let Some(heap) = self.heap {
                    crate::memory::smuggled_longs::record_minted_long(heap, r.as_ptr() as u64);
                }
                r.as_ptr() as i64
            }
            crate::types::Value::Object(None) => 0,
            crate::types::Value::Uninitialized => return None,
            crate::types::Value::ReturnAddress(addr) => addr as i64,
        })
    }

    fn set_local(
        &mut self,
        thread_id: u64,
        frame_depth: usize,
        slot: usize,
        value: i64,
        type_tag: u8,
    ) -> Option<()> {
        if thread_id != self.thread_id {
            return None;
        }
        if frame_depth >= self.thread.frames.len() {
            return None;
        }
        let frame_idx = self.thread.frames.len() - 1 - frame_depth;
        let frame = &mut self.thread.frames[frame_idx];
        let val = match type_tag {
            b'I' => crate::types::Value::Int(value as i32),
            b'J' => crate::types::Value::Long(value),
            b'F' => crate::types::Value::Float(f32::from_bits(value as u32)),
            b'D' => crate::types::Value::Double(f64::from_bits(value as u64)),
            b'L' => {
                if value == 0 {
                    crate::types::Value::Object(None)
                } else {
                    let ptr = value as usize as *mut u8;
                    crate::types::Value::Object(Some(unsafe {
                        crate::types::ObjectRef::from_raw(ptr)
                    }))
                }
            }
            _ => crate::types::Value::Int(value as i32),
        };
        frame.set_local(slot as u16, val);
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[test]
    fn test_create_jvmti_env() {
        let env = create_jvmti_env();
        assert!(!env.event_manager.is_enabled(JvmtiEvent::VMInit));
        assert!(!env.capabilities.can_tag_objects);
        assert!(env.agent_registry.agents().is_empty());
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
            .agent_registry
            .load_agent("myagent.so", "debug=true")
            .expect_err("missing native agent should fail closed");
        assert!(matches!(err, JvmtiError::AgentLibraryLoadFailed { .. }));
        assert!(env.agent_registry.agents().is_empty());

        // Request capabilities. obsaudit D14 (2026-07-26): this used to
        // request can_generate_breakpoint_events, which JvmtiCapabilities::
        // potential() no longer grants — breakpoint events are never fired
        // in production (see the doc comment on potential()), so requesting
        // that capability now correctly fails with InvalidCapability.
        // Garbage-collection events, unlike breakpoints, are wired all the
        // way from the interpreter/GC to a real agent (the D14 bridge), so
        // this exercises the same local-workflow mechanics against a
        // capability that is actually honest to request.
        let mut req = JvmtiCapabilities::default();
        req.can_generate_garbage_collection_events = true;
        env.capabilities.add_capabilities(&req).unwrap();
        assert!(env
            .capabilities
            .has_capability("can_generate_garbage_collection_events"));

        // Enable events and fire.
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
    fn test_object_tag_map_set_get() {
        let tags = ObjectTagMap::new();
        assert_eq!(tags.get_tag(0x1000), 0);
        tags.set_tag(0x1000, 42);
        assert_eq!(tags.get_tag(0x1000), 42);
        tags.set_tag(0x1000, 0); // remove
        assert_eq!(tags.get_tag(0x1000), 0);
    }

    #[test]
    fn test_object_tag_map_sweep_dead() {
        let tags = ObjectTagMap::new();
        tags.set_tag(0x1000, 1);
        tags.set_tag(0x2000, 2);
        tags.set_tag(0x3000, 3);
        let live: std::collections::HashSet<usize> = [0x1000, 0x3000].iter().copied().collect();
        let freed = tags.sweep_dead(&live);
        assert_eq!(freed, vec![2]);
        assert_eq!(tags.count(), 2);
        assert_eq!(tags.get_tag(0x2000), 0);
    }

    #[test]
    fn test_object_tag_map_update_after_gc() {
        let tags = ObjectTagMap::new();
        tags.set_tag(0x1000, 10);
        tags.set_tag(0x2000, 20);
        let mut forwarding = cratonvm_types::PointerMap::default();
        forwarding.insert(0x1000usize, 0x5000usize);
        tags.update_after_gc(&forwarding);
        assert_eq!(tags.get_tag(0x5000), 10);
        assert_eq!(tags.get_tag(0x2000), 20);
        assert_eq!(tags.get_tag(0x1000), 0);
    }

    #[test]
    fn test_heap_iteration() {
        // Use a mock: iterate_over_heap takes pre-walked objects
        let tags = ObjectTagMap::new();
        tags.set_tag(0x100, 77);
        // No real heap objects, just test the API compiles and iterates
        let objects: Vec<(*mut u8, usize)> = Vec::new();
        let mut count = 0;
        iterate_over_heap(&objects, &tags, |_info| {
            count += 1;
            HeapVisitControl::Continue
        });
        assert_eq!(count, 0);
    }

    #[test]
    fn test_heap_iteration_abort() {
        let tags = ObjectTagMap::new();
        let objects: Vec<(*mut u8, usize)> = Vec::new();
        let mut count = 0;
        iterate_over_heap(&objects, &tags, |_info| {
            count += 1;
            HeapVisitControl::Abort
        });
        assert_eq!(count, 0);
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
    fn test_object_tag_snapshot() {
        let tags = ObjectTagMap::new();
        tags.set_tag(0x1000, 10);
        tags.set_tag(0x2000, 20);
        let snap = tags.snapshot();
        assert_eq!(snap.len(), 2);
        assert!(snap.contains(&(0x1000, 10)));
        assert!(snap.contains(&(0x2000, 20)));
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
    fn test_get_local_variable_trait() {
        struct MockLocals;
        impl LocalVariableProvider for MockLocals {
            fn get_local(&self, _thread_id: u64, _frame_depth: usize, slot: usize) -> Option<i64> {
                if slot == 0 {
                    Some(42)
                } else {
                    None
                }
            }
            fn set_local(
                &mut self,
                _thread_id: u64,
                _frame_depth: usize,
                _slot: usize,
                _value: i64,
                _type_tag: u8,
            ) -> Option<()> {
                Some(())
            }
        }
        let provider = MockLocals;
        assert_eq!(get_local_variable(&provider, 1, 0, 0).unwrap(), 42);
        assert!(get_local_variable(&provider, 1, 0, 5).is_err());
    }

    #[test]
    fn test_vm_local_variable_provider() {
        use crate::classloading::ClassId;
        use crate::runtime::frame::Frame;
        use crate::threading::jvm_thread::JvmThread;

        let mut thread = JvmThread::new(crate::threading::jvm_thread::ThreadId(1), "test");
        let frame = Frame::new(
            ClassId::new(0),
            "TestClass".to_string(),
            "testMethod".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            4, // max_stack
            4, // max_locals
            &[crate::types::Value::Int(42), crate::types::Value::Long(100)],
        );
        thread.frames.push(frame);

        let provider = VmLocalVariableProvider {
            thread: &mut thread,
            thread_id: 1,
            heap: None,
        };

        // Read int local at slot 0
        assert_eq!(provider.get_local(1, 0, 0).unwrap(), 42);
        // Read long local at slot 1
        assert_eq!(provider.get_local(1, 0, 1).unwrap(), 100);
        // Wrong thread returns None
        assert!(provider.get_local(999, 0, 0).is_none());
        // Frame depth out of bounds
        assert!(provider.get_local(1, 5, 0).is_none());
    }

    #[test]
    fn test_vm_local_variable_provider_set() {
        use crate::classloading::ClassId;
        use crate::runtime::frame::Frame;
        use crate::threading::jvm_thread::JvmThread;

        let mut thread = JvmThread::new(crate::threading::jvm_thread::ThreadId(1), "test");
        let frame = Frame::new(
            ClassId::new(0),
            "TestClass".to_string(),
            "testMethod".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            4,
            4,
            &[crate::types::Value::Int(0)],
        );
        thread.frames.push(frame);

        let mut provider = VmLocalVariableProvider {
            thread: &mut thread,
            thread_id: 1,
            heap: None,
        };

        // Set int local
        assert!(provider.set_local(1, 0, 0, 77, b'I').is_some());
        assert_eq!(provider.get_local(1, 0, 0).unwrap(), 77);

        // Set long local
        assert!(provider.set_local(1, 0, 1, 12345, b'J').is_some());
        assert_eq!(provider.get_local(1, 0, 1).unwrap(), 12345);

        // Wrong thread returns None
        assert!(provider.set_local(999, 0, 0, 1, b'I').is_none());
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

    #[test]
    fn test_gc_tag_lifecycle() {
        // Test that tag lifecycle operations work together correctly
        let tags = ObjectTagMap::new();
        // Tag some objects
        tags.set_tag(0x1000, 10);
        tags.set_tag(0x2000, 20);
        tags.set_tag(0x3000, 30);
        assert_eq!(tags.count(), 3);

        // Simulate GC: object at 0x1000 moved to 0x5000, 0x3000 moved to 0x6000
        let mut forwarding = cratonvm_types::PointerMap::default();
        forwarding.insert(0x1000usize, 0x5000usize);
        forwarding.insert(0x3000usize, 0x6000usize);
        tags.update_after_gc(&forwarding);

        // Verify forwarded tags
        assert_eq!(tags.get_tag(0x5000), 10);
        assert_eq!(tags.get_tag(0x2000), 20);
        assert_eq!(tags.get_tag(0x6000), 30);
        assert_eq!(tags.get_tag(0x1000), 0); // old addr gone
        assert_eq!(tags.get_tag(0x3000), 0); // old addr gone

        // Sweep: only 0x5000 and 0x6000 survived
        let live: std::collections::HashSet<usize> = [0x5000, 0x6000].iter().copied().collect();
        let freed = tags.sweep_dead(&live);
        assert_eq!(freed, vec![20]); // 0x2000 was dead
        assert_eq!(tags.count(), 2);
    }

    #[test]
    fn test_heap_iteration_by_class() {
        let tags = ObjectTagMap::new();
        let objects: Vec<(*mut u8, usize)> = Vec::new();
        let mut count = 0;
        iterate_over_instances_of_class(&objects, &tags, 42, |_info| {
            count += 1;
            HeapVisitControl::Continue
        });
        assert_eq!(count, 0);
    }
}
