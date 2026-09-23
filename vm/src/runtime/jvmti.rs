// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JVMTI (JVM Tool Interface) implementation.
//!
//! Provides the JVMTI function table, agent loading support, event delivery
//! infrastructure, and capabilities management modelled on the JVMTI
//! specification (JSR-163 / JVM TI 11.0+).
//!
//! ---------------------------------------------------------------------------
//! # LIVENESS AND SCOPE — established by the observability audit, 2026-07-26
//! ---------------------------------------------------------------------------
//!
//! **There are two JVMTI implementations in this tree.** Know which one you
//! are looking at:
//!
//! | | `vm/src/runtime/jvmti.rs` (this file) | `vm/src/jvmti/` |
//! |---|---|---|
//! | Event delivery | `JvmtiEventManager` + `fire_*` free functions | `EventManager` + `EventCallbacks` |
//! | Callback type | in-process Rust `Box<dyn Fn>` | in-process Rust `Box<dyn Fn>` |
//! | Reached by native agents | **no** — see the bridge below | yes — `agent.rs` does a real `libloading` `dlopen` + `Agent_OnLoad` |
//! | Wired to the interpreter | **yes** — see below | only via the bridge (D14) |
//! | Gated on a cargo feature | no | `experimental-debug` (on by default) |
//!
//! **obsaudit D14 (2026-07-26): a one-way bridge now forwards 9 event kinds
//! from this file to the real, native-agent-facing env** — see
//! `install_real_agent_env_bridge` near the bottom of this file for exactly
//! which ones and why not all of them. This is a bridge, not a merge: the two
//! `JvmtiEventManager`/`EventManager` types, their event enums, and their two
//! `JvmtiCapabilities` structs remain separate. A real agent now receives
//! VMInit, VMDeath, ThreadStart, ThreadEnd, ClassLoad, ClassPrepare,
//! GarbageCollectionStart/Finish, and ObjectFree; it still receives nothing
//! for method-level tracing (MethodEntry/Exit, SingleStep, Breakpoint,
//! FramePop, FieldAccess/Modification) or monitor contention events, and
//! `vm/src/jvmti/capabilities.rs`'s `JvmtiCapabilities::potential()` was
//! corrected to advertise `false` for exactly those, so `AddCapabilities`
//! honestly reports what an agent will and won't see. Full unification
//! remains a separate, larger task.
//!
//! ---------------------------------------------------------------------------
//! # SCOPE: one JVMTI environment per VM (C2 review remediation, 2026-08-01)
//! ---------------------------------------------------------------------------
//!
//! JVMTI's own model is one `jvmtiEnv` per agent per **VM**. This file used to
//! hold two process-global cells instead — `GLOBAL_MANAGER` (every callback
//! and every `any_*_listener` fast-path flag) and `REAL_AGENT_ENV_BRIDGE` (a
//! single `Weak<SharedVm>`) — so event delivery had no VM scope at all. They
//! are replaced by `ENVIRONMENTS`, a `vm_identity`-keyed registry of
//! `VmJvmtiEnvironment` rows (manager + bridge + field watchpoints).
//!
//! Two rules to keep in mind when adding to this file:
//!
//!  * **Guards may over-approximate; delivery may not.** `any_*_listener_active()`
//!    reads a process-wide union mirror so the interpreter's per-opcode check
//!    stays a single atomic load. Every `fire_*` resolves the exact owning VM
//!    before it dispatches.
//!  * **`UNATTRIBUTED_VM` (`0`) is a migration seam, not a scope.** Call sites
//!    that cannot supply a `vm_identity` land there, and per-VM lookups fall
//!    back to it. Prefer the `*_for_vm` entry points; see
//!    `jvmti-vm-scoping.md` for what is still unattributed
//!    and why.
//!
//! What IS live in this file on a default build:
//!
//!  * `install_global_manager` runs unconditionally from `SharedVm::new`, so
//!    the unattributed `JvmtiEventManager` always exists.
//!  * `fire_class_load` / `fire_class_prepare` are driven by the
//!    `classloading` hook adapters, `fire_gc_start` / `fire_gc_finish` by the
//!    `gc` hook adapters, `fire_vm_init` / `fire_vm_death` by the VM
//!    lifecycle, and `fire_method_entry` / `fire_method_exit` /
//!    `fire_single_step` / `fire_frame_pop` / `fire_field_access_if_watched` /
//!    `fire_field_modification_if_watched` from the interpreter.
//!  * Every one of those call sites is guarded by an `any_*_listener_active()`
//!    atomic check, so with no listener registered the cost is a relaxed load.
//!  * `install_real_agent_env_bridge` runs unconditionally from `Vm::new`
//!    (the real boot path), so the 9 bridged event kinds above reach a real
//!    attached agent without it registering anything on this file's manager.
//!
//! In-tree/embedder listeners (Rust closures registered directly on this
//! file's `JvmtiEventManager`, e.g. in tests) still work exactly as before
//! and are unaffected by the bridge — they are a separate delivery path from
//! `snapshot_envs()`/`self.callbacks`, not routed through `vm/src/jvmti/` at
//! all.
//!
//! ## Known correctness gaps (each documented at its definition)
//!
//!  * `get_local_*` / `set_local_*` operate on a side table, not on real
//!    interpreter frames — see [`JvmtiEnv::get_local_int`].

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
// `OnceLock` is deliberately NOT imported here: the two process-global
// `OnceLock`s this module used to carry (`GLOBAL_MANAGER`,
// `REAL_AGENT_ENV_BRIDGE`) were the C2-review bug, and a bare `OnceLock` in
// this file is now almost always the wrong tool — per-VM state belongs in
// `ENVIRONMENTS`. The one remaining use is a `#[cfg(test)]` serialisation
// mutex, which spells the path out in full.
use std::sync::{Arc, Mutex, RwLock, Weak};

// ---------------------------------------------------------------------------
// JVMTI Version Constants
// ---------------------------------------------------------------------------

/// JVMTI version 11.0 encoded as per spec: major.minor.micro
const JVMTI_VERSION_11: u32 = 0x3000_0000 | (11 << 16) | (0 << 8) | 0;

// ---------------------------------------------------------------------------
// Error Types
// ---------------------------------------------------------------------------

/// All JVMTI error codes as defined by the specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum JvmtiError {
    None = 0,
    InvalidThread = 10,
    InvalidThreadGroup = 11,
    InvalidPriority = 12,
    ThreadNotSuspended = 13,
    ThreadSuspended = 14,
    ThreadNotAlive = 15,
    InvalidObject = 20,
    InvalidClass = 21,
    ClassNotPrepared = 22,
    InvalidMethodId = 23,
    InvalidLocation = 24,
    InvalidFieldId = 25,
    NoMoreFrames = 31,
    OpaqueFrame = 32,
    TypeMismatch = 34,
    InvalidSlot = 35,
    Duplicate = 40,
    NotFound = 41,
    InvalidMonitor = 50,
    NotMonitorOwner = 51,
    Interrupt = 52,
    InvalidClassFormat = 60,
    CircularClassDefinition = 61,
    FailsVerification = 62,
    UnsupportedRedefinitionMethodAdded = 63,
    UnsupportedRedefinitionSchemaChanged = 64,
    InvalidTypeState = 65,
    UnsupportedRedefinitionHierarchyChanged = 66,
    UnsupportedRedefinitionMethodDeleted = 67,
    UnsupportedVersion = 68,
    NamesDontMatch = 69,
    UnsupportedRedefinitionClassModifiersChanged = 70,
    UnsupportedRedefinitionMethodModifiersChanged = 71,
    MustPossessCapability = 99,
    NullPointer = 100,
    AbsentInformation = 101,
    InvalidEnvironment = 116,
    WrongPhase = 112,
    Internal = 113,
    UnattachedThread = 115,
    NotAvailable = 98,
    AccessDenied = 111,
    OutOfMemory = 110,
    IllegalArgument = 103,
}

impl fmt::Display for JvmtiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

impl std::error::Error for JvmtiError {}

impl JvmtiError {
    /// Return the standard JVMTI error name string.
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "JVMTI_ERROR_NONE",
            Self::InvalidThread => "JVMTI_ERROR_INVALID_THREAD",
            Self::InvalidThreadGroup => "JVMTI_ERROR_INVALID_THREAD_GROUP",
            Self::InvalidPriority => "JVMTI_ERROR_INVALID_PRIORITY",
            Self::ThreadNotSuspended => "JVMTI_ERROR_THREAD_NOT_SUSPENDED",
            Self::ThreadSuspended => "JVMTI_ERROR_THREAD_SUSPENDED",
            Self::ThreadNotAlive => "JVMTI_ERROR_THREAD_NOT_ALIVE",
            Self::InvalidObject => "JVMTI_ERROR_INVALID_OBJECT",
            Self::InvalidClass => "JVMTI_ERROR_INVALID_CLASS",
            Self::ClassNotPrepared => "JVMTI_ERROR_CLASS_NOT_PREPARED",
            Self::InvalidMethodId => "JVMTI_ERROR_INVALID_METHODID",
            Self::InvalidLocation => "JVMTI_ERROR_INVALID_LOCATION",
            Self::InvalidFieldId => "JVMTI_ERROR_INVALID_FIELDID",
            Self::NoMoreFrames => "JVMTI_ERROR_NO_MORE_FRAMES",
            Self::OpaqueFrame => "JVMTI_ERROR_OPAQUE_FRAME",
            Self::TypeMismatch => "JVMTI_ERROR_TYPE_MISMATCH",
            Self::InvalidSlot => "JVMTI_ERROR_INVALID_SLOT",
            Self::Duplicate => "JVMTI_ERROR_DUPLICATE",
            Self::NotFound => "JVMTI_ERROR_NOT_FOUND",
            Self::InvalidMonitor => "JVMTI_ERROR_INVALID_MONITOR",
            Self::NotMonitorOwner => "JVMTI_ERROR_NOT_MONITOR_OWNER",
            Self::Interrupt => "JVMTI_ERROR_INTERRUPT",
            Self::InvalidClassFormat => "JVMTI_ERROR_INVALID_CLASS_FORMAT",
            Self::CircularClassDefinition => "JVMTI_ERROR_CIRCULAR_CLASS_DEFINITION",
            Self::FailsVerification => "JVMTI_ERROR_FAILS_VERIFICATION",
            Self::UnsupportedRedefinitionMethodAdded => {
                "JVMTI_ERROR_UNSUPPORTED_REDEFINITION_METHOD_ADDED"
            }
            Self::UnsupportedRedefinitionSchemaChanged => {
                "JVMTI_ERROR_UNSUPPORTED_REDEFINITION_SCHEMA_CHANGED"
            }
            Self::InvalidTypeState => "JVMTI_ERROR_INVALID_TYPESTATE",
            Self::UnsupportedRedefinitionHierarchyChanged => {
                "JVMTI_ERROR_UNSUPPORTED_REDEFINITION_HIERARCHY_CHANGED"
            }
            Self::UnsupportedRedefinitionMethodDeleted => {
                "JVMTI_ERROR_UNSUPPORTED_REDEFINITION_METHOD_DELETED"
            }
            Self::UnsupportedVersion => "JVMTI_ERROR_UNSUPPORTED_VERSION",
            Self::NamesDontMatch => "JVMTI_ERROR_NAMES_DONT_MATCH",
            Self::UnsupportedRedefinitionClassModifiersChanged => {
                "JVMTI_ERROR_UNSUPPORTED_REDEFINITION_CLASS_MODIFIERS_CHANGED"
            }
            Self::UnsupportedRedefinitionMethodModifiersChanged => {
                "JVMTI_ERROR_UNSUPPORTED_REDEFINITION_METHOD_MODIFIERS_CHANGED"
            }
            Self::MustPossessCapability => "JVMTI_ERROR_MUST_POSSESS_CAPABILITY",
            Self::NullPointer => "JVMTI_ERROR_NULL_POINTER",
            Self::AbsentInformation => "JVMTI_ERROR_ABSENT_INFORMATION",
            Self::InvalidEnvironment => "JVMTI_ERROR_INVALID_ENVIRONMENT",
            Self::WrongPhase => "JVMTI_ERROR_WRONG_PHASE",
            Self::Internal => "JVMTI_ERROR_INTERNAL",
            Self::UnattachedThread => "JVMTI_ERROR_UNATTACHED_THREAD",
            Self::NotAvailable => "JVMTI_ERROR_NOT_AVAILABLE",
            Self::AccessDenied => "JVMTI_ERROR_ACCESS_DENIED",
            Self::OutOfMemory => "JVMTI_ERROR_OUT_OF_MEMORY",
            Self::IllegalArgument => "JVMTI_ERROR_ILLEGAL_ARGUMENT",
        }
    }

    /// Convert a raw error code to a JvmtiError.
    pub fn from_code(code: i32) -> Self {
        match code {
            0 => Self::None,
            10 => Self::InvalidThread,
            11 => Self::InvalidThreadGroup,
            12 => Self::InvalidPriority,
            13 => Self::ThreadNotSuspended,
            14 => Self::ThreadSuspended,
            15 => Self::ThreadNotAlive,
            20 => Self::InvalidObject,
            21 => Self::InvalidClass,
            22 => Self::ClassNotPrepared,
            23 => Self::InvalidMethodId,
            24 => Self::InvalidLocation,
            25 => Self::InvalidFieldId,
            31 => Self::NoMoreFrames,
            32 => Self::OpaqueFrame,
            34 => Self::TypeMismatch,
            35 => Self::InvalidSlot,
            40 => Self::Duplicate,
            41 => Self::NotFound,
            50 => Self::InvalidMonitor,
            51 => Self::NotMonitorOwner,
            52 => Self::Interrupt,
            60 => Self::InvalidClassFormat,
            61 => Self::CircularClassDefinition,
            62 => Self::FailsVerification,
            63 => Self::UnsupportedRedefinitionMethodAdded,
            64 => Self::UnsupportedRedefinitionSchemaChanged,
            65 => Self::InvalidTypeState,
            66 => Self::UnsupportedRedefinitionHierarchyChanged,
            67 => Self::UnsupportedRedefinitionMethodDeleted,
            68 => Self::UnsupportedVersion,
            69 => Self::NamesDontMatch,
            70 => Self::UnsupportedRedefinitionClassModifiersChanged,
            71 => Self::UnsupportedRedefinitionMethodModifiersChanged,
            98 => Self::NotAvailable,
            99 => Self::MustPossessCapability,
            100 => Self::NullPointer,
            101 => Self::AbsentInformation,
            103 => Self::IllegalArgument,
            110 => Self::OutOfMemory,
            111 => Self::AccessDenied,
            112 => Self::WrongPhase,
            113 => Self::Internal,
            115 => Self::UnattachedThread,
            116 => Self::InvalidEnvironment,
            _ => Self::Internal,
        }
    }
}

pub type JvmtiResult<T> = Result<T, JvmtiError>;

// ---------------------------------------------------------------------------
// Event Kinds
// ---------------------------------------------------------------------------

/// All JVMTI event kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum JvmtiEventKind {
    VmInit = 50,
    VmDeath = 51,
    ThreadStart = 52,
    ThreadEnd = 53,
    ClassFileLoadHook = 54,
    ClassLoad = 55,
    ClassPrepare = 56,
    MethodEntry = 64,
    MethodExit = 65,
    Exception = 58,
    ExceptionCatch = 59,
    FieldAccess = 63,
    FieldModification = 62,
    Breakpoint = 60,
    SingleStep = 61,
    FramePop = 66,
    GarbageCollectionStart = 75,
    GarbageCollectionFinish = 76,
    MonitorContendedEnter = 77,
    MonitorContendedEntered = 78,
    MonitorWait = 79,
    MonitorWaited = 80,
    CompiledMethodLoad = 68,
    CompiledMethodUnload = 69,
    DynamicCodeGenerated = 70,
    NativeMethodBind = 67,
    /// Fired when a tagged object is garbage-collected. One-shot per object.
    ObjectFree = 83,
    /// Fired for every Java object allocation. Requires the
    /// `can_generate_vm_object_alloc_events` capability.
    VMObjectAlloc = 84,
    /// Fired per heap-sampling interval (default 512 KB of allocation).
    /// Mirrors the JFR allocation sampler used by async-profiler.
    SampledObjectAlloc = 86,
    /// Fired when an external debugger requests a heap dump.
    DataDumpRequest = 71,
}

impl JvmtiEventKind {
    /// All known event kinds for iteration.
    pub const ALL: &'static [JvmtiEventKind] = &[
        Self::VmInit,
        Self::VmDeath,
        Self::ThreadStart,
        Self::ThreadEnd,
        Self::ClassFileLoadHook,
        Self::ClassLoad,
        Self::ClassPrepare,
        Self::MethodEntry,
        Self::MethodExit,
        Self::Exception,
        Self::ExceptionCatch,
        Self::FieldAccess,
        Self::FieldModification,
        Self::Breakpoint,
        Self::SingleStep,
        Self::FramePop,
        Self::GarbageCollectionStart,
        Self::GarbageCollectionFinish,
        Self::MonitorContendedEnter,
        Self::MonitorContendedEntered,
        Self::MonitorWait,
        Self::MonitorWaited,
        Self::CompiledMethodLoad,
        Self::CompiledMethodUnload,
        Self::DynamicCodeGenerated,
        Self::NativeMethodBind,
        Self::ObjectFree,
        Self::VMObjectAlloc,
        Self::SampledObjectAlloc,
        Self::DataDumpRequest,
    ];

    /// Convert from raw u32 event number.
    pub fn from_raw(val: u32) -> Option<Self> {
        Self::ALL.iter().find(|k| **k as u32 == val).copied()
    }
}

// ---------------------------------------------------------------------------
// Notification Mode
// ---------------------------------------------------------------------------

/// Whether an event is enabled or disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventMode {
    Enable,
    Disable,
}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

/// JVMTI capability bitfield. Each field represents a capability that can be
/// requested, granted, and relinquished at runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JvmtiCapabilities {
    pub can_tag_objects: bool,
    pub can_generate_field_modification_events: bool,
    pub can_generate_field_access_events: bool,
    pub can_get_bytecodes: bool,
    pub can_get_synthetic_attribute: bool,
    pub can_get_owned_monitor_info: bool,
    pub can_get_current_contended_monitor: bool,
    pub can_get_monitor_info: bool,
    pub can_pop_frame: bool,
    pub can_redefine_classes: bool,
    pub can_signal_thread: bool,
    pub can_get_source_file_name: bool,
    pub can_get_line_numbers: bool,
    pub can_get_source_debug_extension: bool,
    pub can_access_local_variables: bool,
    pub can_maintain_original_method_order: bool,
    pub can_generate_single_step_events: bool,
    pub can_generate_exception_events: bool,
    pub can_generate_frame_pop_events: bool,
    pub can_generate_breakpoint_events: bool,
    pub can_suspend: bool,
    pub can_redefine_any_class: bool,
    pub can_get_current_thread_cpu_time: bool,
    pub can_get_thread_cpu_time: bool,
    pub can_generate_method_entry_events: bool,
    pub can_generate_method_exit_events: bool,
    pub can_generate_all_class_hook_events: bool,
    pub can_generate_compiled_method_load_events: bool,
    pub can_generate_monitor_events: bool,
    pub can_generate_vm_object_alloc_events: bool,
    pub can_generate_native_method_bind_events: bool,
    pub can_generate_garbage_collection_events: bool,
    pub can_generate_object_free_events: bool,
    pub can_force_early_return: bool,
    pub can_get_owned_monitor_stack_depth_info: bool,
    pub can_get_constant_pool: bool,
    pub can_set_native_method_prefix: bool,
    pub can_retransform_classes: bool,
    pub can_retransform_any_class: bool,
    pub can_generate_resource_exhaustion_heap_events: bool,
    pub can_generate_resource_exhaustion_threads_events: bool,
}

impl JvmtiCapabilities {
    /// Merge capabilities: result has a capability if either input has it.
    pub fn union(&self, other: &Self) -> Self {
        Self {
            can_tag_objects: self.can_tag_objects || other.can_tag_objects,
            can_generate_field_modification_events: self.can_generate_field_modification_events
                || other.can_generate_field_modification_events,
            can_generate_field_access_events: self.can_generate_field_access_events
                || other.can_generate_field_access_events,
            can_get_bytecodes: self.can_get_bytecodes || other.can_get_bytecodes,
            can_get_synthetic_attribute: self.can_get_synthetic_attribute
                || other.can_get_synthetic_attribute,
            can_get_owned_monitor_info: self.can_get_owned_monitor_info
                || other.can_get_owned_monitor_info,
            can_get_current_contended_monitor: self.can_get_current_contended_monitor
                || other.can_get_current_contended_monitor,
            can_get_monitor_info: self.can_get_monitor_info || other.can_get_monitor_info,
            can_pop_frame: self.can_pop_frame || other.can_pop_frame,
            can_redefine_classes: self.can_redefine_classes || other.can_redefine_classes,
            can_signal_thread: self.can_signal_thread || other.can_signal_thread,
            can_get_source_file_name: self.can_get_source_file_name
                || other.can_get_source_file_name,
            can_get_line_numbers: self.can_get_line_numbers || other.can_get_line_numbers,
            can_get_source_debug_extension: self.can_get_source_debug_extension
                || other.can_get_source_debug_extension,
            can_access_local_variables: self.can_access_local_variables
                || other.can_access_local_variables,
            can_maintain_original_method_order: self.can_maintain_original_method_order
                || other.can_maintain_original_method_order,
            can_generate_single_step_events: self.can_generate_single_step_events
                || other.can_generate_single_step_events,
            can_generate_exception_events: self.can_generate_exception_events
                || other.can_generate_exception_events,
            can_generate_frame_pop_events: self.can_generate_frame_pop_events
                || other.can_generate_frame_pop_events,
            can_generate_breakpoint_events: self.can_generate_breakpoint_events
                || other.can_generate_breakpoint_events,
            can_suspend: self.can_suspend || other.can_suspend,
            can_redefine_any_class: self.can_redefine_any_class || other.can_redefine_any_class,
            can_get_current_thread_cpu_time: self.can_get_current_thread_cpu_time
                || other.can_get_current_thread_cpu_time,
            can_get_thread_cpu_time: self.can_get_thread_cpu_time || other.can_get_thread_cpu_time,
            can_generate_method_entry_events: self.can_generate_method_entry_events
                || other.can_generate_method_entry_events,
            can_generate_method_exit_events: self.can_generate_method_exit_events
                || other.can_generate_method_exit_events,
            can_generate_all_class_hook_events: self.can_generate_all_class_hook_events
                || other.can_generate_all_class_hook_events,
            can_generate_compiled_method_load_events: self.can_generate_compiled_method_load_events
                || other.can_generate_compiled_method_load_events,
            can_generate_monitor_events: self.can_generate_monitor_events
                || other.can_generate_monitor_events,
            can_generate_vm_object_alloc_events: self.can_generate_vm_object_alloc_events
                || other.can_generate_vm_object_alloc_events,
            can_generate_native_method_bind_events: self.can_generate_native_method_bind_events
                || other.can_generate_native_method_bind_events,
            can_generate_garbage_collection_events: self.can_generate_garbage_collection_events
                || other.can_generate_garbage_collection_events,
            can_generate_object_free_events: self.can_generate_object_free_events
                || other.can_generate_object_free_events,
            can_force_early_return: self.can_force_early_return || other.can_force_early_return,
            can_get_owned_monitor_stack_depth_info: self.can_get_owned_monitor_stack_depth_info
                || other.can_get_owned_monitor_stack_depth_info,
            can_get_constant_pool: self.can_get_constant_pool || other.can_get_constant_pool,
            can_set_native_method_prefix: self.can_set_native_method_prefix
                || other.can_set_native_method_prefix,
            can_retransform_classes: self.can_retransform_classes || other.can_retransform_classes,
            can_retransform_any_class: self.can_retransform_any_class
                || other.can_retransform_any_class,
            can_generate_resource_exhaustion_heap_events: self
                .can_generate_resource_exhaustion_heap_events
                || other.can_generate_resource_exhaustion_heap_events,
            can_generate_resource_exhaustion_threads_events: self
                .can_generate_resource_exhaustion_threads_events
                || other.can_generate_resource_exhaustion_threads_events,
        }
    }

    /// Remove capabilities present in `other` from self.
    pub fn subtract(&self, other: &Self) -> Self {
        Self {
            can_tag_objects: self.can_tag_objects && !other.can_tag_objects,
            can_generate_field_modification_events: self.can_generate_field_modification_events
                && !other.can_generate_field_modification_events,
            can_generate_field_access_events: self.can_generate_field_access_events
                && !other.can_generate_field_access_events,
            can_get_bytecodes: self.can_get_bytecodes && !other.can_get_bytecodes,
            can_get_synthetic_attribute: self.can_get_synthetic_attribute
                && !other.can_get_synthetic_attribute,
            can_get_owned_monitor_info: self.can_get_owned_monitor_info
                && !other.can_get_owned_monitor_info,
            can_get_current_contended_monitor: self.can_get_current_contended_monitor
                && !other.can_get_current_contended_monitor,
            can_get_monitor_info: self.can_get_monitor_info && !other.can_get_monitor_info,
            can_pop_frame: self.can_pop_frame && !other.can_pop_frame,
            can_redefine_classes: self.can_redefine_classes && !other.can_redefine_classes,
            can_signal_thread: self.can_signal_thread && !other.can_signal_thread,
            can_get_source_file_name: self.can_get_source_file_name
                && !other.can_get_source_file_name,
            can_get_line_numbers: self.can_get_line_numbers && !other.can_get_line_numbers,
            can_get_source_debug_extension: self.can_get_source_debug_extension
                && !other.can_get_source_debug_extension,
            can_access_local_variables: self.can_access_local_variables
                && !other.can_access_local_variables,
            can_maintain_original_method_order: self.can_maintain_original_method_order
                && !other.can_maintain_original_method_order,
            can_generate_single_step_events: self.can_generate_single_step_events
                && !other.can_generate_single_step_events,
            can_generate_exception_events: self.can_generate_exception_events
                && !other.can_generate_exception_events,
            can_generate_frame_pop_events: self.can_generate_frame_pop_events
                && !other.can_generate_frame_pop_events,
            can_generate_breakpoint_events: self.can_generate_breakpoint_events
                && !other.can_generate_breakpoint_events,
            can_suspend: self.can_suspend && !other.can_suspend,
            can_redefine_any_class: self.can_redefine_any_class && !other.can_redefine_any_class,
            can_get_current_thread_cpu_time: self.can_get_current_thread_cpu_time
                && !other.can_get_current_thread_cpu_time,
            can_get_thread_cpu_time: self.can_get_thread_cpu_time && !other.can_get_thread_cpu_time,
            can_generate_method_entry_events: self.can_generate_method_entry_events
                && !other.can_generate_method_entry_events,
            can_generate_method_exit_events: self.can_generate_method_exit_events
                && !other.can_generate_method_exit_events,
            can_generate_all_class_hook_events: self.can_generate_all_class_hook_events
                && !other.can_generate_all_class_hook_events,
            can_generate_compiled_method_load_events: self.can_generate_compiled_method_load_events
                && !other.can_generate_compiled_method_load_events,
            can_generate_monitor_events: self.can_generate_monitor_events
                && !other.can_generate_monitor_events,
            can_generate_vm_object_alloc_events: self.can_generate_vm_object_alloc_events
                && !other.can_generate_vm_object_alloc_events,
            can_generate_native_method_bind_events: self.can_generate_native_method_bind_events
                && !other.can_generate_native_method_bind_events,
            can_generate_garbage_collection_events: self.can_generate_garbage_collection_events
                && !other.can_generate_garbage_collection_events,
            can_generate_object_free_events: self.can_generate_object_free_events
                && !other.can_generate_object_free_events,
            can_force_early_return: self.can_force_early_return && !other.can_force_early_return,
            can_get_owned_monitor_stack_depth_info: self.can_get_owned_monitor_stack_depth_info
                && !other.can_get_owned_monitor_stack_depth_info,
            can_get_constant_pool: self.can_get_constant_pool && !other.can_get_constant_pool,
            can_set_native_method_prefix: self.can_set_native_method_prefix
                && !other.can_set_native_method_prefix,
            can_retransform_classes: self.can_retransform_classes && !other.can_retransform_classes,
            can_retransform_any_class: self.can_retransform_any_class
                && !other.can_retransform_any_class,
            can_generate_resource_exhaustion_heap_events: self
                .can_generate_resource_exhaustion_heap_events
                && !other.can_generate_resource_exhaustion_heap_events,
            can_generate_resource_exhaustion_threads_events: self
                .can_generate_resource_exhaustion_threads_events
                && !other.can_generate_resource_exhaustion_threads_events,
        }
    }

    /// The set of all capabilities that can potentially be granted.
    pub fn potentially_available() -> Self {
        Self {
            can_tag_objects: true,
            can_generate_field_modification_events: true,
            can_generate_field_access_events: true,
            can_get_bytecodes: true,
            can_get_synthetic_attribute: true,
            can_get_owned_monitor_info: true,
            can_get_current_contended_monitor: true,
            can_get_monitor_info: true,
            can_pop_frame: true,
            can_redefine_classes: true,
            can_signal_thread: true,
            can_get_source_file_name: true,
            can_get_line_numbers: true,
            can_get_source_debug_extension: true,
            // obsaudit D2 (2026-07-26): `get_local_*`/`set_local_*` read and
            // write a side table, never a real interpreter/JIT frame — see
            // the GAP note above `set_local_variable_table` below.
            // `GetPotentialCapabilities` must not claim this works; a
            // well-behaved caller checks potential capabilities before
            // `AddCapabilities`, and `add_capabilities` below now enforces
            // it regardless.
            can_access_local_variables: false,
            can_maintain_original_method_order: true,
            can_generate_single_step_events: true,
            can_generate_exception_events: true,
            can_generate_frame_pop_events: true,
            can_generate_breakpoint_events: true,
            can_suspend: true,
            can_redefine_any_class: true,
            can_get_current_thread_cpu_time: true,
            can_get_thread_cpu_time: true,
            can_generate_method_entry_events: true,
            can_generate_method_exit_events: true,
            can_generate_all_class_hook_events: true,
            can_generate_compiled_method_load_events: true,
            can_generate_monitor_events: true,
            can_generate_vm_object_alloc_events: true,
            can_generate_native_method_bind_events: true,
            can_generate_garbage_collection_events: true,
            can_generate_object_free_events: true,
            can_force_early_return: true,
            can_get_owned_monitor_stack_depth_info: true,
            can_get_constant_pool: true,
            can_set_native_method_prefix: true,
            can_retransform_classes: true,
            can_retransform_any_class: true,
            can_generate_resource_exhaustion_heap_events: true,
            can_generate_resource_exhaustion_threads_events: true,
        }
    }

    /// Returns true if no capabilities are set.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

// ---------------------------------------------------------------------------
// Internal VM-model types (self-contained, no external deps)
// ---------------------------------------------------------------------------

/// Unique thread identifier within the JVMTI environment.
pub type ThreadId = u64;

/// Unique class identifier.
pub type ClassId = u64;

/// Unique method identifier.
pub type MethodId = u64;

/// Unique field identifier.
pub type FieldId = u64;

/// Thread state flags matching JVMTI spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadState(pub u32);

impl ThreadState {
    pub const ALIVE: u32 = 0x0001;
    pub const TERMINATED: u32 = 0x0002;
    pub const RUNNABLE: u32 = 0x0004;
    pub const BLOCKED_ON_MONITOR: u32 = 0x0400;
    pub const WAITING: u32 = 0x0080;
    pub const WAITING_INDEFINITELY: u32 = 0x0010;
    pub const WAITING_WITH_TIMEOUT: u32 = 0x0020;
    pub const SLEEPING: u32 = 0x0040;
    pub const SUSPENDED: u32 = 0x100000;
    pub const INTERRUPTED: u32 = 0x200000;
    pub const IN_NATIVE: u32 = 0x400000;
}

/// Information about a thread.
#[derive(Debug, Clone)]
pub struct ThreadInfo {
    pub name: String,
    pub priority: i32,
    pub is_daemon: bool,
    pub thread_group_name: String,
    pub state: ThreadState,
}

/// A single frame in a stack trace.
#[derive(Debug, Clone)]
pub struct FrameInfo {
    pub method_id: MethodId,
    pub class_id: ClassId,
    pub location: i64,
    pub method_name: String,
    pub class_name: String,
}

/// A value that can be stored in a local variable slot.
#[derive(Debug, Clone, PartialEq)]
pub enum LocalValue {
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Object(Option<u64>), // object reference or null
}

/// Information about a field.
#[derive(Debug, Clone)]
pub struct FieldInfo {
    pub field_id: FieldId,
    pub name: String,
    pub signature: String,
    pub modifiers: u32,
}

/// Information about a method.
#[derive(Debug, Clone)]
pub struct MethodInfo {
    pub method_id: MethodId,
    pub name: String,
    pub signature: String,
    pub modifiers: u32,
    pub declaring_class: ClassId,
}

/// Information about a class.
#[derive(Debug, Clone)]
pub struct ClassInfo {
    pub class_id: ClassId,
    pub name: String,
    pub bytecode: Vec<u8>,
    pub is_prepared: bool,
    pub fields: Vec<FieldInfo>,
    pub methods: Vec<MethodInfo>,
}

/// A breakpoint location.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BreakpointLocation {
    pub class_id: ClassId,
    pub method_id: MethodId,
    pub location: i64,
}

/// A field watch (for access or modification).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FieldWatch {
    pub class_id: ClassId,
    pub field_id: FieldId,
}

// ---------------------------------------------------------------------------
// Event Callbacks
// ---------------------------------------------------------------------------

/// Callback function types for each event kind. These mirror the JVMTI
/// jvmtiEventCallbacks structure, adapted to Rust closures.
pub struct EventCallbacks {
    pub vm_init: Option<Box<dyn Fn() + Send + Sync>>,
    pub vm_death: Option<Box<dyn Fn() + Send + Sync>>,
    pub thread_start: Option<Box<dyn Fn(ThreadId) + Send + Sync>>,
    pub thread_end: Option<Box<dyn Fn(ThreadId) + Send + Sync>>,
    pub class_file_load_hook:
        Option<Box<dyn Fn(ClassId, &str, &[u8]) -> Option<Vec<u8>> + Send + Sync>>,
    pub class_load: Option<Box<dyn Fn(ThreadId, ClassId) + Send + Sync>>,
    pub class_prepare: Option<Box<dyn Fn(ThreadId, ClassId) + Send + Sync>>,
    pub method_entry: Option<Box<dyn Fn(ThreadId, MethodId) + Send + Sync>>,
    pub method_exit: Option<Box<dyn Fn(ThreadId, MethodId, bool, LocalValue) + Send + Sync>>,
    pub exception: Option<Box<dyn Fn(ThreadId, MethodId, i64) + Send + Sync>>,
    pub exception_catch: Option<Box<dyn Fn(ThreadId, MethodId, i64) + Send + Sync>>,
    pub field_access: Option<Box<dyn Fn(ThreadId, MethodId, FieldId) + Send + Sync>>,
    pub field_modification: Option<Box<dyn Fn(ThreadId, MethodId, FieldId) + Send + Sync>>,
    pub breakpoint: Option<Box<dyn Fn(ThreadId, MethodId, i64) + Send + Sync>>,
    pub single_step: Option<Box<dyn Fn(ThreadId, MethodId, i64) + Send + Sync>>,
    pub frame_pop: Option<Box<dyn Fn(ThreadId, MethodId, bool) + Send + Sync>>,
    pub gc_start: Option<Box<dyn Fn() + Send + Sync>>,
    pub gc_finish: Option<Box<dyn Fn() + Send + Sync>>,
    pub monitor_contended_enter: Option<Box<dyn Fn(ThreadId, u64) + Send + Sync>>,
    pub monitor_contended_entered: Option<Box<dyn Fn(ThreadId, u64) + Send + Sync>>,
    pub monitor_wait: Option<Box<dyn Fn(ThreadId, u64, i64) + Send + Sync>>,
    pub monitor_waited: Option<Box<dyn Fn(ThreadId, u64, bool) + Send + Sync>>,
    pub compiled_method_load: Option<Box<dyn Fn(MethodId, usize) + Send + Sync>>,
    pub compiled_method_unload: Option<Box<dyn Fn(MethodId) + Send + Sync>>,
    pub dynamic_code_generated: Option<Box<dyn Fn(&str, usize) + Send + Sync>>,
    pub native_method_bind: Option<Box<dyn Fn(ThreadId, MethodId) + Send + Sync>>,
    /// ObjectFree(tag) — called one-shot per tagged object that was collected.
    pub object_free: Option<Box<dyn Fn(i64) + Send + Sync>>,
    /// VMObjectAlloc(thread, obj_addr, class, size_bytes).
    pub vm_object_alloc: Option<Box<dyn Fn(ThreadId, u64, ClassId, usize) + Send + Sync>>,
    /// SampledObjectAlloc(thread, obj_addr, class, size_bytes).
    pub sampled_object_alloc: Option<Box<dyn Fn(ThreadId, u64, ClassId, usize) + Send + Sync>>,
    /// DataDumpRequest — parameterless heap-dump trigger.
    pub data_dump_request: Option<Box<dyn Fn() + Send + Sync>>,
}

impl Default for EventCallbacks {
    fn default() -> Self {
        Self {
            vm_init: None,
            vm_death: None,
            thread_start: None,
            thread_end: None,
            class_file_load_hook: None,
            class_load: None,
            class_prepare: None,
            method_entry: None,
            method_exit: None,
            exception: None,
            exception_catch: None,
            field_access: None,
            field_modification: None,
            breakpoint: None,
            single_step: None,
            frame_pop: None,
            gc_start: None,
            gc_finish: None,
            monitor_contended_enter: None,
            monitor_contended_entered: None,
            monitor_wait: None,
            monitor_waited: None,
            compiled_method_load: None,
            compiled_method_unload: None,
            dynamic_code_generated: None,
            native_method_bind: None,
            object_free: None,
            vm_object_alloc: None,
            sampled_object_alloc: None,
            data_dump_request: None,
        }
    }
}

impl fmt::Debug for EventCallbacks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventCallbacks")
            .field("vm_init", &self.vm_init.is_some())
            .field("vm_death", &self.vm_death.is_some())
            .field("thread_start", &self.thread_start.is_some())
            .field("thread_end", &self.thread_end.is_some())
            .field("class_file_load_hook", &self.class_file_load_hook.is_some())
            .field("class_load", &self.class_load.is_some())
            .field("class_prepare", &self.class_prepare.is_some())
            .field("method_entry", &self.method_entry.is_some())
            .field("method_exit", &self.method_exit.is_some())
            .field("exception", &self.exception.is_some())
            .field("breakpoint", &self.breakpoint.is_some())
            .field("gc_start", &self.gc_start.is_some())
            .field("gc_finish", &self.gc_finish.is_some())
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Event Manager
// ---------------------------------------------------------------------------

/// Tracks which events are enabled globally and per-thread, and delivers
/// events to registered callbacks.
///
/// In addition to the manager's own callback table, extra `JvmtiEnv`s may be
/// attached via [`JvmtiEventManager::register_env`]. When a fire_ method runs
/// it dispatches to every attached env's callback table in turn, matching the
/// JVMTI spec semantics where multiple agents may subscribe to the same event.
///
/// **That contract is currently only partially implemented.** 11 of the 30
/// `fire_*` methods run the `snapshot_envs()` loop; the rest dispatch only to
/// this manager's own `callbacks` table, so an attached env silently receives
/// nothing for them. There is no diagnostic when this happens. Still missing
/// the loop, as of 2026-07-26:
///
/// ```text
/// vm_init          vm_death          thread_start      thread_end
/// class_file_load_hook               exception         exception_catch
/// breakpoint       gc_start          gc_finish         monitor_contended_enter
/// monitor_contended_entered          monitor_wait      monitor_waited
/// compiled_method_load               compiled_method_unload
/// dynamic_code_generated
/// ```
///
/// `class_load` / `class_prepare` were in that list and now dispatch per-env;
/// the others were left alone rather than swept, because two of them need a
/// semantic decision first, not a copied loop: `class_file_load_hook` returns
/// replacement bytes (with N agents, whose transform wins — first, last, or
/// chained?), and `breakpoint` / `exception` are the events where double
/// delivery to an agent that registered on both tables would be most visible.
///
/// Note also that the per-env loops do *not* consult the env's own enable
/// state — `is_event_enabled` is checked once against this manager, then every
/// attached env's callback is invoked. An env that never enabled the event
/// still gets it. That is the established behaviour of every existing loop, so
/// the two added here match it deliberately rather than inventing a second
/// semantics; it is worth revisiting if per-env enablement ever matters.
///
/// Scope check before relying on any of this: `register_env` has no callers
/// outside tests, and agents loaded via `-agentpath:` reach a *different*
/// manager entirely (`vm/src/jvmti/`, see the D14 note at the top of this
/// file). Per-env delivery here is therefore test-only reachable today.
///
/// A per-manager [`AtomicBool`] is used as the no-agent fast path: when no
/// environment has enabled any event and no callback is registered, the flag
/// is false and fire_ methods exit in a single atomic load. This keeps the
/// hot-path cost at O(1) (a single relaxed load + branch) when no tool is
/// attached, which is the overwhelming common case.
pub struct JvmtiEventManager {
    /// The `vm_identity` of the VM this manager belongs to, or
    /// [`UNATTRIBUTED_VM`] (`0`) for a manager that was installed through the
    /// legacy, VM-less [`install_global_manager`] entry point (or built
    /// standalone by a unit test).
    ///
    /// This is what makes the D14 bridge per-VM: a bridged `fire_*` resolves
    /// the real, native-agent-facing env through *this* field, so a manager
    /// owned by VM B can never deliver into VM A's `shared.debug.jvmti_env`.
    /// An unattributed manager falls back to [`sole_live_bridge`], which
    /// answers `None` unless exactly one VM is live — fail-closed, never a
    /// guess. See `jvmti-vm-scoping.md`.
    vm: usize,
    /// Global event enable/disable state.
    global_events: RwLock<HashSet<JvmtiEventKind>>,
    /// Per-thread event enable/disable state.
    thread_events: RwLock<HashMap<ThreadId, HashSet<JvmtiEventKind>>>,
    /// Registered callbacks for event delivery.
    callbacks: RwLock<EventCallbacks>,
    /// Count of events fired, for diagnostics.
    event_counts: Mutex<HashMap<JvmtiEventKind, u64>>,
    /// Attached JVMTI environments (weak refs so envs may be dropped).
    attached_envs: RwLock<Vec<Weak<JvmtiEnv>>>,
    /// Fast-path flag: true iff any event is enabled OR any env is attached OR
    /// any callback is registered. Checked first in every fire_ method so the
    /// no-agent case costs a single atomic load.
    any_listener: AtomicBool,
    // T17.Δ — per-event fast-path flags. These let hot-path interpreter
    // sites branch on a single `Acquire` load per dispatch without touching
    // the manager's global maps. Set on Enable of the corresponding kind,
    // cleared by `recompute_per_event_flags` when mode → Disable.
    /// True iff any listener is interested in `MethodEntry`.
    any_method_entry_listener: AtomicBool,
    /// True iff any listener is interested in `MethodExit`.
    any_method_exit_listener: AtomicBool,
    /// True iff any listener is interested in `SingleStep`.
    any_single_step_listener: AtomicBool,
    /// True iff any listener is interested in `FieldAccess`.
    any_field_access_listener: AtomicBool,
    /// True iff any listener is interested in `FieldModification`.
    any_field_modification_listener: AtomicBool,
    /// True iff any listener is interested in `FramePop`.
    any_frame_pop_listener: AtomicBool,
    /// Bytes-allocated since last SampledObjectAlloc fire. Used by the
    /// sampling sub-system to decide when to emit an event; reset by
    /// fire_sampled_object_alloc when the sample threshold is reached.
    sampling_bytes: AtomicU64,
    /// Threshold (in bytes) for SampledObjectAlloc events. Default 512 KB.
    sampling_threshold: AtomicU64,
}

/// Default sampling threshold for `SampledObjectAlloc`: 512 KB between fires.
/// Matches the HotSpot `HeapMonitor` / `-XX:SamplingInterval=524288` default.
pub const DEFAULT_SAMPLING_INTERVAL_BYTES: u64 = 512 * 1024;

impl JvmtiEventManager {
    /// A manager with no owning VM. Equivalent to `new_for_vm(UNATTRIBUTED_VM)`.
    ///
    /// Kept for the legacy [`install_global_manager`] call site and for unit
    /// tests. **New production wiring should use [`Self::new_for_vm`]** so the
    /// manager's events, listener flags and D14 bridge are scoped to one VM.
    pub fn new() -> Self {
        Self::new_for_vm(UNATTRIBUTED_VM)
    }

    /// A manager owned by the VM with `vm_identity == vm`.
    ///
    /// `vm` must be a real `vm_identity` (monotonic, allocated from
    /// `NEXT_VM_IDENTITY` in `vm/src/vm/vm_init.rs`, never recycled, never
    /// `0`). Passing `0` produces an unattributed manager.
    pub fn new_for_vm(vm: usize) -> Self {
        Self {
            vm,
            global_events: RwLock::new(HashSet::new()),
            thread_events: RwLock::new(HashMap::new()),
            callbacks: RwLock::new(EventCallbacks::default()),
            event_counts: Mutex::new(HashMap::new()),
            attached_envs: RwLock::new(Vec::new()),
            any_listener: AtomicBool::new(false),
            any_method_entry_listener: AtomicBool::new(false),
            any_method_exit_listener: AtomicBool::new(false),
            any_single_step_listener: AtomicBool::new(false),
            any_field_access_listener: AtomicBool::new(false),
            any_field_modification_listener: AtomicBool::new(false),
            any_frame_pop_listener: AtomicBool::new(false),
            sampling_bytes: AtomicU64::new(0),
            sampling_threshold: AtomicU64::new(DEFAULT_SAMPLING_INTERVAL_BYTES),
        }
    }

    /// The `vm_identity` this manager belongs to, or [`UNATTRIBUTED_VM`].
    #[inline]
    pub fn vm_identity(&self) -> usize {
        self.vm
    }

    /// The live `SharedVm` whose real, native-agent-facing JVMTI env this
    /// manager's bridged events belong to.
    ///
    /// * An attributed manager resolves **its own** VM's bridge, and answers
    ///   `None` once that VM is gone. It can never reach another VM's agent.
    /// * An unattributed manager (the legacy [`install_global_manager`] path,
    ///   which carries no VM) falls back to [`sole_live_bridge`]: the unique
    ///   live VM if there is exactly one, `None` if there are none or several.
    ///   Dropping is deliberate — delivering VM B's `ClassLoad`, carrying a
    ///   `ClassId` from VM B's class-id space, into VM A's agent is a
    ///   correctness bug in an interface debuggers treat as authoritative.
    fn bridged_shared(&self) -> Option<Arc<crate::vm::SharedVm>> {
        if self.vm == UNATTRIBUTED_VM {
            sole_live_bridge()
        } else {
            bridge_for_vm(self.vm)
        }
    }

    /// Recompute per-event fast-path flags from the current event-enable
    /// state. Called after Enable / Disable of a relevant event kind so the
    /// interpreter's per-opcode hot-path load sees the correct value.
    fn recompute_per_event_flag(&self, kind: JvmtiEventKind) {
        let global = self
            .global_events
            .read()
            .map(|g| g.contains(&kind))
            .unwrap_or(false);
        let per_thread = self
            .thread_events
            .read()
            .map(|t| t.values().any(|s| s.contains(&kind)))
            .unwrap_or(false);
        let enabled = global || per_thread;
        match kind {
            JvmtiEventKind::MethodEntry => self
                .any_method_entry_listener
                .store(enabled, Ordering::Release),
            JvmtiEventKind::MethodExit => self
                .any_method_exit_listener
                .store(enabled, Ordering::Release),
            JvmtiEventKind::SingleStep => self
                .any_single_step_listener
                .store(enabled, Ordering::Release),
            JvmtiEventKind::FieldAccess => self
                .any_field_access_listener
                .store(enabled, Ordering::Release),
            JvmtiEventKind::FieldModification => self
                .any_field_modification_listener
                .store(enabled, Ordering::Release),
            JvmtiEventKind::FramePop => self
                .any_frame_pop_listener
                .store(enabled, Ordering::Release),
            _ => {}
        }
        publish_union_listener_flags();
    }

    /// Fast-path query for `MethodEntry` listener. Single Acquire load.
    #[inline]
    pub fn has_method_entry_listener(&self) -> bool {
        self.any_method_entry_listener.load(Ordering::Acquire)
    }
    /// Fast-path query for `MethodExit` listener.
    #[inline]
    pub fn has_method_exit_listener(&self) -> bool {
        self.any_method_exit_listener.load(Ordering::Acquire)
    }
    /// Fast-path query for `SingleStep` listener.
    #[inline]
    pub fn has_single_step_listener(&self) -> bool {
        self.any_single_step_listener.load(Ordering::Acquire)
    }
    /// Fast-path query for `FieldAccess` listener.
    #[inline]
    pub fn has_field_access_listener(&self) -> bool {
        self.any_field_access_listener.load(Ordering::Acquire)
    }
    /// Fast-path query for `FieldModification` listener.
    #[inline]
    pub fn has_field_modification_listener(&self) -> bool {
        self.any_field_modification_listener.load(Ordering::Acquire)
    }
    /// Fast-path query for `FramePop` listener.
    #[inline]
    pub fn has_frame_pop_listener(&self) -> bool {
        self.any_frame_pop_listener.load(Ordering::Acquire)
    }

    /// Attach a JvmtiEnv so its callbacks are invoked when fire_ methods run.
    /// The env is held by Weak<JvmtiEnv> so dropping the env does not keep
    /// the manager from garbage-collecting it.
    pub fn register_env(&self, env: &Arc<JvmtiEnv>) -> JvmtiResult<()> {
        let mut list = self
            .attached_envs
            .write()
            .map_err(|_| JvmtiError::Internal)?;
        list.push(Arc::downgrade(env));
        self.any_listener.store(true, Ordering::Release);
        publish_union_listener_flags();
        Ok(())
    }

    /// Remove an attached JvmtiEnv. Used during agent unload.
    pub fn unregister_env(&self, env: &Arc<JvmtiEnv>) -> JvmtiResult<()> {
        let any_env_left: bool;
        {
            let mut list = self
                .attached_envs
                .write()
                .map_err(|_| JvmtiError::Internal)?;
            let target = Arc::as_ptr(env) as usize;
            list.retain(|w| {
                w.upgrade()
                    .map(|e| Arc::as_ptr(&e) as usize != target)
                    .unwrap_or(false)
            });
            any_env_left = list.iter().any(|w| w.strong_count() > 0);
        }
        // Lock released above — now safe to re-check global/thread event state.
        let have_global = self
            .global_events
            .read()
            .map(|g| !g.is_empty())
            .unwrap_or(false);
        let have_thread = self
            .thread_events
            .read()
            .map(|t| t.values().any(|s| !s.is_empty()))
            .unwrap_or(false);
        self.any_listener.store(
            any_env_left || have_global || have_thread,
            Ordering::Release,
        );
        publish_union_listener_flags();
        Ok(())
    }

    /// Recompute the `any_listener` flag from the current state.
    /// Called after env detach or event-mode disable operations.
    fn recompute_any_listener(&self) {
        let have_env = self
            .attached_envs
            .read()
            .map(|l| l.iter().any(|w| w.strong_count() > 0))
            .unwrap_or(false);
        let have_global = self
            .global_events
            .read()
            .map(|g| !g.is_empty())
            .unwrap_or(false);
        let have_thread = self
            .thread_events
            .read()
            .map(|t| t.values().any(|s| !s.is_empty()))
            .unwrap_or(false);
        self.any_listener
            .store(have_env || have_global || have_thread, Ordering::Release);
        publish_union_listener_flags();
    }

    /// Fast-path check: is any listener attached at all?
    /// Single atomic load — O(1) when no agent is subscribed.
    #[inline]
    pub fn has_any_listener(&self) -> bool {
        self.any_listener.load(Ordering::Acquire)
    }

    /// Configure the `SampledObjectAlloc` threshold (bytes between fires).
    /// Setting to 0 means "fire on every allocation" (debug only).
    pub fn set_sampling_interval(&self, bytes: u64) {
        self.sampling_threshold.store(bytes, Ordering::Relaxed);
    }

    /// Current `SampledObjectAlloc` threshold in bytes.
    pub fn sampling_interval(&self) -> u64 {
        self.sampling_threshold.load(Ordering::Relaxed)
    }

    /// Set event notification mode globally or for a specific thread.
    pub fn set_event_notification_mode(
        &self,
        mode: EventMode,
        event_kind: JvmtiEventKind,
        thread: Option<ThreadId>,
    ) -> JvmtiResult<()> {
        match thread {
            None => {
                let mut global = self
                    .global_events
                    .write()
                    .map_err(|_| JvmtiError::Internal)?;
                match mode {
                    EventMode::Enable => {
                        global.insert(event_kind);
                    }
                    EventMode::Disable => {
                        global.remove(&event_kind);
                    }
                }
                // Lock dropped at end of scope.
            }
            Some(tid) => {
                let mut per_thread = self
                    .thread_events
                    .write()
                    .map_err(|_| JvmtiError::Internal)?;
                let set = per_thread.entry(tid).or_default();
                match mode {
                    EventMode::Enable => {
                        set.insert(event_kind);
                    }
                    EventMode::Disable => {
                        set.remove(&event_kind);
                    }
                }
            }
        }
        // Fast-path flag must reflect the new state. Locks above have been
        // released; `recompute_any_listener` re-takes read locks safely.
        match mode {
            EventMode::Enable => self.any_listener.store(true, Ordering::Release),
            EventMode::Disable => self.recompute_any_listener(),
        }
        // T17.Δ — keep the per-event flag in sync regardless of mode so the
        // hot-path interpreter sites see the correct value on both enable
        // and disable transitions.
        self.recompute_per_event_flag(event_kind);
        Ok(())
    }

    /// Check if an event is enabled (globally or for the given thread).
    pub fn is_event_enabled(&self, event_kind: JvmtiEventKind, thread: Option<ThreadId>) -> bool {
        let global = match self.global_events.read() {
            Ok(g) => g,
            Err(_) => return false,
        };
        if global.contains(&event_kind) {
            return true;
        }
        if let Some(tid) = thread {
            let per_thread = match self.thread_events.read() {
                Ok(pt) => pt,
                Err(_) => return false,
            };
            if let Some(set) = per_thread.get(&tid) {
                return set.contains(&event_kind);
            }
        }
        false
    }

    /// Set the full callback table. Replaces all previous callbacks.
    pub fn set_event_callbacks(&self, cbs: EventCallbacks) -> JvmtiResult<()> {
        let mut current = self.callbacks.write().map_err(|_| JvmtiError::Internal)?;
        *current = cbs;
        drop(current);
        // Installing callbacks implies the caller wants events to be delivered
        // (even if mode is not yet Enabled — match existing tests that call
        // fire_* directly). Setting the flag is safe: fire_ methods still
        // gate on is_event_enabled before recording/dispatching.
        self.any_listener.store(true, Ordering::Release);
        // T17.Δ — refresh all per-event flags.  set_event_callbacks installs
        // callbacks but doesn't necessarily Enable a mode; however,  tests
        // that skip set_event_notification_mode and invoke fire_ directly
        // (see test_event_callbacks_fire) expect dispatch even so. Keep each
        // per-event flag in sync with the *actual* enabled state so the
        // interpreter fast path remains correct.
        for kind in JvmtiEventKind::ALL {
            self.recompute_per_event_flag(*kind);
        }
        Ok(())
    }

    /// Increment the event counter for the given kind.
    fn record_event(&self, kind: JvmtiEventKind) {
        if let Ok(mut counts) = self.event_counts.lock() {
            *counts.entry(kind).or_insert(0) += 1;
        }
    }

    /// Get the total number of events fired for a given kind.
    pub fn event_count(&self, kind: JvmtiEventKind) -> u64 {
        self.event_counts
            .lock()
            .map(|c| c.get(&kind).copied().unwrap_or(0))
            .unwrap_or(0)
    }

    // --- Event firing methods ---

    pub fn fire_vm_init(&self) {
        #[cfg(feature = "experimental-debug")]
        // obsaudit D14 bridge — see the notes above `install_real_agent_env_bridge`.
        if let Some(shared) = self.bridged_shared() {
            crate::jvmti::notify_vm_init(&shared.debug.jvmti_env.lock(), 0);
        }
        if !self.is_event_enabled(JvmtiEventKind::VmInit, None) {
            return;
        }
        self.record_event(JvmtiEventKind::VmInit);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.vm_init {
                cb();
            }
        }
    }

    pub fn fire_vm_death(&self) {
        #[cfg(feature = "experimental-debug")]
        // obsaudit D14 bridge — see the notes above `install_real_agent_env_bridge`.
        if let Some(shared) = self.bridged_shared() {
            crate::jvmti::notify_vm_death(&shared.debug.jvmti_env.lock());
        }
        if !self.is_event_enabled(JvmtiEventKind::VmDeath, None) {
            return;
        }
        self.record_event(JvmtiEventKind::VmDeath);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.vm_death {
                cb();
            }
        }
    }

    pub fn fire_thread_start(&self, thread: ThreadId) {
        #[cfg(feature = "experimental-debug")]
        // obsaudit D14 bridge — see the notes above `install_real_agent_env_bridge`.
        if let Some(shared) = self.bridged_shared() {
            let name = resolve_thread_name_for_bridge(&shared, thread);
            crate::jvmti::notify_thread_start(&shared.debug.jvmti_env.lock(), thread, &name);
        }
        if !self.is_event_enabled(JvmtiEventKind::ThreadStart, Some(thread)) {
            return;
        }
        self.record_event(JvmtiEventKind::ThreadStart);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.thread_start {
                cb(thread);
            }
        }
    }

    pub fn fire_thread_end(&self, thread: ThreadId) {
        #[cfg(feature = "experimental-debug")]
        // obsaudit D14 bridge — see the notes above `install_real_agent_env_bridge`.
        // Note: `vm/src/vm/vm_exec.rs` already has its own hand-written
        // `notify_thread_end` call site for the real env; this bridge makes
        // that call redundant whenever this method is *also* invoked for the
        // same thread exit, but harmless — ThreadEnd carries no per-call
        // state an agent couldn't tolerate seeing twice as cheaply as never.
        if let Some(shared) = self.bridged_shared() {
            crate::jvmti::notify_thread_end(&shared.debug.jvmti_env.lock(), thread);
        }
        if !self.is_event_enabled(JvmtiEventKind::ThreadEnd, Some(thread)) {
            return;
        }
        self.record_event(JvmtiEventKind::ThreadEnd);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.thread_end {
                cb(thread);
            }
        }
    }

    /// Fire ClassFileLoadHook. Returns transformed bytes if the callback
    /// provides a replacement, otherwise None.
    pub fn fire_class_file_load_hook(
        &self,
        class_id: ClassId,
        class_name: &str,
        bytecode: &[u8],
    ) -> Option<Vec<u8>> {
        if !self.is_event_enabled(JvmtiEventKind::ClassFileLoadHook, None) {
            return None;
        }
        self.record_event(JvmtiEventKind::ClassFileLoadHook);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.class_file_load_hook {
                return cb(class_id, class_name, bytecode);
            }
        }
        None
    }

    /// Fire ClassLoad.
    ///
    /// Reached from `ClassManager::define_class_shared_with_options` via
    /// `ClassManagerWriteGuard::drop` (`vm/src/vm/realms/class_realm.rs`) —
    /// see the DEFERRED FIRING notes above `install_class_load_hook` in
    /// `classloading/src/class_manager.rs`. As of obsaudit D1 (2026-07-26)
    /// this runs strictly *after* the L10 `ClassRealm::class_manager` write
    /// guard has been released, not while it is held: a listener may freely
    /// call back into the class manager (`GetClassSignature`,
    /// `GetLoadedClasses`, `RetransformClasses`, ...) without self-
    /// deadlocking. `catch_unwind` is kept regardless — an agent callback is
    /// untrusted code from the VM's point of view, and a panic in one must
    /// not unwind through interpreter/classloader frames it has no business
    /// touching.
    pub fn fire_class_load(&self, thread: ThreadId, class_id: ClassId) {
        // obsaudit D14 bridge — see the notes above `install_real_agent_env_bridge`.
        // Replaces the old hand-written call site in `vm/src/vm/vm_init.rs`
        // (a narrower helper that only covered one dynamic-load path), which
        // was removed so ClassLoad reaches the real env exactly once, from
        // every class-definition path, not just that one.
        #[cfg(feature = "experimental-debug")]
        if let Some(shared) = self.bridged_shared() {
            if let Some(name) = resolve_class_name_for_bridge(&shared, class_id) {
                crate::jvmti::notify_class_load(&shared.debug.jvmti_env.lock(), class_id, &name);
            }
        }
        if !self.is_event_enabled(JvmtiEventKind::ClassLoad, Some(thread)) {
            return;
        }
        self.record_event(JvmtiEventKind::ClassLoad);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.class_load {
                let _ =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(thread, class_id)));
            }
        }
        for env in self.snapshot_envs() {
            if let Ok(cbs) = env.event_manager.callbacks.read() {
                if let Some(ref cb) = cbs.class_load {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        cb(thread, class_id)
                    }));
                }
            }
        }
    }

    /// Fire ClassPrepare. Same timing and re-entrancy contract as
    /// [`Self::fire_class_load`] — see its doc comment.
    pub fn fire_class_prepare(&self, thread: ThreadId, class_id: ClassId) {
        // obsaudit D14 bridge — see the notes above `install_real_agent_env_bridge`.
        #[cfg(feature = "experimental-debug")]
        if let Some(shared) = self.bridged_shared() {
            if let Some(name) = resolve_class_name_for_bridge(&shared, class_id) {
                crate::jvmti::notify_class_prepare(&shared.debug.jvmti_env.lock(), class_id, &name);
            }
        }
        if !self.is_event_enabled(JvmtiEventKind::ClassPrepare, Some(thread)) {
            return;
        }
        self.record_event(JvmtiEventKind::ClassPrepare);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.class_prepare {
                let _ =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(thread, class_id)));
            }
        }
        for env in self.snapshot_envs() {
            if let Ok(cbs) = env.event_manager.callbacks.read() {
                if let Some(ref cb) = cbs.class_prepare {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        cb(thread, class_id)
                    }));
                }
            }
        }
    }

    pub fn fire_method_entry(&self, thread: ThreadId, method: MethodId) {
        if !self.is_event_enabled(JvmtiEventKind::MethodEntry, Some(thread)) {
            return;
        }
        self.record_event(JvmtiEventKind::MethodEntry);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.method_entry {
                // Panic-safe: agent callbacks may panic, don't bring down VM.
                let _ =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(thread, method)));
            }
        }
        for env in self.snapshot_envs() {
            if let Ok(cbs) = env.event_manager.callbacks.read() {
                if let Some(ref cb) = cbs.method_entry {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        cb(thread, method)
                    }));
                }
            }
        }
    }

    pub fn fire_method_exit(
        &self,
        thread: ThreadId,
        method: MethodId,
        was_popped_by_exception: bool,
        return_value: LocalValue,
    ) {
        if !self.is_event_enabled(JvmtiEventKind::MethodExit, Some(thread)) {
            return;
        }
        self.record_event(JvmtiEventKind::MethodExit);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.method_exit {
                let rv = return_value.clone();
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    cb(thread, method, was_popped_by_exception, rv)
                }));
            }
        }
        for env in self.snapshot_envs() {
            if let Ok(cbs) = env.event_manager.callbacks.read() {
                if let Some(ref cb) = cbs.method_exit {
                    let rv = return_value.clone();
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        cb(thread, method, was_popped_by_exception, rv)
                    }));
                }
            }
        }
    }

    pub fn fire_exception(&self, thread: ThreadId, method: MethodId, location: i64) {
        if !self.is_event_enabled(JvmtiEventKind::Exception, Some(thread)) {
            return;
        }
        self.record_event(JvmtiEventKind::Exception);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.exception {
                cb(thread, method, location);
            }
        }
    }

    pub fn fire_exception_catch(&self, thread: ThreadId, method: MethodId, location: i64) {
        if !self.is_event_enabled(JvmtiEventKind::ExceptionCatch, Some(thread)) {
            return;
        }
        self.record_event(JvmtiEventKind::ExceptionCatch);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.exception_catch {
                cb(thread, method, location);
            }
        }
    }

    pub fn fire_field_access(&self, thread: ThreadId, method: MethodId, field: FieldId) {
        if !self.is_event_enabled(JvmtiEventKind::FieldAccess, Some(thread)) {
            return;
        }
        self.record_event(JvmtiEventKind::FieldAccess);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.field_access {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    cb(thread, method, field)
                }));
            }
        }
        for env in self.snapshot_envs() {
            if let Ok(cbs) = env.event_manager.callbacks.read() {
                if let Some(ref cb) = cbs.field_access {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        cb(thread, method, field)
                    }));
                }
            }
        }
    }

    pub fn fire_field_modification(&self, thread: ThreadId, method: MethodId, field: FieldId) {
        if !self.is_event_enabled(JvmtiEventKind::FieldModification, Some(thread)) {
            return;
        }
        self.record_event(JvmtiEventKind::FieldModification);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.field_modification {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    cb(thread, method, field)
                }));
            }
        }
        for env in self.snapshot_envs() {
            if let Ok(cbs) = env.event_manager.callbacks.read() {
                if let Some(ref cb) = cbs.field_modification {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        cb(thread, method, field)
                    }));
                }
            }
        }
    }

    pub fn fire_breakpoint(&self, thread: ThreadId, method: MethodId, location: i64) {
        if !self.is_event_enabled(JvmtiEventKind::Breakpoint, Some(thread)) {
            return;
        }
        self.record_event(JvmtiEventKind::Breakpoint);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.breakpoint {
                cb(thread, method, location);
            }
        }
    }

    pub fn fire_single_step(&self, thread: ThreadId, method: MethodId, location: i64) {
        if !self.is_event_enabled(JvmtiEventKind::SingleStep, Some(thread)) {
            return;
        }
        self.record_event(JvmtiEventKind::SingleStep);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.single_step {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    cb(thread, method, location)
                }));
            }
        }
        for env in self.snapshot_envs() {
            if let Ok(cbs) = env.event_manager.callbacks.read() {
                if let Some(ref cb) = cbs.single_step {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        cb(thread, method, location)
                    }));
                }
            }
        }
    }

    pub fn fire_frame_pop(
        &self,
        thread: ThreadId,
        method: MethodId,
        was_popped_by_exception: bool,
    ) {
        if !self.is_event_enabled(JvmtiEventKind::FramePop, Some(thread)) {
            return;
        }
        self.record_event(JvmtiEventKind::FramePop);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.frame_pop {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    cb(thread, method, was_popped_by_exception)
                }));
            }
        }
        for env in self.snapshot_envs() {
            if let Ok(cbs) = env.event_manager.callbacks.read() {
                if let Some(ref cb) = cbs.frame_pop {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        cb(thread, method, was_popped_by_exception)
                    }));
                }
            }
        }
    }

    pub fn fire_gc_start(&self) {
        // obsaudit D14 bridge — see the notes above `install_real_agent_env_bridge`.
        #[cfg(feature = "experimental-debug")]
        if let Some(shared) = self.bridged_shared() {
            crate::jvmti::notify_gc_start(&shared.debug.jvmti_env.lock());
        }
        if !self.is_event_enabled(JvmtiEventKind::GarbageCollectionStart, None) {
            return;
        }
        self.record_event(JvmtiEventKind::GarbageCollectionStart);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.gc_start {
                cb();
            }
        }
    }

    pub fn fire_gc_finish(&self) {
        // obsaudit D14 bridge — see the notes above `install_real_agent_env_bridge`.
        #[cfg(feature = "experimental-debug")]
        if let Some(shared) = self.bridged_shared() {
            crate::jvmti::notify_gc_finish(&shared.debug.jvmti_env.lock());
        }
        if !self.is_event_enabled(JvmtiEventKind::GarbageCollectionFinish, None) {
            return;
        }
        self.record_event(JvmtiEventKind::GarbageCollectionFinish);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.gc_finish {
                cb();
            }
        }
    }

    pub fn fire_monitor_contended_enter(&self, thread: ThreadId, object: u64) {
        if !self.is_event_enabled(JvmtiEventKind::MonitorContendedEnter, Some(thread)) {
            return;
        }
        self.record_event(JvmtiEventKind::MonitorContendedEnter);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.monitor_contended_enter {
                cb(thread, object);
            }
        }
    }

    pub fn fire_monitor_contended_entered(&self, thread: ThreadId, object: u64) {
        if !self.is_event_enabled(JvmtiEventKind::MonitorContendedEntered, Some(thread)) {
            return;
        }
        self.record_event(JvmtiEventKind::MonitorContendedEntered);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.monitor_contended_entered {
                cb(thread, object);
            }
        }
    }

    pub fn fire_monitor_wait(&self, thread: ThreadId, object: u64, timeout: i64) {
        if !self.is_event_enabled(JvmtiEventKind::MonitorWait, Some(thread)) {
            return;
        }
        self.record_event(JvmtiEventKind::MonitorWait);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.monitor_wait {
                cb(thread, object, timeout);
            }
        }
    }

    pub fn fire_monitor_waited(&self, thread: ThreadId, object: u64, timed_out: bool) {
        if !self.is_event_enabled(JvmtiEventKind::MonitorWaited, Some(thread)) {
            return;
        }
        self.record_event(JvmtiEventKind::MonitorWaited);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.monitor_waited {
                cb(thread, object, timed_out);
            }
        }
    }

    pub fn fire_compiled_method_load(&self, method: MethodId, code_size: usize) {
        if !self.is_event_enabled(JvmtiEventKind::CompiledMethodLoad, None) {
            return;
        }
        self.record_event(JvmtiEventKind::CompiledMethodLoad);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.compiled_method_load {
                cb(method, code_size);
            }
        }
    }

    pub fn fire_compiled_method_unload(&self, method: MethodId) {
        if !self.is_event_enabled(JvmtiEventKind::CompiledMethodUnload, None) {
            return;
        }
        self.record_event(JvmtiEventKind::CompiledMethodUnload);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.compiled_method_unload {
                cb(method);
            }
        }
    }

    pub fn fire_dynamic_code_generated(&self, name: &str, code_size: usize) {
        if !self.is_event_enabled(JvmtiEventKind::DynamicCodeGenerated, None) {
            return;
        }
        self.record_event(JvmtiEventKind::DynamicCodeGenerated);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.dynamic_code_generated {
                cb(name, code_size);
            }
        }
    }

    pub fn fire_native_method_bind(&self, thread: ThreadId, method: MethodId) {
        if !self.is_event_enabled(JvmtiEventKind::NativeMethodBind, Some(thread)) {
            return;
        }
        self.record_event(JvmtiEventKind::NativeMethodBind);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.native_method_bind {
                cb(thread, method);
            }
        }
    }

    // ------------------------------------------------------------------
    // T6.3.1 — New event kinds wired to real safepoints
    // ------------------------------------------------------------------

    /// Snapshot the set of currently-attached envs, pruning dead weak refs.
    ///
    /// The returned vector holds strong [`Arc<JvmtiEnv>`] handles so the list
    /// remains valid for the duration of callback dispatch even if the
    /// manager's `attached_envs` list is concurrently mutated. Pruning of
    /// dropped envs is piggy-backed onto this call so the manager amortizes
    /// cleanup cost across fire_ invocations (no separate sweep thread).
    fn snapshot_envs(&self) -> Vec<Arc<JvmtiEnv>> {
        // Read-lock first: typical case is empty or unchanged list.
        if let Ok(list) = self.attached_envs.read() {
            let strong: Vec<Arc<JvmtiEnv>> = list.iter().filter_map(|w| w.upgrade()).collect();
            if strong.len() == list.len() {
                return strong;
            }
        }
        // Some entries died — take write lock and prune.
        if let Ok(mut list) = self.attached_envs.write() {
            list.retain(|w| w.strong_count() > 0);
            list.iter().filter_map(|w| w.upgrade()).collect()
        } else {
            Vec::new()
        }
    }

    /// Dispatch the ObjectFree callback registered on each attached env.
    /// Intentionally separate from the self-callbacks path so agent-specific
    /// tags are delivered via the env whose JVMTI instance issued SetTag.
    fn dispatch_object_free(&self, tag: i64) {
        for env in self.snapshot_envs() {
            if let Ok(cbs) = env.event_manager.callbacks.read() {
                if let Some(ref cb) = cbs.object_free {
                    cb(tag);
                }
            }
        }
    }

    /// Fire ObjectFree — called once per tagged object that was collected.
    ///
    /// `tag` is the user-defined tag installed via SetTag. A tag of 0 means
    /// "no tag" and should not normally reach this path; callers (the GC's
    /// tag-sweep step) are expected to filter those out.
    pub fn fire_object_free(&self, tag: i64) {
        // obsaudit D14 bridge — see the notes above `install_real_agent_env_bridge`.
        // Deliberately ahead of the `has_any_listener` fast-path below: that
        // flag tracks only this (synthetic) manager's own listeners, so a
        // real native agent with `can_tag_objects` and nothing registered
        // here would otherwise never see its own ObjectFree events.
        #[cfg(feature = "experimental-debug")]
        if let Some(shared) = self.bridged_shared() {
            crate::jvmti::notify_object_free(&shared.debug.jvmti_env.lock(), tag);
        }
        if !self.has_any_listener() {
            return;
        }
        if !self.is_event_enabled(JvmtiEventKind::ObjectFree, None) {
            return;
        }
        self.record_event(JvmtiEventKind::ObjectFree);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.object_free {
                cb(tag);
            }
        }
        self.dispatch_object_free(tag);
    }

    /// Fire VMObjectAlloc — called on every Java object allocation when the
    /// agent holds `can_generate_vm_object_alloc_events`. This is on the
    /// allocation hot path; callers MUST consult `has_any_listener` first
    /// (the method itself checks again but inlining the fast-path check at
    /// the call site lets the caller skip building the arguments too).
    pub fn fire_vm_object_alloc(
        &self,
        thread: ThreadId,
        object_addr: u64,
        class_id: ClassId,
        size: usize,
    ) {
        if !self.has_any_listener() {
            return;
        }
        if !self.is_event_enabled(JvmtiEventKind::VMObjectAlloc, Some(thread)) {
            return;
        }
        self.record_event(JvmtiEventKind::VMObjectAlloc);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.vm_object_alloc {
                cb(thread, object_addr, class_id, size);
            }
        }
        for env in self.snapshot_envs() {
            if let Ok(cbs) = env.event_manager.callbacks.read() {
                if let Some(ref cb) = cbs.vm_object_alloc {
                    cb(thread, object_addr, class_id, size);
                }
            }
        }
    }

    /// Record an allocation of `bytes` bytes and, if the running sum crosses
    /// the sampling threshold, fire a SampledObjectAlloc event and reset the
    /// counter. Returns true iff an event was fired.
    ///
    /// This is the intended entry point for the allocation fast path: it
    /// is a single relaxed fetch_add plus a branch when no agent has
    /// requested sampling, making it cheap enough to call per-allocation.
    pub fn record_allocation_sample(
        &self,
        thread: ThreadId,
        object_addr: u64,
        class_id: ClassId,
        size: usize,
    ) -> bool {
        // Hot-path: no listener → single atomic load + return.
        if !self.has_any_listener() {
            return false;
        }
        if !self.is_event_enabled(JvmtiEventKind::SampledObjectAlloc, Some(thread)) {
            return false;
        }
        let threshold = self.sampling_threshold.load(Ordering::Relaxed);
        let prev = self
            .sampling_bytes
            .fetch_add(size as u64, Ordering::Relaxed);
        if threshold == 0 || prev.wrapping_add(size as u64) < threshold {
            return false;
        }
        // Reached threshold — reset and fire.
        self.sampling_bytes.store(0, Ordering::Relaxed);
        self.fire_sampled_object_alloc(thread, object_addr, class_id, size);
        true
    }

    /// Fire SampledObjectAlloc directly, bypassing the rate-limiting counter.
    /// Normally callers should prefer [`record_allocation_sample`] which
    /// enforces the sampling interval.
    pub fn fire_sampled_object_alloc(
        &self,
        thread: ThreadId,
        object_addr: u64,
        class_id: ClassId,
        size: usize,
    ) {
        if !self.is_event_enabled(JvmtiEventKind::SampledObjectAlloc, Some(thread)) {
            return;
        }
        self.record_event(JvmtiEventKind::SampledObjectAlloc);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.sampled_object_alloc {
                cb(thread, object_addr, class_id, size);
            }
        }
        for env in self.snapshot_envs() {
            if let Ok(cbs) = env.event_manager.callbacks.read() {
                if let Some(ref cb) = cbs.sampled_object_alloc {
                    cb(thread, object_addr, class_id, size);
                }
            }
        }
    }

    /// Fire DataDumpRequest — called when a debugger asks for a heap dump.
    /// Parameterless by spec; the agent is expected to call into IterateOverHeap
    /// (or similar) from inside the callback to materialize the dump.
    pub fn fire_data_dump_request(&self) {
        if !self.has_any_listener() {
            return;
        }
        if !self.is_event_enabled(JvmtiEventKind::DataDumpRequest, None) {
            return;
        }
        self.record_event(JvmtiEventKind::DataDumpRequest);
        if let Ok(cbs) = self.callbacks.read() {
            if let Some(ref cb) = cbs.data_dump_request {
                cb();
            }
        }
        for env in self.snapshot_envs() {
            if let Ok(cbs) = env.event_manager.callbacks.read() {
                if let Some(ref cb) = cbs.data_dump_request {
                    cb();
                }
            }
        }
    }
}

impl Default for JvmtiEventManager {
    fn default() -> Self {
        Self::new()
    }
}

// obsaudit D13 (2026-07-26), fixed by removal: this file used to define its
// own `AgentRegistry`/`AgentEntry`/`split_agent_arg` "agent loading" type.
// It never actually loaded a native library (no `dlopen`, no `Agent_OnLoad`
// symbol lookup — every registered agent was unconditionally marked
// `loaded = true`), it was not the registry any bootstrap path used (that is
// `vm/src/jvmti/agent.rs`'s `AgentRegistry`, a *different* type with a real
// `libloading` implementation, driven by `SharedVm::new` via
// `load_startup_jvmti_agents`), and — per a repo-wide search — nothing
// outside its own unit tests ever constructed it. Two same-named,
// adjacent-module types with identical method names (`load_agents`,
// `agents()`, `loaded_count()`) is exactly the confusion the observability
// audit flagged: keeping a fake one around "for embedders" when no embedder
// anywhere in this tree used it just left the trap armed for the next
// person who greps for `AgentRegistry` and finds this one first. Removed
// rather than fixed in place; use `vm::jvmti::AgentRegistry` for anything
// agent-loading related.

// ---------------------------------------------------------------------------
// JVMTI Environment — the main function table
// ---------------------------------------------------------------------------

/// The JVMTI environment, exposing all major JVMTI functions.
/// This is the central struct that agents interact with.
pub struct JvmtiEnv {
    /// Current capabilities granted to this environment.
    capabilities: RwLock<JvmtiCapabilities>,
    /// Event management.
    pub event_manager: Arc<JvmtiEventManager>,
    /// Thread registry: maps thread IDs to their info.
    threads: RwLock<HashMap<ThreadId, ThreadInfo>>,
    /// Suspended threads set.
    suspended_threads: RwLock<HashSet<ThreadId>>,
    /// Stack traces per thread (most recent snapshot).
    stack_traces: RwLock<HashMap<ThreadId, Vec<FrameInfo>>>,
    /// Active breakpoints.
    breakpoints: RwLock<HashSet<BreakpointLocation>>,
    /// Active field access watches.
    field_access_watches: RwLock<HashSet<FieldWatch>>,
    /// Active field modification watches.
    field_modification_watches: RwLock<HashSet<FieldWatch>>,
    /// Class registry for introspection.
    classes: RwLock<HashMap<ClassId, ClassInfo>>,
    /// Local variable table per thread per frame depth.
    ///
    /// **This is a side table, not a view of real interpreter frames.** It is
    /// only ever populated by `set_frame_locals` / `set_local_*` on this same
    /// `JvmtiEnv`. Nothing in the interpreter, the JIT, or the stack walker
    /// writes into it. See [`JvmtiEnv::get_local_int`] for what that means for
    /// `GetLocalVariable*`.
    local_variables: RwLock<HashMap<(ThreadId, u32), HashMap<u32, LocalValue>>>,
    /// System properties.
    system_properties: RwLock<HashMap<String, String>>,
    /// Retransform bytecode transformer callback.
    retransform_hook: Mutex<Option<Box<dyn Fn(ClassId, &[u8]) -> Vec<u8> + Send + Sync>>>,
    /// GC trigger callback (delegates to VM's GC subsystem).
    gc_trigger: Mutex<Option<Box<dyn Fn() -> bool + Send + Sync>>>,
}

impl JvmtiEnv {
    /// Create a new JVMTI environment with default (empty) capabilities.
    pub fn new() -> Self {
        Self {
            capabilities: RwLock::new(JvmtiCapabilities::default()),
            event_manager: Arc::new(JvmtiEventManager::new()),
            threads: RwLock::new(HashMap::new()),
            suspended_threads: RwLock::new(HashSet::new()),
            stack_traces: RwLock::new(HashMap::new()),
            breakpoints: RwLock::new(HashSet::new()),
            field_access_watches: RwLock::new(HashSet::new()),
            field_modification_watches: RwLock::new(HashSet::new()),
            classes: RwLock::new(HashMap::new()),
            local_variables: RwLock::new(HashMap::new()),
            system_properties: RwLock::new(HashMap::new()),
            retransform_hook: Mutex::new(None),
            gc_trigger: Mutex::new(None),
        }
    }

    // --- Version & Error ---

    /// GetVersionNumber: return the JVMTI version.
    pub fn get_version_number(&self) -> u32 {
        JVMTI_VERSION_11
    }

    /// GetErrorName: return the standard name for an error code.
    pub fn get_error_name(&self, error: JvmtiError) -> &'static str {
        error.name()
    }

    // --- Event Notification ---

    /// SetEventNotificationMode: enable or disable an event globally or per-thread.
    pub fn set_event_notification_mode(
        &self,
        mode: EventMode,
        event_kind: JvmtiEventKind,
        thread: Option<ThreadId>,
    ) -> JvmtiResult<()> {
        self.event_manager
            .set_event_notification_mode(mode, event_kind, thread)
    }

    // --- Thread Functions ---

    /// Register a thread with the JVMTI environment (called by VM on thread creation).
    pub fn register_thread(&self, id: ThreadId, info: ThreadInfo) -> JvmtiResult<()> {
        let mut threads = self.threads.write().map_err(|_| JvmtiError::Internal)?;
        threads.insert(id, info);
        Ok(())
    }

    /// Unregister a thread (called by VM on thread death).
    pub fn unregister_thread(&self, id: ThreadId) -> JvmtiResult<()> {
        let mut threads = self.threads.write().map_err(|_| JvmtiError::Internal)?;
        threads.remove(&id);
        let mut suspended = self
            .suspended_threads
            .write()
            .map_err(|_| JvmtiError::Internal)?;
        suspended.remove(&id);
        let mut traces = self
            .stack_traces
            .write()
            .map_err(|_| JvmtiError::Internal)?;
        traces.remove(&id);
        Ok(())
    }

    /// GetAllThreads: return all live thread IDs.
    pub fn get_all_threads(&self) -> JvmtiResult<Vec<ThreadId>> {
        let threads = self.threads.read().map_err(|_| JvmtiError::Internal)?;
        Ok(threads.keys().copied().collect())
    }

    /// GetThreadInfo: return information about a thread.
    pub fn get_thread_info(&self, thread: ThreadId) -> JvmtiResult<ThreadInfo> {
        let threads = self.threads.read().map_err(|_| JvmtiError::Internal)?;
        threads
            .get(&thread)
            .cloned()
            .ok_or(JvmtiError::InvalidThread)
    }

    /// GetThreadState: return the state of a thread.
    pub fn get_thread_state(&self, thread: ThreadId) -> JvmtiResult<ThreadState> {
        let threads = self.threads.read().map_err(|_| JvmtiError::Internal)?;
        let info = threads.get(&thread).ok_or(JvmtiError::InvalidThread)?;
        let suspended = self
            .suspended_threads
            .read()
            .map_err(|_| JvmtiError::Internal)?;
        let mut state = info.state.0;
        if suspended.contains(&thread) {
            state |= ThreadState::SUSPENDED;
        }
        Ok(ThreadState(state))
    }

    /// SuspendThread: suspend a thread's execution.
    pub fn suspend_thread(&self, thread: ThreadId) -> JvmtiResult<()> {
        let caps = self.capabilities.read().map_err(|_| JvmtiError::Internal)?;
        if !caps.can_suspend {
            return Err(JvmtiError::MustPossessCapability);
        }
        let threads = self.threads.read().map_err(|_| JvmtiError::Internal)?;
        if !threads.contains_key(&thread) {
            return Err(JvmtiError::InvalidThread);
        }
        drop(threads);
        let mut suspended = self
            .suspended_threads
            .write()
            .map_err(|_| JvmtiError::Internal)?;
        if suspended.contains(&thread) {
            return Err(JvmtiError::ThreadSuspended);
        }
        suspended.insert(thread);
        Ok(())
    }

    /// ResumeThread: resume a suspended thread.
    pub fn resume_thread(&self, thread: ThreadId) -> JvmtiResult<()> {
        let caps = self.capabilities.read().map_err(|_| JvmtiError::Internal)?;
        if !caps.can_suspend {
            return Err(JvmtiError::MustPossessCapability);
        }
        let mut suspended = self
            .suspended_threads
            .write()
            .map_err(|_| JvmtiError::Internal)?;
        if !suspended.remove(&thread) {
            return Err(JvmtiError::ThreadNotSuspended);
        }
        Ok(())
    }

    // --- Stack Trace ---

    /// Update the stack trace for a thread (called by VM during execution).
    pub fn set_stack_trace(&self, thread: ThreadId, frames: Vec<FrameInfo>) -> JvmtiResult<()> {
        let mut traces = self
            .stack_traces
            .write()
            .map_err(|_| JvmtiError::Internal)?;
        traces.insert(thread, frames);
        Ok(())
    }

    /// GetStackTrace: return the stack trace for a thread, limited to max_count frames.
    pub fn get_stack_trace(
        &self,
        thread: ThreadId,
        start_depth: u32,
        max_count: u32,
    ) -> JvmtiResult<Vec<FrameInfo>> {
        let traces = self.stack_traces.read().map_err(|_| JvmtiError::Internal)?;
        let frames = traces.get(&thread).ok_or(JvmtiError::InvalidThread)?;
        let start = start_depth as usize;
        if start >= frames.len() && !frames.is_empty() {
            return Err(JvmtiError::NoMoreFrames);
        }
        let end = std::cmp::min(start + max_count as usize, frames.len());
        Ok(frames[start..end].to_vec())
    }

    /// GetFrameCount: return the number of frames on a thread's stack.
    pub fn get_frame_count(&self, thread: ThreadId) -> JvmtiResult<u32> {
        let traces = self.stack_traces.read().map_err(|_| JvmtiError::Internal)?;
        let frames = traces.get(&thread).ok_or(JvmtiError::InvalidThread)?;
        Ok(frames.len() as u32)
    }

    // --- GC ---

    /// Register a GC trigger callback. The VM provides this to connect JVMTI
    /// ForceGarbageCollection to the real GC subsystem.
    pub fn set_gc_trigger<F>(&self, trigger: F) -> JvmtiResult<()>
    where
        F: Fn() -> bool + Send + Sync + 'static,
    {
        let mut gc = self.gc_trigger.lock().map_err(|_| JvmtiError::Internal)?;
        *gc = Some(Box::new(trigger));
        Ok(())
    }

    /// ForceGarbageCollection: request a GC cycle.
    pub fn force_garbage_collection(&self) -> JvmtiResult<()> {
        let gc = self.gc_trigger.lock().map_err(|_| JvmtiError::Internal)?;
        match gc.as_ref() {
            Some(trigger) => {
                trigger();
                Ok(())
            }
            None => {
                // No GC subsystem connected; this is valid per spec (best-effort).
                Ok(())
            }
        }
    }

    // --- Breakpoints ---

    /// SetBreakpoint: set a breakpoint at the given location.
    pub fn set_breakpoint(&self, location: BreakpointLocation) -> JvmtiResult<()> {
        let caps = self.capabilities.read().map_err(|_| JvmtiError::Internal)?;
        if !caps.can_generate_breakpoint_events {
            return Err(JvmtiError::MustPossessCapability);
        }
        let mut bps = self.breakpoints.write().map_err(|_| JvmtiError::Internal)?;
        if !bps.insert(location) {
            return Err(JvmtiError::Duplicate);
        }
        Ok(())
    }

    /// ClearBreakpoint: remove a breakpoint at the given location.
    pub fn clear_breakpoint(&self, location: &BreakpointLocation) -> JvmtiResult<()> {
        let mut bps = self.breakpoints.write().map_err(|_| JvmtiError::Internal)?;
        if !bps.remove(location) {
            return Err(JvmtiError::NotFound);
        }
        Ok(())
    }

    /// Check if a breakpoint is set at the given location.
    pub fn has_breakpoint(&self, location: &BreakpointLocation) -> bool {
        self.breakpoints
            .read()
            .map(|bps| bps.contains(location))
            .unwrap_or(false)
    }

    // --- Field Watches ---

    /// SetFieldAccessWatch: register a watch on field access.
    pub fn set_field_access_watch(&self, watch: FieldWatch) -> JvmtiResult<()> {
        let caps = self.capabilities.read().map_err(|_| JvmtiError::Internal)?;
        if !caps.can_generate_field_access_events {
            return Err(JvmtiError::MustPossessCapability);
        }
        let mut watches = self
            .field_access_watches
            .write()
            .map_err(|_| JvmtiError::Internal)?;
        if !watches.insert(watch) {
            return Err(JvmtiError::Duplicate);
        }
        Ok(())
    }

    /// ClearFieldAccessWatch: remove a field access watch.
    pub fn clear_field_access_watch(&self, watch: &FieldWatch) -> JvmtiResult<()> {
        let mut watches = self
            .field_access_watches
            .write()
            .map_err(|_| JvmtiError::Internal)?;
        if !watches.remove(watch) {
            return Err(JvmtiError::NotFound);
        }
        Ok(())
    }

    /// SetFieldModificationWatch: register a watch on field modification.
    pub fn set_field_modification_watch(&self, watch: FieldWatch) -> JvmtiResult<()> {
        let caps = self.capabilities.read().map_err(|_| JvmtiError::Internal)?;
        if !caps.can_generate_field_modification_events {
            return Err(JvmtiError::MustPossessCapability);
        }
        let mut watches = self
            .field_modification_watches
            .write()
            .map_err(|_| JvmtiError::Internal)?;
        if !watches.insert(watch) {
            return Err(JvmtiError::Duplicate);
        }
        Ok(())
    }

    /// ClearFieldModificationWatch: remove a field modification watch.
    pub fn clear_field_modification_watch(&self, watch: &FieldWatch) -> JvmtiResult<()> {
        let mut watches = self
            .field_modification_watches
            .write()
            .map_err(|_| JvmtiError::Internal)?;
        if !watches.remove(watch) {
            return Err(JvmtiError::NotFound);
        }
        Ok(())
    }

    // --- Local Variables ---
    //
    // -----------------------------------------------------------------------
    // obsaudit D2 (2026-07-26): `GetLocalVariable*` DOES NOT READ REAL FRAMES
    // — PARTIALLY FIXED (capability negotiation is now honest; the
    // underlying data source is still a side table, by design).
    // -----------------------------------------------------------------------
    // `get_local_int` / `_long` / `_float` / `_double` / `_object` read the
    // `local_variables` side table, written by `set_local_variable_table` /
    // `set_local_*` on this same `JvmtiEnv`. Outside `#[cfg(test)]`, nothing
    // calls any of them — not the interpreter, not the JIT deopt path, not
    // the stack walker. Wiring this to real frames is NOT just a matter of
    // plumbing a frame reference in: JVMTI's API is typed per slot —
    // `GetLocalInt` on a slot that holds a reference must return
    // `JVMTI_ERROR_TYPE_MISMATCH`, not a reinterpreted pointer — so the
    // reader needs a per-slot *kind* (int/long/float/double/ref). The
    // verifier type maps in `classloading/src/type_maps.rs` are
    // **oop-vs-not only**: they can say "slot 3 holds a reference", which is
    // what the GC needs, but cannot distinguish an `int` slot from a `float`
    // slot or identify the second half of a `long`. Implementing
    // `GetLocalVariable*` faithfully therefore needs either the class
    // file's `LocalVariableTable` attribute (optional, absent from most
    // release builds) or a widened slot-kind map — both larger, separate
    // undertakings than this pass. That part of the gap remains open.
    //
    // What IS fixed: `can_access_local_variables` used to be advertised as
    // `true` in `potentially_available()` while granting it via
    // `add_capabilities` and then finding every frame empty via the side
    // table — the exact "VM lost my frames" failure mode described by the
    // original audit. `potentially_available()` now reports `false`, and
    // `add_capabilities` rejects a request for it with `NotAvailable`. A
    // caller that checks potential capabilities before requesting (the
    // JVMTI-spec-correct client behaviour) will not be misled.
    //
    // The capability check was removed from all ten `get_local_*`/
    // `set_local_*` methods below (it can never be satisfied through the
    // real API anymore) so the side table remains usable exactly as before
    // as an embedder/test surface — it was never real JVMTI local-variable
    // access, and gating it behind a capability that can no longer be
    // granted would have made it unusable even for that purpose.

    /// Set local variable values for a given thread and frame depth.
    pub fn set_local_variable_table(
        &self,
        thread: ThreadId,
        depth: u32,
        vars: HashMap<u32, LocalValue>,
    ) -> JvmtiResult<()> {
        let mut locals = self
            .local_variables
            .write()
            .map_err(|_| JvmtiError::Internal)?;
        locals.insert((thread, depth), vars);
        Ok(())
    }

    /// GetLocalVariableInt
    pub fn get_local_int(&self, thread: ThreadId, depth: u32, slot: u32) -> JvmtiResult<i32> {
        let locals = self
            .local_variables
            .read()
            .map_err(|_| JvmtiError::Internal)?;
        let frame = locals
            .get(&(thread, depth))
            .ok_or(JvmtiError::NoMoreFrames)?;
        match frame.get(&slot) {
            Some(LocalValue::Int(v)) => Ok(*v),
            Some(_) => Err(JvmtiError::TypeMismatch),
            None => Err(JvmtiError::InvalidSlot),
        }
    }

    /// GetLocalVariableLong
    pub fn get_local_long(&self, thread: ThreadId, depth: u32, slot: u32) -> JvmtiResult<i64> {
        let locals = self
            .local_variables
            .read()
            .map_err(|_| JvmtiError::Internal)?;
        let frame = locals
            .get(&(thread, depth))
            .ok_or(JvmtiError::NoMoreFrames)?;
        match frame.get(&slot) {
            Some(LocalValue::Long(v)) => Ok(*v),
            Some(_) => Err(JvmtiError::TypeMismatch),
            None => Err(JvmtiError::InvalidSlot),
        }
    }

    /// GetLocalVariableFloat
    pub fn get_local_float(&self, thread: ThreadId, depth: u32, slot: u32) -> JvmtiResult<f32> {
        let locals = self
            .local_variables
            .read()
            .map_err(|_| JvmtiError::Internal)?;
        let frame = locals
            .get(&(thread, depth))
            .ok_or(JvmtiError::NoMoreFrames)?;
        match frame.get(&slot) {
            Some(LocalValue::Float(v)) => Ok(*v),
            Some(_) => Err(JvmtiError::TypeMismatch),
            None => Err(JvmtiError::InvalidSlot),
        }
    }

    /// GetLocalVariableDouble
    pub fn get_local_double(&self, thread: ThreadId, depth: u32, slot: u32) -> JvmtiResult<f64> {
        let locals = self
            .local_variables
            .read()
            .map_err(|_| JvmtiError::Internal)?;
        let frame = locals
            .get(&(thread, depth))
            .ok_or(JvmtiError::NoMoreFrames)?;
        match frame.get(&slot) {
            Some(LocalValue::Double(v)) => Ok(*v),
            Some(_) => Err(JvmtiError::TypeMismatch),
            None => Err(JvmtiError::InvalidSlot),
        }
    }

    /// GetLocalVariableObject
    pub fn get_local_object(
        &self,
        thread: ThreadId,
        depth: u32,
        slot: u32,
    ) -> JvmtiResult<Option<u64>> {
        let locals = self
            .local_variables
            .read()
            .map_err(|_| JvmtiError::Internal)?;
        let frame = locals
            .get(&(thread, depth))
            .ok_or(JvmtiError::NoMoreFrames)?;
        match frame.get(&slot) {
            Some(LocalValue::Object(v)) => Ok(*v),
            Some(_) => Err(JvmtiError::TypeMismatch),
            None => Err(JvmtiError::InvalidSlot),
        }
    }

    /// SetLocalVariableInt
    pub fn set_local_int(
        &self,
        thread: ThreadId,
        depth: u32,
        slot: u32,
        value: i32,
    ) -> JvmtiResult<()> {
        let mut locals = self
            .local_variables
            .write()
            .map_err(|_| JvmtiError::Internal)?;
        let frame = locals.entry((thread, depth)).or_default();
        frame.insert(slot, LocalValue::Int(value));
        Ok(())
    }

    /// SetLocalVariableLong
    pub fn set_local_long(
        &self,
        thread: ThreadId,
        depth: u32,
        slot: u32,
        value: i64,
    ) -> JvmtiResult<()> {
        let mut locals = self
            .local_variables
            .write()
            .map_err(|_| JvmtiError::Internal)?;
        let frame = locals.entry((thread, depth)).or_default();
        frame.insert(slot, LocalValue::Long(value));
        Ok(())
    }

    /// SetLocalVariableFloat
    pub fn set_local_float(
        &self,
        thread: ThreadId,
        depth: u32,
        slot: u32,
        value: f32,
    ) -> JvmtiResult<()> {
        let mut locals = self
            .local_variables
            .write()
            .map_err(|_| JvmtiError::Internal)?;
        let frame = locals.entry((thread, depth)).or_default();
        frame.insert(slot, LocalValue::Float(value));
        Ok(())
    }

    /// SetLocalVariableDouble
    pub fn set_local_double(
        &self,
        thread: ThreadId,
        depth: u32,
        slot: u32,
        value: f64,
    ) -> JvmtiResult<()> {
        let mut locals = self
            .local_variables
            .write()
            .map_err(|_| JvmtiError::Internal)?;
        let frame = locals.entry((thread, depth)).or_default();
        frame.insert(slot, LocalValue::Double(value));
        Ok(())
    }

    /// SetLocalVariableObject
    pub fn set_local_object(
        &self,
        thread: ThreadId,
        depth: u32,
        slot: u32,
        value: Option<u64>,
    ) -> JvmtiResult<()> {
        let mut locals = self
            .local_variables
            .write()
            .map_err(|_| JvmtiError::Internal)?;
        let frame = locals.entry((thread, depth)).or_default();
        frame.insert(slot, LocalValue::Object(value));
        Ok(())
    }

    // --- Class Retransformation & Redefinition ---

    /// Register a bytecode transformer for RetransformClasses.
    pub fn set_retransform_hook<F>(&self, hook: F) -> JvmtiResult<()>
    where
        F: Fn(ClassId, &[u8]) -> Vec<u8> + Send + Sync + 'static,
    {
        let mut h = self
            .retransform_hook
            .lock()
            .map_err(|_| JvmtiError::Internal)?;
        *h = Some(Box::new(hook));
        Ok(())
    }

    /// RetransformClasses: apply the registered bytecode transformer to a class.
    pub fn retransform_classes(&self, class_ids: &[ClassId]) -> JvmtiResult<()> {
        let caps = self.capabilities.read().map_err(|_| JvmtiError::Internal)?;
        if !caps.can_retransform_classes {
            return Err(JvmtiError::MustPossessCapability);
        }
        drop(caps);

        let hook = self
            .retransform_hook
            .lock()
            .map_err(|_| JvmtiError::Internal)?;
        let transformer = hook.as_ref().ok_or(JvmtiError::NotAvailable)?;

        for &cid in class_ids {
            let classes = self.classes.write().map_err(|_| JvmtiError::Internal)?;
            let class = classes.get(&cid).ok_or(JvmtiError::InvalidClass)?;
            let original_bytes = class.bytecode.clone();
            let class_name = class.name.clone();
            drop(classes);

            let new_bytes = transformer(cid, &original_bytes);

            // Also fire ClassFileLoadHook if enabled
            let final_bytes = self
                .event_manager
                .fire_class_file_load_hook(cid, &class_name, &new_bytes)
                .unwrap_or(new_bytes);

            let mut classes2 = self.classes.write().map_err(|_| JvmtiError::Internal)?;
            if let Some(class) = classes2.get_mut(&cid) {
                class.bytecode = final_bytes;
            }
        }
        Ok(())
    }

    /// RedefineClasses: replace the bytecode of a class entirely.
    pub fn redefine_classes(&self, redefinitions: &[(ClassId, Vec<u8>)]) -> JvmtiResult<()> {
        let caps = self.capabilities.read().map_err(|_| JvmtiError::Internal)?;
        if !caps.can_redefine_classes {
            return Err(JvmtiError::MustPossessCapability);
        }
        drop(caps);

        let mut classes = self.classes.write().map_err(|_| JvmtiError::Internal)?;
        for (cid, new_bytes) in redefinitions {
            let class = classes.get_mut(cid).ok_or(JvmtiError::InvalidClass)?;
            if new_bytes.len() < 4 {
                return Err(JvmtiError::InvalidClassFormat);
            }
            // Basic magic number check for classfile (0xCAFEBABE)
            if new_bytes.len() >= 4
                && (new_bytes[0] != 0xCA
                    || new_bytes[1] != 0xFE
                    || new_bytes[2] != 0xBA
                    || new_bytes[3] != 0xBE)
            {
                return Err(JvmtiError::InvalidClassFormat);
            }
            class.bytecode = new_bytes.clone();
        }
        Ok(())
    }

    // --- Class Introspection ---

    /// Register a class with the JVMTI environment.
    pub fn register_class(&self, info: ClassInfo) -> JvmtiResult<()> {
        let mut classes = self.classes.write().map_err(|_| JvmtiError::Internal)?;
        classes.insert(info.class_id, info);
        Ok(())
    }

    /// GetClassFields: return field IDs for a class.
    pub fn get_class_fields(&self, class_id: ClassId) -> JvmtiResult<Vec<FieldId>> {
        let classes = self.classes.read().map_err(|_| JvmtiError::Internal)?;
        let class = classes.get(&class_id).ok_or(JvmtiError::InvalidClass)?;
        Ok(class.fields.iter().map(|f| f.field_id).collect())
    }

    /// GetClassMethods: return method IDs for a class.
    pub fn get_class_methods(&self, class_id: ClassId) -> JvmtiResult<Vec<MethodId>> {
        let classes = self.classes.read().map_err(|_| JvmtiError::Internal)?;
        let class = classes.get(&class_id).ok_or(JvmtiError::InvalidClass)?;
        Ok(class.methods.iter().map(|m| m.method_id).collect())
    }

    /// GetMethodName: return the name of a method.
    pub fn get_method_name(&self, method_id: MethodId) -> JvmtiResult<String> {
        let classes = self.classes.read().map_err(|_| JvmtiError::Internal)?;
        for class in classes.values() {
            if let Some(m) = class.methods.iter().find(|m| m.method_id == method_id) {
                return Ok(m.name.clone());
            }
        }
        Err(JvmtiError::InvalidMethodId)
    }

    /// GetFieldName: return the name of a field.
    pub fn get_field_name(&self, field_id: FieldId) -> JvmtiResult<String> {
        let classes = self.classes.read().map_err(|_| JvmtiError::Internal)?;
        for class in classes.values() {
            if let Some(f) = class.fields.iter().find(|f| f.field_id == field_id) {
                return Ok(f.name.clone());
            }
        }
        Err(JvmtiError::InvalidFieldId)
    }

    /// GetMethodDeclaringClass: return the class that declares a method.
    pub fn get_method_declaring_class(&self, method_id: MethodId) -> JvmtiResult<ClassId> {
        let classes = self.classes.read().map_err(|_| JvmtiError::Internal)?;
        for class in classes.values() {
            if class.methods.iter().any(|m| m.method_id == method_id) {
                return Ok(class.class_id);
            }
        }
        Err(JvmtiError::InvalidMethodId)
    }

    // --- Capabilities ---

    /// AddCapabilities: request additional capabilities.
    ///
    /// obsaudit D2 (2026-07-26): rejects `can_access_local_variables`
    /// explicitly — `potentially_available()` already reports it `false`;
    /// this is the enforcement half, so a caller cannot be granted a
    /// capability this env has already declared it cannot honor. No other
    /// field is checked against `potentially_available()` here: every other
    /// capability in that function is `true` today, so a generic per-field
    /// loop would be a no-op everywhere except this one case.
    pub fn add_capabilities(&self, requested: &JvmtiCapabilities) -> JvmtiResult<()> {
        if requested.can_access_local_variables {
            return Err(JvmtiError::NotAvailable);
        }
        let mut caps = self
            .capabilities
            .write()
            .map_err(|_| JvmtiError::Internal)?;
        *caps = caps.union(requested);
        Ok(())
    }

    /// RelinquishCapabilities: give up previously acquired capabilities.
    pub fn relinquish_capabilities(&self, to_relinquish: &JvmtiCapabilities) -> JvmtiResult<()> {
        let mut caps = self
            .capabilities
            .write()
            .map_err(|_| JvmtiError::Internal)?;
        *caps = caps.subtract(to_relinquish);
        Ok(())
    }

    /// GetCapabilities: return the currently held capabilities.
    pub fn get_capabilities(&self) -> JvmtiResult<JvmtiCapabilities> {
        let caps = self.capabilities.read().map_err(|_| JvmtiError::Internal)?;
        Ok(caps.clone())
    }

    /// Helper to check a capability is held.
    fn require_capability<F: Fn(&JvmtiCapabilities) -> bool>(&self, check: F) -> JvmtiResult<()> {
        let caps = self.capabilities.read().map_err(|_| JvmtiError::Internal)?;
        if check(&caps) {
            Ok(())
        } else {
            Err(JvmtiError::MustPossessCapability)
        }
    }

    // --- System Properties ---

    /// GetSystemProperty: retrieve a VM system property.
    pub fn get_system_property(&self, key: &str) -> JvmtiResult<String> {
        let props = self
            .system_properties
            .read()
            .map_err(|_| JvmtiError::Internal)?;
        props.get(key).cloned().ok_or(JvmtiError::NotFound)
    }

    /// SetSystemProperty: set a VM system property.
    pub fn set_system_property(&self, key: &str, value: &str) -> JvmtiResult<()> {
        let mut props = self
            .system_properties
            .write()
            .map_err(|_| JvmtiError::Internal)?;
        props.insert(key.to_string(), value.to_string());
        Ok(())
    }

    /// Bulk-load system properties (called during VM init).
    pub fn load_system_properties(&self, props: &[(String, String)]) -> JvmtiResult<()> {
        let mut sp = self
            .system_properties
            .write()
            .map_err(|_| JvmtiError::Internal)?;
        for (k, v) in props {
            sp.insert(k.clone(), v.clone());
        }
        Ok(())
    }
}

impl Default for JvmtiEnv {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for JvmtiEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JvmtiEnv")
            .field("version", &self.get_version_number())
            .field("capabilities", &self.capabilities)
            .field(
                "thread_count",
                &self.threads.read().map(|t| t.len()).unwrap_or(0),
            )
            .field(
                "breakpoint_count",
                &self.breakpoints.read().map(|b| b.len()).unwrap_or(0),
            )
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Per-VM JVMTI environment registry (C2 review remediation, 2026-08-01)
// ---------------------------------------------------------------------------
//
// This replaces the two process-global cells this module used to carry:
//
//   * `GLOBAL_MANAGER: OnceLock<Arc<JvmtiEventManager>>` — one event manager
//     for the whole process, holding every registered callback and every
//     `any_*_listener` fast-path flag. Event *delivery* was process-global,
//     so re-keying any one downstream table (the field-watchpoint map, say)
//     produced a subsystem that looked isolated in review and was not. See
//     `vm-process-global-state-round-2.md` § "Still open".
//   * `REAL_AGENT_ENV_BRIDGE: OnceLock<Weak<SharedVm>>` — a single `Weak`,
//     first-writer-wins. Exactly the shape that made `RedefineClasses`
//     silently do nothing in a second VM. Verified failure modes:
//       - two live VMs: VM B's bridged events were delivered into VM A's
//         `shared.debug.jvmti_env`, carrying VM B's `ClassId`s and thread
//         ids, which VM A's agent resolves against VM A's class manager;
//       - **sequential** VMs: after VM A is dropped, `OnceLock::set` from
//         VM B still fails and the stored `Weak` no longer upgrades, so
//         VM B's native agent received *zero* bridged events, permanently.
//         That breaks sequential embedding, not just concurrency.
//
// The registry below is keyed on `vm_identity` (`vm/src/vm/vm_init.rs`'s
// `NEXT_VM_IDENTITY` — monotonic, allocated from 1, never recycled, so a
// stale key can never be re-observed by a later VM). `0` is the reserved
// "unattributed" key, per the convention established by round 1.
//
// Hot-path contract, unchanged from before: `any_*_listener_active()` is a
// single `Acquire` `AtomicBool` load against a process-wide **union**
// mirror. A union is a deliberate over-approximation — VM A may pay the
// branch cost of VM B's agent — because the alternative (a map lookup under
// a lock per getfield/putfield/invoke) is not affordable. Over-approximating
// the *guard* is safe; over-approximating *delivery* is not, and delivery is
// always resolved against the exact VM below.

/// The reserved key for state that arrived without a VM identity.
///
/// `vm_identity` is allocated from `NEXT_VM_IDENTITY`, which starts at 1, so
/// `0` can never collide with a real VM.
pub const UNATTRIBUTED_VM: usize = 0;

/// One VM's JVMTI environment.
struct VmJvmtiEnvironment {
    /// The event manager this VM's listeners are registered on.
    ///
    /// `None` means this VM never installed one of its own — it was created
    /// by [`install_real_agent_env_bridge`] to hold a bridge. Such a VM
    /// resolves through the [`UNATTRIBUTED_VM`] row instead
    /// ([`manager_for_vm`]). Storing `None` rather than eagerly minting an
    /// empty manager is what keeps that fallback reachable: an empty manager
    /// present in the row would shadow the unattributed one and silently
    /// swallow every event.
    manager: Option<Arc<JvmtiEventManager>>,
    /// Bridge to this VM's real, native-agent-facing env
    /// (`shared.debug.jvmti_env`). `Weak`, so the registry never keeps a VM
    /// alive.
    ///
    /// `None` means *no bridge was ever installed* for this row (a bare
    /// `SharedVm` built by a unit test, or a row created by
    /// `install_manager_for_vm` before `Vm::new` runs). `Some(w)` with
    /// `w.strong_count() == 0` means the VM is **gone**. The two must not be
    /// conflated: pruning on `strong_count() == 0` alone would delete a live
    /// row that simply has no bridge yet.
    bridge: Option<Weak<crate::vm::SharedVm>>,
    /// This VM's field access/modification watchpoints, keyed on
    /// `(class_id, field_index)`. `class_id` is only unique *within* a VM,
    /// which is why this table cannot be process-global: a watchpoint set by
    /// an agent in VM A would otherwise fire on an unrelated field of an
    /// unrelated class in VM B.
    ///
    /// Holds no `ObjectRef` and no heap address — this is metadata only, so
    /// it needs no GC root source and no remap half.
    watchpoints: HashMap<(u64, usize), FieldWatchpoint>,
}

impl VmJvmtiEnvironment {
    fn empty() -> Self {
        Self {
            manager: None,
            bridge: None,
            watchpoints: HashMap::new(),
        }
    }

    /// `true` once this row's VM has definitively gone away: a bridge was
    /// installed and its `Weak` no longer upgrades.
    fn is_dead(&self) -> bool {
        matches!(&self.bridge, Some(w) if w.strong_count() == 0)
    }
}

/// The per-VM environments. `None` until the first VM registers, so the
/// static needs no lazy initialiser.
static ENVIRONMENTS: RwLock<Option<HashMap<usize, VmJvmtiEnvironment>>> = RwLock::new(None);

// Process-wide union mirrors of the per-VM fast-path flags. Each is `true`
// iff *some* registered VM has that listener. Conservative by construction:
// never false while a VM is listening, so no event can be missed; possibly
// true while the asking VM is not listening, which costs a predicted branch
// and a per-VM re-check inside the corresponding `fire_*`.
static UNION_ANY_LISTENER: AtomicBool = AtomicBool::new(false);
static UNION_METHOD_ENTRY: AtomicBool = AtomicBool::new(false);
static UNION_METHOD_EXIT: AtomicBool = AtomicBool::new(false);
static UNION_SINGLE_STEP: AtomicBool = AtomicBool::new(false);
static UNION_FIELD_ACCESS: AtomicBool = AtomicBool::new(false);
static UNION_FIELD_MODIFICATION: AtomicBool = AtomicBool::new(false);
static UNION_FRAME_POP: AtomicBool = AtomicBool::new(false);

fn environments_read(
) -> std::sync::RwLockReadGuard<'static, Option<HashMap<usize, VmJvmtiEnvironment>>> {
    // Poison recovery rather than propagation: a poisoned lock must not turn
    // the JVMTI plane into a permanent no-op (or a panic on the interpreter's
    // hot path). The data is a plain map; a panic mid-write leaves it
    // structurally intact.
    match ENVIRONMENTS.read() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

fn environments_write(
) -> std::sync::RwLockWriteGuard<'static, Option<HashMap<usize, VmJvmtiEnvironment>>> {
    match ENVIRONMENTS.write() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

/// Recompute every union mirror from the registered managers.
///
/// Called from each listener-state mutator on `JvmtiEventManager`. Those
/// transitions happen on agent attach/detach and `SetEventNotificationMode`,
/// never on an event, so the walk is off every hot path.
fn publish_union_listener_flags() {
    let (mut any, mut me, mut mx, mut ss, mut fa, mut fm, mut fp) =
        (false, false, false, false, false, false, false);
    {
        let guard = environments_read();
        if let Some(map) = guard.as_ref() {
            for m in map.values().filter_map(|e| e.manager.as_ref()) {
                any |= m.has_any_listener();
                me |= m.has_method_entry_listener();
                mx |= m.has_method_exit_listener();
                ss |= m.has_single_step_listener();
                fa |= m.has_field_access_listener();
                fm |= m.has_field_modification_listener();
                fp |= m.has_frame_pop_listener();
            }
        }
    }
    UNION_ANY_LISTENER.store(any, Ordering::Release);
    UNION_METHOD_ENTRY.store(me, Ordering::Release);
    UNION_METHOD_EXIT.store(mx, Ordering::Release);
    UNION_SINGLE_STEP.store(ss, Ordering::Release);
    UNION_FIELD_ACCESS.store(fa, Ordering::Release);
    UNION_FIELD_MODIFICATION.store(fm, Ordering::Release);
    UNION_FRAME_POP.store(fp, Ordering::Release);
}

/// Install `vm`'s event manager. Idempotent per VM — the first install for a
/// given `vm_identity` wins, matching the old process-wide behaviour but one
/// scope down.
///
/// **This is the entry point production wiring uses.** `SharedVm::new` calls
/// it with `vm.vm_identity`, so every live VM owns a row and the
/// [`UNATTRIBUTED_VM`] fallback is reached only by hooks that genuinely have
/// no VM in scope — see `docs/feature-designs/jvmti-delivery-threading.md` for
/// the current census. `SharedVm::new` *also* still installs the row-0
/// manager, and that is not redundant: the six remaining VM-less sites
/// resolve row 0 through `global_manager()`, an exact lookup with no
/// fallback.
pub fn install_manager_for_vm(vm: usize, mgr: Arc<JvmtiEventManager>) {
    // On the "already installed" path `mgr` is dropped. Do it outside the
    // lock: dropping a manager runs agent-supplied callback destructors.
    let rejected;
    {
        let mut guard = environments_write();
        let map = guard.get_or_insert_with(HashMap::new);
        let entry = map.entry(vm).or_insert_with(VmJvmtiEnvironment::empty);
        // Idempotent: the first install for a VM wins, and a bridge or
        // watchpoints registered before the manager are preserved.
        if entry.manager.is_none() {
            entry.manager = Some(mgr);
            rejected = None;
        } else {
            rejected = Some(mgr);
        }
    }
    drop(rejected);
    publish_union_listener_flags();
}

/// Install the JVMTI event manager for callers that have no VM identity.
///
/// Retained because `SharedVm::new` (`vm/src/vm/vm_init.rs`) and the
/// `classloading` / `gc` hook adapters it installs are `fn` pointers with no
/// VM in scope. Registers under [`UNATTRIBUTED_VM`], which every
/// `*_for_vm` lookup falls back to, so a VM that never installs its own
/// manager still sees listeners registered this way — that fallback is what
/// keeps the migration safe in either order.
pub fn install_global_manager(mgr: Arc<JvmtiEventManager>) {
    install_manager_for_vm(UNATTRIBUTED_VM, mgr);
}

/// The unattributed manager, if one was installed. Prefer
/// [`manager_for_vm`].
pub fn global_manager() -> Option<Arc<JvmtiEventManager>> {
    manager_for_vm_exact(UNATTRIBUTED_VM)
}

/// `vm`'s manager if it has one, else the unattributed manager.
///
/// The fallback is the migration seam: an event site that has already been
/// converted to pass `vm_identity` keeps reaching listeners registered
/// through [`install_global_manager`] until `SharedVm::new` is converted too.
/// Once every VM installs its own manager the unattributed row is never
/// created and the fallback is inert.
pub fn manager_for_vm(vm: usize) -> Option<Arc<JvmtiEventManager>> {
    let guard = environments_read();
    let map = guard.as_ref()?;
    if let Some(m) = map.get(&vm).and_then(|e| e.manager.as_ref()) {
        return Some(Arc::clone(m));
    }
    map.get(&UNATTRIBUTED_VM)
        .and_then(|e| e.manager.as_ref())
        .map(Arc::clone)
}

/// `vm`'s manager, with no fallback to the unattributed row.
fn manager_for_vm_exact(vm: usize) -> Option<Arc<JvmtiEventManager>> {
    let guard = environments_read();
    guard
        .as_ref()?
        .get(&vm)
        .and_then(|e| e.manager.as_ref())
        .map(Arc::clone)
}

/// `true` iff **some** registered VM has a listener. A conservative
/// over-approximation for VM-less hot-path guards; see
/// [`any_listener_active_for_vm`] for the exact answer.
#[inline]
pub fn any_listener_active() -> bool {
    UNION_ANY_LISTENER.load(Ordering::Acquire)
}

/// `true` iff **this** VM has a listener.
pub fn any_listener_active_for_vm(vm: usize) -> bool {
    manager_for_vm(vm).is_some_and(|m| m.has_any_listener())
}

/// Drop every scrap of `vm`'s JVMTI state: its event manager (and with it
/// every callback closure the agent registered), its bridge to the real env,
/// and its field watchpoints.
///
/// Must be called when a VM is released. Without it a disposed VM's manager
/// keeps its agent's callback closures alive for the life of the process, its
/// listener flags keep every *other* VM's interpreter on the slow path, and
/// its watchpoints keep firing for a `class_id` that now means something else.
///
/// Idempotent — removing an absent row is a no-op — because the teardown hook
/// it belongs in (`release_vm_native_state`) is deliberately invoked from two
/// places, either of which may run first or alone.
pub fn forget_vm_jvmti_state(vm: usize) {
    // Rows are moved out under the lock and dropped after it is released.
    // Dropping a row drops its manager, and with it every agent-supplied
    // callback closure — arbitrary code that must not run while this module's
    // registry lock is held. This hook is reached from `Drop for SharedVm`.
    let mut released: Vec<VmJvmtiEnvironment> = Vec::new();
    {
        let mut guard = environments_write();
        if let Some(map) = guard.as_mut() {
            if let Some(row) = map.remove(&vm) {
                released.push(row);
            }
            // Opportunistically prune rows whose VM is provably gone but was
            // never released explicitly (a VM torn down before this hook
            // existed, or one leaked by a test). Keeping them would make
            // `sole_live_bridge` answer `None` forever after the first VM,
            // which is the exact sequential-embedding bug this change fixes.
            // `is_dead` requires a bridge that was installed and has since
            // expired, so a live bridge-less row is never touched.
            let dead: Vec<usize> = map
                .iter()
                .filter(|(k, env)| **k != UNATTRIBUTED_VM && env.is_dead())
                .map(|(k, _)| *k)
                .collect();
            for k in dead {
                if let Some(row) = map.remove(&k) {
                    released.push(row);
                }
            }
        }
    }
    drop(released);
    publish_union_listener_flags();
    refresh_watchpoint_union();
}

/// Number of registered environments. Diagnostics and tests only.
pub fn registered_environment_count() -> usize {
    environments_read().as_ref().map_or(0, |m| m.len())
}

// ---------------------------------------------------------------------------
// obsaudit D14 (2026-07-26) — bridge to the real, native-agent-facing JVMTI
// env (`vm/src/jvmti/`, `shared.debug.jvmti_env`)
// ---------------------------------------------------------------------------
//
// This file's `JvmtiEventManager` and `vm/src/jvmti/`'s `EventManager` are
// separate objects (see the LIVENESS block at the top of this file) — a
// native agent loaded via `-agentpath:` (real `dlopen`, `vm/src/jvmti/agent.rs`)
// only ever sees the latter. Before this bridge, of the ~26 event kinds this
// file's `fire_*` methods drive from the interpreter/GC/classloading, the
// real env received exactly two (`ClassLoad`, `ThreadEnd`), each via its own
// hand-written call site elsewhere in `vm/` — not through this manager at
// all. The bridge below forwards the 9 event kinds that (a) have a
// `notify_*` counterpart in `vm/src/jvmti/mod.rs` and (b) are not a
// per-bytecode/per-invocation hot path, so a real agent attached today
// actually receives VMInit, VMDeath, ThreadStart, ThreadEnd, ClassLoad,
// ClassPrepare, GarbageCollectionStart/Finish, and ObjectFree.
//
// Deliberately NOT bridged: MethodEntry/MethodExit/SingleStep/Breakpoint/
// FramePop/FieldAccess/FieldModification (per-bytecode or per-invocation —
// would add a `Mutex<JvmtiEnv>` lock to the interpreter's hottest paths for
// every VM, whether or not a native agent is attached to them) and
// MonitorWait/MonitorContendedEnter (per contended lock — same hot-path
// concern; also `vm/src/jvmti/mod.rs` has no `notify_monitor_waited` /
// `notify_monitor_contended_entered`, so `can_generate_monitor_events`
// could only ever be half-honest here). `JvmtiCapabilities::potential()`
// in `vm/src/jvmti/capabilities.rs` was corrected to advertise `false` for
// every capability whose events are not bridged, so a real agent's
// `AddCapabilities` negotiation reflects what it will actually receive
// instead of silently promising events that never arrive — the same
// failure shape D2 (`GetLocalVariable*`) documents for a different
// capability. Full unification of the two implementations (shared event
// enum, shared capability set, one `JvmtiEnv`) remains a separate, larger
// task — this bridge closes the "silently receives nothing" gap without
// attempting that rewrite.

/// Install the bridge to `shared`'s real, native-agent-facing JVMTI env.
///
/// Keyed on `shared.vm_identity`, so it is idempotent **per VM** and every VM
/// gets its own bridge. The previous shape — one `OnceLock<Weak<SharedVm>>`
/// for the process — had two verified failure modes, both fixed here:
///
/// * with two live VMs, `OnceLock::set` from the second VM failed silently
///   and VM B's bridged events were delivered into VM A's
///   `shared.debug.jvmti_env`;
/// * **sequentially**, after VM A was dropped the stored `Weak` no longer
///   upgraded and `set` from VM B still failed, so VM B's native agent
///   received nothing at all, for the life of the process.
///
/// Called from `Vm::new` (`vm/src/vm/vm_init.rs`), which is the only place
/// with both the `Arc<SharedVm>` and its identity in hand.
pub fn install_real_agent_env_bridge(shared: &Arc<crate::vm::SharedVm>) {
    let vm = shared.vm_identity;
    let mut released: Vec<VmJvmtiEnvironment> = Vec::new();
    {
        let mut guard = environments_write();
        let map = guard.get_or_insert_with(HashMap::new);
        let entry = map.entry(vm).or_insert_with(VmJvmtiEnvironment::empty);
        entry.bridge = Some(Arc::downgrade(shared));
        // A new VM registering means any row left behind by a previous,
        // now-dead VM is stale; drop those so `sole_live_bridge` can resolve
        // again. Rows are moved out and dropped after the lock is released —
        // dropping one runs an agent's callback destructors.
        let dead: Vec<usize> = map
            .iter()
            .filter(|(k, env)| **k != UNATTRIBUTED_VM && **k != vm && env.is_dead())
            .map(|(k, _)| *k)
            .collect();
        for k in dead {
            if let Some(row) = map.remove(&k) {
                released.push(row);
            }
        }
    }
    drop(released);
    publish_union_listener_flags();
}

/// `vm`'s live `SharedVm`, if it registered a bridge and has not been torn
/// down. Never falls back to another VM.
fn bridge_for_vm(vm: usize) -> Option<Arc<crate::vm::SharedVm>> {
    let guard = environments_read();
    guard.as_ref()?.get(&vm)?.bridge.as_ref()?.upgrade()
}

/// The unique live bridged VM, or `None` if there are zero or more than one.
///
/// This is what an *unattributed* manager (one installed through the VM-less
/// [`install_global_manager`]) uses to find the real env. With the single VM
/// that every non-embedding run has, it is exact. With two live VMs it
/// answers `None` and the bridged event is dropped rather than delivered to a
/// guess: an event attributed to the wrong VM's agent carries `ClassId`s and
/// thread ids from the wrong id space, which is worse than no event at all in
/// an interface a debugger treats as authoritative.
///
/// The fix that removes the ambiguity is at the call site, not here — see
/// `jvmti-vm-scoping.md`.
fn sole_live_bridge() -> Option<Arc<crate::vm::SharedVm>> {
    let guard = environments_read();
    let map = guard.as_ref()?;
    let mut found: Option<Arc<crate::vm::SharedVm>> = None;
    for env in map.values() {
        if let Some(shared) = env.bridge.as_ref().and_then(|w| w.upgrade()) {
            if found.is_some() {
                return None;
            }
            found = Some(shared);
        }
    }
    found
}

/// Resolve a loaded class's name for `vm/src/jvmti/mod.rs`'s
/// `notify_class_load` / `notify_class_prepare`, which (unlike this file's
/// `fire_class_load` / `fire_class_prepare`) take the name directly rather
/// than expecting the listener to look it up.
///
/// Safe to call from inside a bridged `fire_*` method even though it takes
/// the L10 `class_manager` read lock: as of obsaudit D1 (2026-07-26),
/// `fire_class_load`/`fire_class_prepare` only run after the write guard
/// that produced them has already been released, so a fresh `.read()` here
/// cannot self-deadlock.
fn resolve_class_name_for_bridge(
    shared: &crate::vm::SharedVm,
    class_id: ClassId,
) -> Option<String> {
    let cid = crate::classloading::ClassId::new(class_id as u32);
    shared
        .classes
        .class_manager
        .read()
        .class_store
        .get(cid)
        .map(|c| c.name.to_string())
}

/// Resolve a thread's name for `vm/src/jvmti/mod.rs`'s `notify_thread_start`
/// (unlike this file's `fire_thread_start`, it takes the name directly).
/// Falls back to a synthetic name rather than skipping the event: an agent
/// still needs to see the thread came into existence even if the registry
/// entry raced with this lookup.
fn resolve_thread_name_for_bridge(shared: &crate::vm::SharedVm, thread: ThreadId) -> String {
    shared
        .threads
        .thread_registry
        .thread_name(crate::threading::jvm_thread::ThreadId(thread))
        .unwrap_or_else(|| format!("Thread-{thread}"))
}

/// Fire VMInit at the global level. No-op if no manager is installed.
pub fn fire_vm_init() {
    if let Some(m) = global_manager() {
        m.fire_vm_init();
    }
}

/// Fire VMDeath at the global level. Called from VM shutdown / drop.
pub fn fire_vm_death() {
    if let Some(m) = global_manager() {
        m.fire_vm_death();
    }
}

/// Fire VMInit into `vm`'s environment only.
///
/// Provided so `SharedVm::new`'s `fire_vm_init()` (`vm/src/vm/vm_init.rs`) can
/// become a one-line change: `vm.vm_identity` is in scope there, and VMInit is
/// per VM by definition — it is the event that tells an agent *its* VM is up.
/// Unlike the class-load / GC hook adapters, this site has no signature
/// problem; it is simply still on the VM-less call. See
/// `docs/feature-designs/jvmti-delivery-threading.md`.
pub fn fire_vm_init_for_vm(vm: usize) {
    if let Some(m) = manager_for_vm(vm) {
        m.fire_vm_init();
    }
}

/// Fire VMDeath into `vm`'s environment only. Sibling of
/// [`fire_vm_init_for_vm`]; `Vm`'s shutdown path has `self.shared.vm_identity`
/// in scope. Must run **before** [`forget_vm_jvmti_state`] drops the row, or
/// it resolves through the unattributed seam and the agent that asked for
/// VMDeath never learns its VM died.
pub fn fire_vm_death_for_vm(vm: usize) {
    if let Some(m) = manager_for_vm(vm) {
        m.fire_vm_death();
    }
}

/// Fire ClassLoad at the global level. Called from the class manager
/// after a new class has been registered.
///
/// obsaudit D1 (2026-07-26), fixed: this used to be reached while the L10
/// `class_manager` write guard was still held (a real agent's `ClassLoad`
/// handler calling `GetClassSignature`/`GetLoadedClasses`/`RetransformClasses`
/// would self-deadlock) and always reported thread 0. Both are fixed — see
/// the DEFERRED FIRING notes near `install_class_load_hook` in
/// `classloading/src/class_manager.rs` and the doc comment on
/// `JvmtiEventManager::fire_class_load`. Also now bridged to the real,
/// native-agent-facing env — see `install_real_agent_env_bridge` (D14).
pub fn fire_class_load(thread: ThreadId, class_id: ClassId) {
    if let Some(m) = global_manager() {
        m.fire_class_load(thread, class_id);
    }
}

/// Fire ClassPrepare at the global level. Called from the class manager
/// after the class has been linked / prepared. See [`fire_class_load`].
pub fn fire_class_prepare(thread: ThreadId, class_id: ClassId) {
    if let Some(m) = global_manager() {
        m.fire_class_prepare(thread, class_id);
    }
}

/// Fire GarbageCollectionStart at the global level. Called from the GC
/// driver immediately before the collection phase.
pub fn fire_gc_start() {
    if let Some(m) = global_manager() {
        m.fire_gc_start();
    }
}

/// Fire GarbageCollectionFinish at the global level.
pub fn fire_gc_finish() {
    if let Some(m) = global_manager() {
        m.fire_gc_finish();
    }
}

/// Fire Exception at the global level.
pub fn fire_exception(thread: ThreadId, method: MethodId, location: i64) {
    if let Some(m) = global_manager() {
        m.fire_exception(thread, method, location);
    }
}

/// Fire ExceptionCatch at the global level. Called from the interpreter
/// when a matching exception handler is resolved.
pub fn fire_exception_catch(thread: ThreadId, method: MethodId, location: i64) {
    if let Some(m) = global_manager() {
        m.fire_exception_catch(thread, method, location);
    }
}

// ---------------------------------------------------------------------------
// T17.Δ — interpreter-side event free functions
// ---------------------------------------------------------------------------
//
// The interpreter dispatch loop and opcode handlers call these at the sites
// listed in roadmap-100.md §T17.Δ. The hot-path contract for each of
// these is:
//
//   1. A single `AtomicBool::Acquire` load on the per-event **union** mirror
//      (`UNION_*`), which is `true` iff some registered VM is listening.
//   2. If the flag is set the caller enters the full manager.fire_* path
//      which resolves the owning VM's manager, takes the event's map
//      read-locks, records the count, and dispatches to callbacks under
//      `catch_unwind`.
//
// When no JVMTI agent is attached anywhere in the process the flag is false
// and each call bottoms out in one load + one predicted branch after
// inlining — cheaper than the `OnceLock::get()` + per-manager load it
// replaced.
//
// The union is deliberate. With two VMs it can put VM A's interpreter on the
// slow path because VM B has an agent; that costs a branch and a re-check.
// It never *delivers* VM B's event to VM A — `fire_*_for_vm` resolves the
// exact VM, and the VM-less `fire_*` resolves the unattributed manager.
// Over-approximating a guard is safe; over-approximating delivery is not.

/// Fast-path query: any listener anywhere interested in `MethodEntry`?
#[inline]
pub fn any_method_entry_listener_active() -> bool {
    UNION_METHOD_ENTRY.load(Ordering::Acquire)
}

/// Fast-path query: any listener anywhere interested in `MethodExit`?
#[inline]
pub fn any_method_exit_listener_active() -> bool {
    UNION_METHOD_EXIT.load(Ordering::Acquire)
}

/// Fast-path query: any listener anywhere interested in `SingleStep`?
#[inline]
pub fn any_single_step_listener_active() -> bool {
    UNION_SINGLE_STEP.load(Ordering::Acquire)
}

/// Fast-path query: any listener anywhere interested in `FieldAccess`?
#[inline]
pub fn any_field_access_listener_active() -> bool {
    UNION_FIELD_ACCESS.load(Ordering::Acquire)
}

/// Fast-path query: any listener anywhere interested in `FieldModification`?
#[inline]
pub fn any_field_modification_listener_active() -> bool {
    UNION_FIELD_MODIFICATION.load(Ordering::Acquire)
}

/// Fast-path query: any listener anywhere interested in `FramePop`?
#[inline]
pub fn any_frame_pop_listener_active() -> bool {
    UNION_FRAME_POP.load(Ordering::Acquire)
}

/// Exact per-VM query: is **this** VM listening for `MethodEntry`?
pub fn any_method_entry_listener_active_for_vm(vm: usize) -> bool {
    manager_for_vm(vm).is_some_and(|m| m.has_method_entry_listener())
}

/// Exact per-VM query: is **this** VM listening for `MethodExit`?
pub fn any_method_exit_listener_active_for_vm(vm: usize) -> bool {
    manager_for_vm(vm).is_some_and(|m| m.has_method_exit_listener())
}

/// Exact per-VM query: is **this** VM listening for `SingleStep`?
pub fn any_single_step_listener_active_for_vm(vm: usize) -> bool {
    manager_for_vm(vm).is_some_and(|m| m.has_single_step_listener())
}

/// Exact per-VM query: is **this** VM listening for `FramePop`?
pub fn any_frame_pop_listener_active_for_vm(vm: usize) -> bool {
    manager_for_vm(vm).is_some_and(|m| m.has_frame_pop_listener())
}

/// Fire MethodEntry into `vm`'s environment only.
#[inline]
pub fn fire_method_entry_for_vm(vm: usize, thread: ThreadId, method: MethodId) {
    if let Some(m) = manager_for_vm(vm) {
        m.fire_method_entry(thread, method);
    }
}

/// Fire MethodExit into `vm`'s environment only.
#[inline]
pub fn fire_method_exit_for_vm(
    vm: usize,
    thread: ThreadId,
    method: MethodId,
    was_popped_by_exception: bool,
    return_value: LocalValue,
) {
    if let Some(m) = manager_for_vm(vm) {
        m.fire_method_exit(thread, method, was_popped_by_exception, return_value);
    }
}

/// Fire SingleStep into `vm`'s environment only.
#[inline]
pub fn fire_single_step_for_vm(vm: usize, thread: ThreadId, method: MethodId, location: i64) {
    if let Some(m) = manager_for_vm(vm) {
        m.fire_single_step(thread, method, location);
    }
}

/// Fire FramePop into `vm`'s environment only.
#[inline]
pub fn fire_frame_pop_for_vm(
    vm: usize,
    thread: ThreadId,
    method: MethodId,
    was_popped_by_exception: bool,
) {
    if let Some(m) = manager_for_vm(vm) {
        m.fire_frame_pop(thread, method, was_popped_by_exception);
    }
}

/// Fire ExceptionCatch into `vm`'s environment only.
#[inline]
pub fn fire_exception_catch_for_vm(vm: usize, thread: ThreadId, method: MethodId, location: i64) {
    if let Some(m) = manager_for_vm(vm) {
        m.fire_exception_catch(thread, method, location);
    }
}

/// Fire MethodEntry at the global level. Called from every frame-push in the
/// interpreter. Gate on [`any_method_entry_listener_active`] at the caller
/// so arg computation (method id synthesis) is skipped on the common no-agent
/// path.
#[inline]
pub fn fire_method_entry(thread: ThreadId, method: MethodId) {
    if let Some(m) = global_manager() {
        m.fire_method_entry(thread, method);
    }
}

/// Fire MethodExit at the global level. Called from every return-opcode and
/// from the exception-unwind path when a frame pops. Gate on
/// [`any_method_exit_listener_active`] at the caller.
#[inline]
pub fn fire_method_exit(
    thread: ThreadId,
    method: MethodId,
    was_popped_by_exception: bool,
    return_value: LocalValue,
) {
    if let Some(m) = global_manager() {
        m.fire_method_exit(thread, method, was_popped_by_exception, return_value);
    }
}

/// Fire SingleStep at the global level. Called from the top of the bytecode
/// dispatch loop when the thread's single-step flag is set.
#[inline]
pub fn fire_single_step(thread: ThreadId, method: MethodId, location: i64) {
    if let Some(m) = global_manager() {
        m.fire_single_step(thread, method, location);
    }
}

/// Fire FieldAccess at the global level. Called from getfield / getstatic
/// when a watchpoint exists for the resolved (class_id, field_index) tuple.
#[inline]
pub fn fire_field_access(thread: ThreadId, method: MethodId, field: FieldId) {
    if let Some(m) = global_manager() {
        m.fire_field_access(thread, method, field);
    }
}

/// Fire FieldModification at the global level. Called from putfield /
/// putstatic when a watchpoint exists.
#[inline]
pub fn fire_field_modification(thread: ThreadId, method: MethodId, field: FieldId) {
    if let Some(m) = global_manager() {
        m.fire_field_modification(thread, method, field);
    }
}

/// Fire FramePop at the global level. Called before a frame is dropped when
/// that frame's depth has a registered `NotifyFramePop` request.
#[inline]
pub fn fire_frame_pop(thread: ThreadId, method: MethodId, was_popped_by_exception: bool) {
    if let Some(m) = global_manager() {
        m.fire_frame_pop(thread, method, was_popped_by_exception);
    }
}

// ---------------------------------------------------------------------------
// T17.Δ.4 — Field access / modification watchpoint registry
// ---------------------------------------------------------------------------
//
// A JVMTI agent calls `SetFieldAccessWatch(class, field_id)` /
// `SetFieldModificationWatch` to register interest in reads/writes of a
// specific field. The interpreter consults the registry from `getfield` /
// `getstatic` / `putfield` / `putstatic` handlers before the field access
// and fires the matching event when a registered watchpoint matches the
// resolved `(class_id, field_index)` tuple.
//
// The registry is keyed by a tuple of the declaring class id and the field
// index (matching the interpreter's `ResolvedField`). Lookup is a single
// HashMap read; on the zero-watchpoint common case the read returns None
// and the interpreter skips the rest of the path.
//
// C2 review remediation (2026-08-01): the table now lives inside the per-VM
// environment (`VmJvmtiEnvironment::watchpoints`), not in a process-global
// static. `class_id` is only unique *within* a VM, so a process-wide table
// meant a watchpoint set by an agent in VM A fired on an unrelated field of
// an unrelated class in VM B. Round 1 of the process-global-state sweep
// recommended re-keying this map on `(vm_identity, class_id, field_index)`;
// round 2 corrected that to "do not fix in isolation", because delivery
// (`GLOBAL_MANAGER`) was process-global too and a re-keyed map alone would
// have produced a subsystem that looked isolated in review and was not.
// Both halves land together here.

/// A single field watchpoint. A field may be watched for access only,
/// modification only, or both; booleans disambiguate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldWatchpoint {
    /// Declaring class id (same encoding as `ResolvedField.declaring_class_id`).
    pub class_id: u64,
    /// Field index within the declaring class's field list.
    pub field_index: usize,
    /// If true, FieldAccess events fire on read.
    pub access_watched: bool,
    /// If true, FieldModification events fire on write.
    pub modification_watched: bool,
}

/// Lock-free mirror of "some registered VM has at least one watchpoint",
/// recomputed from `ENVIRONMENTS` after every register/clear.
/// [`any_field_watchpoint_active`] reads THIS instead of taking the lock — it
/// is polled per getfield/getstatic/putfield/putstatic (the most common
/// opcodes in OO bytecode), so a per-access `RwLock::read` was pure overhead
/// in the overwhelmingly-common no-JVMTI-agent case.
///
/// Like the `UNION_*` listener mirrors this is a process-wide
/// over-approximation: VM A pays a predicted branch for VM B's watchpoint.
/// The `(class_id, field_index)` match that follows is per-VM, so VM A can
/// never *fire* on VM B's watchpoint.
static FIELD_WATCHPOINTS_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Recompute [`FIELD_WATCHPOINTS_ACTIVE`] across every registered VM.
fn refresh_watchpoint_union() {
    let active = environments_read()
        .as_ref()
        .is_some_and(|m| m.values().any(|e| !e.watchpoints.is_empty()));
    FIELD_WATCHPOINTS_ACTIVE.store(active, Ordering::Release);
}

/// Register a field access / modification watchpoint in `vm`'s environment.
///
/// `access` and `modification` are additive: calling with
/// `(true, false)` then `(false, true)` on the same field enables both.
pub fn set_field_watchpoint_for_vm(
    vm: usize,
    class_id: u64,
    field_index: usize,
    access: bool,
    modification: bool,
) -> JvmtiResult<()> {
    {
        let mut guard = environments_write();
        let map = guard.get_or_insert_with(HashMap::new);
        let env = map.entry(vm).or_insert_with(VmJvmtiEnvironment::empty);
        let entry = env
            .watchpoints
            .entry((class_id, field_index))
            .or_insert(FieldWatchpoint {
                class_id,
                field_index,
                access_watched: false,
                modification_watched: false,
            });
        entry.access_watched = entry.access_watched || access;
        entry.modification_watched = entry.modification_watched || modification;
    }
    refresh_watchpoint_union();
    Ok(())
}

/// Register a watchpoint with no VM identity. Lands in the
/// [`UNATTRIBUTED_VM`] row, which every VM's lookup falls back to.
pub fn set_field_watchpoint(
    class_id: u64,
    field_index: usize,
    access: bool,
    modification: bool,
) -> JvmtiResult<()> {
    set_field_watchpoint_for_vm(UNATTRIBUTED_VM, class_id, field_index, access, modification)
}

/// Clear a field watchpoint's access / modification flags in `vm`'s
/// environment. If both become false the entry is removed from the map.
/// Returns Ok even if the watchpoint wasn't previously registered.
pub fn clear_field_watchpoint_for_vm(
    vm: usize,
    class_id: u64,
    field_index: usize,
    access: bool,
    modification: bool,
) -> JvmtiResult<()> {
    {
        let mut guard = environments_write();
        if let Some(env) = guard.as_mut().and_then(|m| m.get_mut(&vm)) {
            let key = (class_id, field_index);
            let now_empty = match env.watchpoints.get_mut(&key) {
                Some(entry) => {
                    if access {
                        entry.access_watched = false;
                    }
                    if modification {
                        entry.modification_watched = false;
                    }
                    !entry.access_watched && !entry.modification_watched
                }
                None => false,
            };
            if now_empty {
                env.watchpoints.remove(&key);
            }
        }
    }
    refresh_watchpoint_union();
    Ok(())
}

/// Clear a watchpoint with no VM identity — see [`set_field_watchpoint`].
pub fn clear_field_watchpoint(
    class_id: u64,
    field_index: usize,
    access: bool,
    modification: bool,
) -> JvmtiResult<()> {
    clear_field_watchpoint_for_vm(UNATTRIBUTED_VM, class_id, field_index, access, modification)
}

/// Look up `vm`'s watchpoint for `(class_id, field_index)`, falling back to
/// the [`UNATTRIBUTED_VM`] row. Returns `None` when no watchpoint matches —
/// the common interpreter hot-path result.
///
/// The `field_index` is a zero-based index into the declaring class's own
/// field list.  Callers that receive an arbitrary caller-supplied index
/// should validate it via [`field_watchpoint_is_valid`] first.
///
/// The fallback exists only for the migration window in which watchpoints are
/// still registered without a VM (see [`set_field_watchpoint`]). Once
/// `SetFieldAccessWatch` passes a `vm_identity`, the unattributed row is never
/// populated and the fallback is inert.
#[inline]
pub fn field_watchpoint_for_vm(
    vm: usize,
    class_id: u64,
    field_index: usize,
) -> Option<FieldWatchpoint> {
    let guard = environments_read();
    let map = guard.as_ref()?;
    let key = (class_id, field_index);
    if let Some(wp) = map.get(&vm).and_then(|e| e.watchpoints.get(&key)) {
        return Some(*wp);
    }
    if vm == UNATTRIBUTED_VM {
        return None;
    }
    map.get(&UNATTRIBUTED_VM)
        .and_then(|e| e.watchpoints.get(&key))
        .copied()
}

/// Look up a watchpoint with no VM identity — consults only the
/// [`UNATTRIBUTED_VM`] row, never another VM's.
#[inline]
pub fn field_watchpoint_for(class_id: u64, field_index: usize) -> Option<FieldWatchpoint> {
    field_watchpoint_for_vm(UNATTRIBUTED_VM, class_id, field_index)
}

/// True iff **some** registered VM has at least one field watchpoint.
///
/// Interpreter callers branch on this before doing a watchpoint lookup so
/// the zero-agent common case is a single atomic load. Conservative across
/// VMs by design; the lookup that follows is per-VM.
#[inline]
pub fn any_field_watchpoint_active() -> bool {
    // Lock-free: read the atomic mirror maintained by set/clear_field_watchpoint.
    //
    // `Acquire` is kept, and it was tried the other way. 2026-09-05 measured
    // the phase bracketing this load at 20.6 corrected cycles — 48% of the
    // real work in a quickened `getfield` — and the suspicion was that
    // `Acquire` is a compiler barrier on the FIRST statement of
    // `field_fast::getfield_fast_keyed`, fencing the whole arm behind itself.
    //
    // `Relaxed` is SOUND here: this flag publishes no data, and every consumer
    // that acts on `true` then calls `field_watchpoint_for_vm`, which takes
    // `environments_read()` — that lock is what synchronises-with the writer's
    // `environments_write()` release and publishes the watchpoint set. Nor
    // would relaxing delay an agent: `Acquire` on a LOAD orders what follows
    // it, it does not make the value fresher.
    //
    // It measured NOTHING. Three runs relaxed against the acquire build:
    // entry 20.3 / 23.2 / 22.7 against 20.6 corrected, with every other phase
    // and every phase SHARE identical to three significant figures. The effect
    // is below ~3 cycles, which is this instrument's resolution on that phase.
    //
    // So the barrier is not the cost, and the ordering is left alone: relaxing
    // a JVMTI mechanism's memory ordering buys nothing measurable, and an
    // unmeasurable change to correctness-adjacent code is not worth carrying.
    // What `entry` actually spends 20 cycles on is unresolved — the prologue
    // is excluded by construction (the first timestamp is taken after it) and
    // the `stack.len()` beside the load is a struct field read.
    FIELD_WATCHPOINTS_ACTIVE.load(Ordering::Acquire)
}

/// True iff **this** VM has at least one field watchpoint (including any it
/// inherits from the unattributed row).
pub fn any_field_watchpoint_active_for_vm(vm: usize) -> bool {
    let guard = environments_read();
    let Some(map) = guard.as_ref() else {
        return false;
    };
    let own = map.get(&vm).is_some_and(|e| !e.watchpoints.is_empty());
    if own || vm == UNATTRIBUTED_VM {
        return own;
    }
    map.get(&UNATTRIBUTED_VM)
        .is_some_and(|e| !e.watchpoints.is_empty())
}

/// Validate that `field_index` is within bounds for the class identified by
/// `class_id`.  Used as a guard when registering a watchpoint from a JVMTI
/// agent call so malformed input returns `JVMTI_ERROR_INVALID_FIELDID`
/// instead of silently succeeding.
///
/// `class_field_count` is the number of declared fields in the class; it is
/// the caller's responsibility to fetch this from the class manager.
pub fn field_watchpoint_is_valid(field_index: usize, class_field_count: usize) -> bool {
    field_index < class_field_count
}

/// Fire FieldAccess only if a watchpoint exists for (class_id, field_index)
/// AND the watchpoint has `access_watched` set. Combines the lookup, fast
/// path, and fire into one call so the interpreter dispatch site stays
/// compact.
#[inline]
pub fn fire_field_access_if_watched(
    thread: ThreadId,
    method: MethodId,
    class_id: u64,
    field_index: usize,
) {
    if !any_field_watchpoint_active() {
        return;
    }
    if let Some(wp) = field_watchpoint_for(class_id, field_index) {
        if wp.access_watched {
            let field_id = encode_field_id(class_id, field_index);
            fire_field_access(thread, method, field_id);
        }
    }
}

/// Fire FieldModification only if a watchpoint exists AND has
/// `modification_watched` set.
#[inline]
pub fn fire_field_modification_if_watched(
    thread: ThreadId,
    method: MethodId,
    class_id: u64,
    field_index: usize,
) {
    if !any_field_watchpoint_active() {
        return;
    }
    if let Some(wp) = field_watchpoint_for(class_id, field_index) {
        if wp.modification_watched {
            let field_id = encode_field_id(class_id, field_index);
            fire_field_modification(thread, method, field_id);
        }
    }
}

/// VM-scoped [`fire_field_access_if_watched`]: the watchpoint lookup and the
/// event delivery both resolve against `vm`.
///
/// This is the form the interpreter should call — `shared.vm_identity` is in
/// scope at all four `getfield`/`getstatic`/`putfield`/`putstatic` sites. The
/// process-wide `any_field_watchpoint_active()` gate is kept as the first
/// check because it is a single atomic load and is never false while some VM
/// is watching.
#[inline]
pub fn fire_field_access_if_watched_for_vm(
    vm: usize,
    thread: ThreadId,
    method: MethodId,
    class_id: u64,
    field_index: usize,
) {
    if !any_field_watchpoint_active() {
        return;
    }
    if let Some(wp) = field_watchpoint_for_vm(vm, class_id, field_index) {
        if wp.access_watched {
            let field_id = encode_field_id(class_id, field_index);
            if let Some(m) = manager_for_vm(vm) {
                m.fire_field_access(thread, method, field_id);
            }
        }
    }
}

/// VM-scoped [`fire_field_modification_if_watched`].
#[inline]
pub fn fire_field_modification_if_watched_for_vm(
    vm: usize,
    thread: ThreadId,
    method: MethodId,
    class_id: u64,
    field_index: usize,
) {
    if !any_field_watchpoint_active() {
        return;
    }
    if let Some(wp) = field_watchpoint_for_vm(vm, class_id, field_index) {
        if wp.modification_watched {
            let field_id = encode_field_id(class_id, field_index);
            if let Some(m) = manager_for_vm(vm) {
                m.fire_field_modification(thread, method, field_id);
            }
        }
    }
}

/// Pack a (class_id, field_index) pair into a single `FieldId` u64.  The
/// JVMTI spec treats field ids as opaque to agents; we pick a packing that
/// keeps both halves decodable (upper 32 bits = class id, lower 32 bits =
/// field index). Class ids currently fit in 32 bits in `types::ClassId`.
#[inline]
pub fn encode_field_id(class_id: u64, field_index: usize) -> FieldId {
    (class_id << 32) | ((field_index as u64) & 0xFFFF_FFFF)
}

/// Inverse of [`encode_field_id`].
#[inline]
pub fn decode_field_id(field_id: FieldId) -> (u64, usize) {
    (field_id >> 32, (field_id & 0xFFFF_FFFF) as usize)
}

/// Fire ObjectFree at the global level. Called from the GC after a
/// tagged object is reclaimed.
pub fn fire_object_free(tag: i64) {
    if let Some(m) = global_manager() {
        m.fire_object_free(tag);
    }
}

/// Fire VMObjectAlloc at the global level. Called from the allocator
/// fast path; caller should branch on [`any_listener_active`] first.
pub fn fire_vm_object_alloc(thread: ThreadId, object_addr: u64, class_id: ClassId, size: usize) {
    if let Some(m) = global_manager() {
        m.fire_vm_object_alloc(thread, object_addr, class_id, size);
    }
}

/// Record an allocation for the sampled-allocation event stream. Returns
/// true if a SampledObjectAlloc event was fired.
pub fn record_allocation_sample(
    thread: ThreadId,
    object_addr: u64,
    class_id: ClassId,
    size: usize,
) -> bool {
    match global_manager() {
        Some(m) => m.record_allocation_sample(thread, object_addr, class_id, size),
        None => false,
    }
}

/// Fire DataDumpRequest at the global level.
pub fn fire_data_dump_request() {
    if let Some(m) = global_manager() {
        m.fire_data_dump_request();
    }
}

/// Test-only reset hook: drain the **unattributed** environment back to a
/// clean state. Used by tests that need a clean slate; not wired into any
/// production path.
///
/// Deliberately touches only the [`UNATTRIBUTED_VM`] row. It must not sweep
/// other VMs' rows: real `SharedVm`s built by other test modules in this
/// crate register their own rows, and dropping those from under them would
/// be a landmine. Tests that create their own VM identity are responsible
/// for calling [`forget_vm_jvmti_state`] themselves.
///
/// The union mirrors are recomputed at the end, so a test that asserts
/// "no listener anywhere" after a reset sees the truth even if another row
/// exists.
#[cfg(test)]
pub(crate) fn reset_global_manager_for_tests() {
    if let Some(m) = global_manager() {
        // Best-effort clear of enabled state so tests don't see stale fires.
        if let Ok(mut g) = m.global_events.write() {
            g.clear();
        }
        if let Ok(mut t) = m.thread_events.write() {
            t.clear();
        }
        if let Ok(mut c) = m.callbacks.write() {
            *c = EventCallbacks::default();
        }
        if let Ok(mut e) = m.attached_envs.write() {
            e.clear();
        }
        if let Ok(mut n) = m.event_counts.lock() {
            n.clear();
        }
        m.any_listener.store(false, Ordering::Release);
        m.any_method_entry_listener.store(false, Ordering::Release);
        m.any_method_exit_listener.store(false, Ordering::Release);
        m.any_single_step_listener.store(false, Ordering::Release);
        m.any_field_access_listener.store(false, Ordering::Release);
        m.any_field_modification_listener
            .store(false, Ordering::Release);
        m.any_frame_pop_listener.store(false, Ordering::Release);
        m.sampling_bytes.store(0, Ordering::Relaxed);
    }
    // Also clear the T17.Δ field-watchpoint table for the unattributed row so
    // watch-dependent tests start empty.
    {
        let mut guard = environments_write();
        if let Some(env) = guard.as_mut().and_then(|m| m.get_mut(&UNATTRIBUTED_VM)) {
            env.watchpoints.clear();
        }
    }
    publish_union_listener_flags();
    refresh_watchpoint_union();
}

/// Process-wide serialisation for tests that move the JVMTI registry's
/// **shared** state: the [`UNATTRIBUTED_VM`] row and the process-wide union
/// listener/watchpoint mirrors. Tests that only touch their own
/// `scoped_test_vm()` row do not need it.
///
/// It lives at module scope, not inside `mod tests`, because the delivery
/// sites this registry exists to serve are in `runtime::interpreter`, and
/// that module's tests move the same union mirrors. A lock only one of two
/// test modules can name is not a lock — an earlier lane found this crate's
/// JVMTI test block was parallel-unsafe and passing by luck, and a
/// cross-module half of the same state would reintroduce exactly that.
///
/// Poison-tolerant on purpose: a panicking test must not convert every later
/// JVMTI test in the binary into a spurious failure that hides the first one.
#[cfg(test)]
pub(crate) fn jvmti_registry_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// Unit Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
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
            ],
        }
    }

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
                tc.fetch_add(1, Ordering::SeqCst);
            })),
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
                c.fetch_add(1, Ordering::SeqCst);
            })),
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
                Some(new)
            })),
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
            JvmtiEventKind::from_raw(60),
            Some(JvmtiEventKind::Breakpoint)
        );
        assert_eq!(JvmtiEventKind::from_raw(0), None);
        assert_eq!(JvmtiEventKind::from_raw(9999), None);
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
                *t.lock().unwrap() += sz;
            })),
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
                c.fetch_add(1, Ordering::SeqCst);
            })),
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
                c.fetch_add(1, Ordering::SeqCst);
            })),
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
        let em = JvmtiEventManager::new();
        em.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::ObjectFree, None)
            .unwrap();

        // Attached env with its own ObjectFree callback.
        let env = Arc::new(JvmtiEnv::new());
        let agent_tags = Arc::new(Mutex::new(Vec::<i64>::new()));
        let t = agent_tags.clone();
        env.event_manager
            .set_event_callbacks(EventCallbacks {
                object_free: Some(Box::new(move |tag| t.lock().unwrap().push(tag))),
                ..Default::default()
            })
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
        let em = JvmtiEventManager::new();
        let tid: ThreadId = 3;
        em.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::ClassLoad, Some(tid))
            .unwrap();
        em.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::ClassPrepare, Some(tid))
            .unwrap();

        let env = Arc::new(JvmtiEnv::new());
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

    #[test]
    fn test_env_dropped_envs_pruned() {
        let em = JvmtiEventManager::new();
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
        fire_exception(1, 1, 0);
        fire_exception_catch(1, 1, 0);
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
                dc.fetch_add(1, Ordering::SeqCst);
            })),
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
            exception_catch: Some(Box::new(move |t, m, l| s.lock().unwrap().push((t, m, l)))),
            ..Default::default()
        })
        .unwrap();
        fire_exception_catch(tid, 99, 10);
        fire_exception_catch(tid, 99, 11);
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
                s.lock().unwrap().push((t, m, exc, rv))
            })),
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
                s.lock().unwrap().push((t, m, exc))
            })),
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
                c.fetch_add(1, Ordering::SeqCst);
            })),
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
                m.fetch_add(1, Ordering::SeqCst);
            })),
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
                s.lock().unwrap().push((t, m, exc))
            })),
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
                    panic!("agent panic");
                }
            })),
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
                    panic!("agent panic in ClassPrepare");
                }
            })),
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
    /// a binary that also constructs real `SharedVm`s. The high base puts these
    /// far outside any plausible allocation.
    fn scoped_test_vm() -> usize {
        use std::sync::atomic::AtomicUsize;
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        0x7000_0000 + NEXT.fetch_add(1, Ordering::Relaxed)
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
                    ha.fetch_add(1, Ordering::SeqCst);
                })),
                ..Default::default()
            })
            .unwrap();
        mgr_b
            .set_event_callbacks(EventCallbacks {
                field_access: Some(Box::new(move |_t, _m, _f| {
                    hb.fetch_add(1, Ordering::SeqCst);
                })),
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
                    sa.fetch_add(1, Ordering::SeqCst);
                })),
                ..Default::default()
            })
            .unwrap();
        mgr_b
            .set_event_callbacks(EventCallbacks {
                method_entry: Some(Box::new(move |_t, _m| {
                    sb.fetch_add(1, Ordering::SeqCst);
                })),
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
}
