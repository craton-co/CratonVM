// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JVMTI (JVM Tool Interface) foundation.
//!
//! Provides the core types for agent loading, event callbacks, and
//! capability negotiation as defined by the JVMTI specification.

pub mod agent;
pub mod events;
/// The C `jvmtiEnv` `GetEnv` hands a native agent (interpreter round i1
/// wave 12).
pub(crate) mod native_env;

pub use agent::{parse_agent_arg, AgentInfo, AgentLibrary, AgentPhase, AgentRegistry};
pub use events::{EventCallbacks, EventData, EventManager, JvmtiEvent};

// The older model's `capabilities` module (`JvmtiCapabilities`, the Rust-side
// `AddCapabilities` record no agent reached) was deleted in interpreter round
// i6 wave 125 (i53-L1 step 3); the C table's capabilities are
// `native_env`'s `jvmtiCapabilities` words.

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

/// The per-VM JVMTI state: the older Rust-side event manager (still the D14
/// bridge's target) and the C table's records.
pub struct JvmtiEnv {
    pub event_manager: EventManager,
    /// The C table's env lists, OnLoad capability record and (since wave 125)
    /// agent registry (i53-L1, wave 123): what the C table and the agent
    /// loader read, moved out of this older model ([`CTableRecord`]).
    pub(crate) c_table: CTableRecord,
    /// What those agents' `Agent_OnLoad`s did to this VM before it existed
    /// (`SetSystemProperty`, the class search additions), applied when
    /// `Vm::new` binds their envs (interpreter round i2 wave 61, lane L1;
    /// `native_env::bind_startup_envs`).
    pub(crate) startup_onload_effects: native_env::OnLoadEffects,
    /// The `java.lang.Thread`s standing for the OS threads that carry this
    /// VM's virtual threads, by OS thread id, as JNI global references: what
    /// the C table's `GetCarrierThread` answers (interpreter round i2 wave
    /// 63, lane L1; `native_env::extensions`).
    pub(crate) carrier_threads: Vec<(u32, crate::native::jni::JObject)>,
    /// `SetHeapSamplingInterval`'s interval in bytes (HotSpot's default 512
    /// KiB; wave 63).
    pub(crate) heap_sampling_interval: i32,
    /// The fields the C envs watch for access / modification, by
    /// `jfieldID` (`SetFieldAccessWatch`, `SetFieldModificationWatch`;
    /// wave 63, `native_env::events_more`).
    pub(crate) c_field_access_watches: std::collections::HashSet<u64>,
    pub(crate) c_field_modification_watches: std::collections::HashSet<u64>,
    /// The bytes each thread has left to allocate before its next
    /// `SampledObjectAlloc` (wave 63), by VM thread id.
    pub(crate) sampling_remaining: std::collections::HashMap<u64, i64>,
    /// A startup env holds `can_generate_early_class_hook_events` (wave 63,
    /// `native_env::bind_startup_envs`): `ClassFileLoadHook` in the
    /// primordial phase too.
    pub(crate) early_class_hooks: bool,
    /// Whether the native-call funnel posts `NativeMethodBind` for the
    /// registered (Rust) natives it runs (interpreter round i3 wave 83, lane
    /// L1; `native_env::events_more::rust_bind_tracking_step`).
    pub(crate) rust_native_bind_tracking: native_env::RustBindTracking,
    /// `bind_startup_envs` is attaching the startup envs: the one window in
    /// which a `NativeMethodBind` enable may start the tracking (no Java code
    /// has run, so no registered native has been linked yet).
    pub(crate) rust_native_bind_startup: bool,
    /// The registered natives' callbacks the funnel has posted
    /// `NativeMethodBind` for (wave 83): each once, as HotSpot links a
    /// native once.
    pub(crate) rust_native_binds_posted: std::collections::HashSet<usize>,
    /// The methods those binds named (wave 83): a native two callbacks serve
    /// (its registered one and an interpreter intrinsic's trampoline) is
    /// posted once.
    pub(crate) rust_native_bound_methods:
        std::collections::HashSet<(crate::classloading::ClassId, String, String)>,
    /// gci i12/ev (`CRATONVM_JVMTI_DEFER_UNOWNED_CLASS_LOAD`): the C
    /// `ClassLoad`s posted on a thread with no handle of its own installed
    /// (`native_env::post_class_load`), as `(VM thread id, class)`, in post
    /// order; delivered at that thread's next own-handle point
    /// (`native_env::flush_deferred_class_loads`).
    pub(crate) deferred_class_loads: Vec<(u64, crate::classloading::ClassId)>,
}

/// The per-VM state the C table (`native_env`) keeps about its envs,
/// separated from the older JVMTI model it used to sit in (interpreter round
/// i6 wave 123, lane L1; i53-L1's contained first step). A pure move: every
/// reader still reaches it under the VM's JVMTI lock
/// (`shared.debug.jvmti_env.lock().c_table`), so ordering is unchanged.
/// `class_prepare_posted` joined it in wave 124 (lane L1), with the C
/// `ClassLoad` post's adapter-side path (`native_env::post_class_load_at_adapter`,
/// behind `CRATONVM_JVMTI_C_CLASS_LOAD_AT_ADAPTER`). `agent_registry` joined it
/// in wave 125 (i53-L1 step 3), when [`JvmtiEnv`]'s public surface changed
/// anyway (its older `capabilities` / `object_tags` fields were deleted).
#[derive(Debug, Default)]
pub(crate) struct CTableRecord {
    /// The native agent libraries loaded into this VM (`-agentpath` /
    /// `-agentlib` at startup, `VirtualMachine.loadAgentPath` on attach), the
    /// libraries `Agent_OnUnload` is called on at VM death
    /// (`native_env::shut_down_agents`). Moved here from [`JvmtiEnv`] in wave
    /// 125, a pure move under the same lock.
    pub(crate) agent_registry: AgentRegistry,
    /// The C `jvmtiEnv`s (`native_env`) acting on this VM, by address: the
    /// envs `ClassPrepare` is delivered to (`native_env::post_class_prepare`).
    pub(crate) native_envs: Vec<usize>,
    /// The C `jvmtiEnv`s the startup agents obtained in `Agent_OnLoad`, before
    /// this VM existed; `Vm::new` binds them (`native_env::bind_startup_envs`).
    pub(crate) startup_native_envs: Vec<usize>,
    /// The capabilities JVMTI lists as potential in the live phase only once
    /// a startup agent acquired them in `Agent_OnLoad`, that this VM's
    /// startup envs held when `Vm::new` bound them (interpreter round i1
    /// wave 46, lane L1; `native_env::bind_startup_envs`, `native_env::ONLOAD_ONLY`),
    /// as `jvmtiCapabilities` words.
    pub(crate) onload_acquired: [u32; 4],
    /// The classes whose `ClassPrepare` the C envs were sent (interpreter
    /// round i1 wave 15): a class is prepared again when its initialization
    /// is retried after a supertype's link failure, and the event is posted
    /// once per class (`native_env::post_class_prepare`). Moved here from
    /// [`JvmtiEnv`] in wave 124 (i53-L1), a pure move under the same lock.
    pub(crate) class_prepare_posted: std::collections::HashSet<crate::classloading::ClassId>,
}

/// Create a new JVMTI environment with default (empty) state.
pub fn create_jvmti_env() -> JvmtiEnv {
    JvmtiEnv {
        event_manager: EventManager::new(),
        c_table: CTableRecord::default(),
        startup_onload_effects: native_env::OnLoadEffects::default(),
        carrier_threads: Vec::new(),
        heap_sampling_interval: 512 * 1024,
        c_field_access_watches: std::collections::HashSet::new(),
        c_field_modification_watches: std::collections::HashSet::new(),
        sampling_remaining: std::collections::HashMap::new(),
        early_class_hooks: false,
        rust_native_bind_tracking: native_env::RustBindTracking::NotArmed,
        rust_native_bind_startup: false,
        rust_native_binds_posted: std::collections::HashSet::new(),
        rust_native_bound_methods: std::collections::HashSet::new(),
        deferred_class_loads: Vec::new(),
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

// The older model's `ObjectTagMap` (an address-keyed tag table nothing
// remapped or swept outside its own tests) and the heap walks that read it
// (`iterate_over_heap`, `iterate_over_instances_of_class`, `HeapObjectInfo`,
// `HeapVisitControl`) were deleted in interpreter round i6 wave 125 (i53-L1
// step 3): the C table's tags and heap walks are `native_env::heap`.

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

// GetLocalVariable / SetLocalVariable: not here. The JDWP server reads and
// writes locals through `debug::debugger_local` (kind-aware) over the frame
// listing JDWP publishes; the C JVMTI table (`native_env.rs`) does not offer
// the local functions yet, and when it does they belong on the same path
// (`docs/feature-designs/jvmti-delivery-threading.md`).
// A provider-trait copy that read `Frame::get_local`, counted interpreter
// frames only and built an object from any raw `i64` lived here with no
// production caller until interpreter round i1 wave 29 deleted it.

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

#[cfg(test)]
mod tests;
