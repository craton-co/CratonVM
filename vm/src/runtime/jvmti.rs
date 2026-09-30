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
//! | Reached by native agents | through the bridge below, and (wave 12) the C `jvmtiEnv` of `jvmti/native_env.rs`, which registers its `Breakpoint` / `Exception` delivery here (wave 13) | yes — `agent.rs` does a real `libloading` `dlopen` + `Agent_OnLoad` |
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
//!  * `fire_class_load` is driven by the `classloading` hook adapter,
//!    `fire_class_prepare_for_vm` by the VM's prepare point
//!    (`vm_util::link_claimed_class`, wave 17), `fire_gc_start` / `fire_gc_finish` by the
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

/// All JVMTI event kinds, numbered as `jvmti.h`'s `jvmtiEvent` (interpreter
/// round i1 wave 13: eleven of them carried other numbers — `Breakpoint` was
/// 60, `SingleStep`'s — so [`JvmtiEventKind::from_raw`] of an agent's event
/// number named the wrong event).
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
    MethodEntry = 65,
    MethodExit = 66,
    Exception = 58,
    ExceptionCatch = 59,
    FieldAccess = 63,
    FieldModification = 64,
    Breakpoint = 62,
    SingleStep = 60,
    FramePop = 61,
    GarbageCollectionStart = 81,
    GarbageCollectionFinish = 82,
    MonitorContendedEnter = 75,
    MonitorContendedEntered = 76,
    MonitorWait = 73,
    MonitorWaited = 74,
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
            // Interpreter round i1 wave 25, lane L1: no `JvmtiEnv` function
            // answers `GetOwnedMonitorInfo`, `GetOwnedMonitorStackDepthInfo`
            // or `GetCurrentContendedMonitor` (this env models a thread table
            // of its own and cannot read a thread's monitors), so, as for
            // `can_access_local_variables` (obsaudit D2), they are not
            // potentially available. The JDWP server answers the JDWP
            // commands from the VM (`debug::inspect`, owned monitors), and
            // the C agent table (`jvmti::native_env`) never offered them.
            can_get_owned_monitor_info: false,
            can_get_current_contended_monitor: false,
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
            // Wave 25: see `can_get_owned_monitor_info` above.
            can_get_owned_monitor_stack_depth_info: false,
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
    /// Exception(thread, method, location, exception, catch_method,
    /// catch_location), posted by the interpreter's unwinder once per throw
    /// ([`fire_exception_for_vm`], wave 11). `exception` is the thrown
    /// object's address, valid while the callback runs and allocates nothing
    /// (the VM pins the object across the callback, but a collection the
    /// callback causes may move it); a listener called after another gets
    /// the address as it is then (wave 18). `catch_method` / `catch_location` are
    /// `0` / `-1` when no interpreter frame on the thread catches it.
    pub exception: Option<Box<dyn Fn(ThreadId, MethodId, i64, u64, MethodId, i64) + Send + Sync>>,
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
/// **That contract is currently only partially implemented.** 16 of the 30
/// `fire_*` methods deliver to the attached envs (`deliver`, which runs the
/// `snapshot_envs()` loop); the rest dispatch only to
/// this manager's own `callbacks` table, so an attached env silently receives
/// nothing for them. There is no diagnostic when this happens. Still missing
/// the loop, as of 2026-09-25:
///
/// ```text
/// vm_death         thread_start      thread_end        class_file_load_hook
/// gc_start         gc_finish         monitor_contended_enter
/// monitor_contended_entered          monitor_wait      monitor_waited
/// compiled_method_load               compiled_method_unload
/// dynamic_code_generated
/// ```
///
/// `class_load` / `class_prepare` were in that list and now dispatch per-env;
/// `breakpoint`, `exception` and `exception_catch` joined them in waves 6 and
/// 11 and `vm_init` in wave 13 (interpreter round i1). `class_file_load_hook`
/// needs a semantic decision first, not a copied loop: it returns replacement
/// bytes (with N agents, whose transform wins — first, last, or chained?).
///
/// Enabling is per listener, as JVMTI specifies it per env (interpreter round
/// i1 wave 14): the manager's own callback table receives an event only while
/// the manager's own state enables it, and an attached env's table only while
/// that env's state (`env.event_manager`'s enable sets) does — the delivery
/// loops ask each listener ([`Self::deliver`]). Until wave 14 every attached
/// env received whatever the manager had enabled, and an event a C agent
/// enabled stayed enabled on the manager after the agent disabled it. The
/// fast-path flags and [`Self::is_event_enabled`] are the union over the
/// listeners, recomputed on every enable, disable, attach and detach,
/// including an attached env's own enable ([`Self::refresh_listener_flags`]).
///
/// Production callers of `register_env`: the C `jvmtiEnv` a native agent gets
/// from `GetEnv` (`jvmti::native_env`, waves 12 and 13) registers its
/// `Breakpoint` / `Exception` delivery here. Its `ClassPrepare` and `VMInit`
/// are delivered by `native_env` itself, on the VM's own paths (this
/// manager's `fire_vm_init` runs before the VM is wrapped in its `Arc`; its
/// `ClassPrepare`, per VM since wave 17, reaches the Rust listeners only).
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
    /// When this manager is an attached env's (`JvmtiEnv::event_manager`):
    /// the managers that env is attached to, whose union flags follow this
    /// manager's enable state (wave 14, [`Self::refresh_listener_flags`]).
    parents: RwLock<Vec<Weak<JvmtiEventManager>>>,
    /// Serialises [`Self::recompute_listener_flags`], so a recompute that
    /// read the state before an enable cannot store its answer after the one
    /// that read it afterwards.
    flags_recompute: Mutex<()>,
    /// Fast-path flag: true iff any event is enabled on this manager or on an
    /// attached env, or a callback table was installed since the last
    /// recompute. Checked first in the allocation-path fire_ methods so the
    /// no-agent case costs a single atomic load.
    any_listener: AtomicBool,
    // T17.Δ — per-event fast-path flags. These let hot-path interpreter
    // sites branch on a single `Acquire` load per dispatch without touching
    // the manager's global maps. Each is the union over this manager's own
    // state and its attached envs', recomputed by `recompute_listener_flags`
    // on every enable, disable, attach and detach (wave 14).
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
    /// True iff any listener is interested in `Exception` (wave 11: the
    /// interpreter's unwinder asks it through [`UNION_EXCEPTION`]).
    any_exception_listener: AtomicBool,
    /// True iff any listener is interested in `ExceptionCatch` (interpreter
    /// round i1 wave 21, lane L2; [`UNION_EXCEPTION_CATCH`]).
    any_exception_catch_listener: AtomicBool,
    /// Bytes-allocated since last SampledObjectAlloc fire. Used by the
    /// sampling sub-system to decide when to emit an event; reset by
    /// fire_sampled_object_alloc when the sample threshold is reached.
    sampling_bytes: AtomicU64,
    /// Threshold (in bytes) for SampledObjectAlloc events. Default 512 KB.
    sampling_threshold: AtomicU64,
    /// The classes whose `ClassPrepare` [`fire_class_prepare_for_vm`] has
    /// posted through this manager (interpreter round i1 wave 17): once per
    /// class, as the C `jvmtiEnv` (`jvmti::JvmtiEnv::class_prepare_posted`)
    /// and JDWP post it, although a class is prepared again when a link
    /// failure that raced its supertype's link is retried.
    class_prepare_posted: Mutex<HashSet<ClassId>>,
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
            parents: RwLock::new(Vec::new()),
            flags_recompute: Mutex::new(()),
            any_listener: AtomicBool::new(false),
            any_method_entry_listener: AtomicBool::new(false),
            any_method_exit_listener: AtomicBool::new(false),
            any_single_step_listener: AtomicBool::new(false),
            any_field_access_listener: AtomicBool::new(false),
            any_field_modification_listener: AtomicBool::new(false),
            any_frame_pop_listener: AtomicBool::new(false),
            any_exception_listener: AtomicBool::new(false),
            any_exception_catch_listener: AtomicBool::new(false),
            sampling_bytes: AtomicU64::new(0),
            sampling_threshold: AtomicU64::new(DEFAULT_SAMPLING_INTERVAL_BYTES),
            class_prepare_posted: Mutex::new(HashSet::new()),
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

    /// Is `kind` enabled for `thread` (globally, or for that thread) on this
    /// manager's OWN state — the listener its own callback table belongs to?
    /// `None` asks the global state only.
    fn own_event_enabled(&self, kind: JvmtiEventKind, thread: Option<ThreadId>) -> bool {
        if self.global_events.read().is_ok_and(|g| g.contains(&kind)) {
            return true;
        }
        thread.is_some_and(|tid| {
            self.thread_events
                .read()
                .is_ok_and(|t| t.get(&tid).is_some_and(|s| s.contains(&kind)))
        })
    }

    /// Is `kind` enabled on this manager's own state for any thread?
    fn own_event_enabled_anywhere(&self, kind: JvmtiEventKind) -> bool {
        self.global_events.read().is_ok_and(|g| g.contains(&kind))
            || self
                .thread_events
                .read()
                .is_ok_and(|t| t.values().any(|s| s.contains(&kind)))
    }

    /// Is any event enabled on this manager's own state?
    fn own_any_event_enabled(&self) -> bool {
        self.global_events.read().is_ok_and(|g| !g.is_empty())
            || self
                .thread_events
                .read()
                .is_ok_and(|t| t.values().any(|s| !s.is_empty()))
    }

    /// Does `pred` hold for the event manager of some attached env? Walks the
    /// list under its read lock, allocating nothing.
    fn any_attached_env(&self, pred: impl Fn(&JvmtiEventManager) -> bool) -> bool {
        self.attached_envs.read().is_ok_and(|list| {
            list.iter()
                .any(|w| w.upgrade().is_some_and(|env| pred(&env.event_manager)))
        })
    }

    /// Recompute every fast-path flag as the union over this manager's own
    /// state and every attached env's (wave 14). Off every hot path: called
    /// on enable, disable, attach and detach.
    fn recompute_listener_flags(&self) {
        let _serial = self
            .flags_recompute
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let enabled = |kind: JvmtiEventKind| {
            self.own_event_enabled_anywhere(kind)
                || self.any_attached_env(|m| m.own_event_enabled_anywhere(kind))
        };
        use JvmtiEventKind as K;
        for (kind, flag) in [
            (K::MethodEntry, &self.any_method_entry_listener),
            (K::MethodExit, &self.any_method_exit_listener),
            (K::SingleStep, &self.any_single_step_listener),
            (K::FieldAccess, &self.any_field_access_listener),
            (K::FieldModification, &self.any_field_modification_listener),
            (K::FramePop, &self.any_frame_pop_listener),
            (K::Exception, &self.any_exception_listener),
            (K::ExceptionCatch, &self.any_exception_catch_listener),
        ] {
            flag.store(enabled(kind), Ordering::Release);
        }
        let any =
            self.own_any_event_enabled() || self.any_attached_env(|m| m.own_any_event_enabled());
        self.any_listener.store(any, Ordering::Release);
    }

    /// After a change of this manager's enable state or of its attached
    /// envs: recompute its flags and those of every manager its env is
    /// attached to (an attached env's own enable is part of their union),
    /// then publish the process-wide mirrors once.
    fn refresh_listener_flags(&self) {
        self.recompute_listener_flags();
        let parents: Vec<Arc<JvmtiEventManager>> = self
            .parents
            .read()
            .map(|list| list.iter().filter_map(Weak::upgrade).collect())
            .unwrap_or_default();
        for parent in &parents {
            parent.recompute_listener_flags();
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
    /// Fast-path query for `Exception` listener (wave 11).
    #[inline]
    pub fn has_exception_listener(&self) -> bool {
        self.any_exception_listener.load(Ordering::Acquire)
    }
    /// Fast-path query for `ExceptionCatch` listener (wave 21).
    #[inline]
    pub fn has_exception_catch_listener(&self) -> bool {
        self.any_exception_catch_listener.load(Ordering::Acquire)
    }
    /// Is any event enabled that only the interpreter posts (`SingleStep`,
    /// `MethodEntry`, `MethodExit`, `FramePop`, `FieldAccess`,
    /// `FieldModification`, and `Exception` while
    /// [`EXCEPTION_EVENTS_NEED_THE_INTERPRETER`] holds)? While it is, this VM
    /// must not enter compiled code — see [`interp_only_events_active_for_vm`].
    pub fn has_interp_only_listener(&self) -> bool {
        self.has_single_step_listener()
            || self.has_method_entry_listener()
            || self.has_method_exit_listener()
            || self.has_frame_pop_listener()
            || self.has_field_access_listener()
            || self.has_field_modification_listener()
            || (EXCEPTION_EVENTS_NEED_THE_INTERPRETER && self.has_exception_listener())
    }

    /// Attach a JvmtiEnv: its callback table receives the events its own
    /// state (`env.event_manager`) enables ([`Self::deliver`]), and that
    /// state joins this manager's union flags. The env is held by
    /// `Weak<JvmtiEnv>` so dropping the env does not keep the manager from
    /// garbage-collecting it. Takes the manager's `Arc` so the env's own
    /// enable changes can reach this manager's flags (wave 14).
    pub fn register_env(self: &Arc<Self>, env: &Arc<JvmtiEnv>) -> JvmtiResult<()> {
        self.attached_envs
            .write()
            .map_err(|_| JvmtiError::Internal)?
            .push(Arc::downgrade(env));
        if let Ok(mut parents) = env.event_manager.parents.write() {
            parents.retain(|p| p.strong_count() > 0);
            parents.push(Arc::downgrade(self));
        }
        self.refresh_listener_flags();
        Ok(())
    }

    /// Remove an attached JvmtiEnv (agent unload, `DisposeEnvironment`): its
    /// callbacks receive nothing more, and its enable state leaves this
    /// manager's union flags.
    pub fn unregister_env(self: &Arc<Self>, env: &Arc<JvmtiEnv>) -> JvmtiResult<()> {
        {
            let mut list = self
                .attached_envs
                .write()
                .map_err(|_| JvmtiError::Internal)?;
            list.retain(|w| w.upgrade().is_some_and(|e| !Arc::ptr_eq(&e, env)));
        }
        if let Ok(mut parents) = env.event_manager.parents.write() {
            parents.retain(|p| p.upgrade().is_some_and(|p| !Arc::ptr_eq(&p, self)));
        }
        self.refresh_listener_flags();
        Ok(())
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

    /// Set event notification mode globally or for a specific thread, on this
    /// manager's own state. On an attached env's manager this is the env's
    /// enable, and the managers it is attached to follow it.
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
        // The locks above are released; the recompute re-takes read locks.
        // T17.Δ — every flag follows both enable and disable transitions, so
        // the hot-path interpreter sites see the correct value.
        self.refresh_listener_flags();
        Ok(())
    }

    /// Replace this manager's own enable state for `kind` in one step:
    /// enabled globally iff `global`, and for exactly the threads in
    /// `threads` (wave 14). For a listener that keeps its enable state
    /// elsewhere and mirrors it here — the C `jvmtiEnv`
    /// (`jvmti::native_env`) mirrors its `Breakpoint` / `Exception` state
    /// into its delivery env — so a disable, a relinquished capability and a
    /// dispose all take the event off the union.
    pub(crate) fn set_event_enablement(
        &self,
        kind: JvmtiEventKind,
        global: bool,
        threads: &[ThreadId],
    ) -> JvmtiResult<()> {
        {
            let mut g = self
                .global_events
                .write()
                .map_err(|_| JvmtiError::Internal)?;
            if global {
                g.insert(kind);
            } else {
                g.remove(&kind);
            }
        }
        {
            let mut per_thread = self
                .thread_events
                .write()
                .map_err(|_| JvmtiError::Internal)?;
            for (tid, set) in per_thread.iter_mut() {
                if !threads.contains(tid) {
                    set.remove(&kind);
                }
            }
            per_thread.retain(|_, set| !set.is_empty());
            for &tid in threads {
                per_thread.entry(tid).or_default().insert(kind);
            }
        }
        self.refresh_listener_flags();
        Ok(())
    }

    /// Is the event enabled (globally or for the given thread) for ANY
    /// listener of this manager: its own state or an attached env's (wave
    /// 14; the union the interpreter's exact checks ask, e.g.
    /// [`exception_event_enabled_for_vm`]). Delivery asks each listener.
    pub fn is_event_enabled(&self, event_kind: JvmtiEventKind, thread: Option<ThreadId>) -> bool {
        self.own_event_enabled(event_kind, thread)
            || self.any_attached_env(|m| m.own_event_enabled(event_kind, thread))
    }

    /// Set the full callback table. Replaces all previous callbacks.
    pub fn set_event_callbacks(&self, cbs: EventCallbacks) -> JvmtiResult<()> {
        let mut current = self.callbacks.write().map_err(|_| JvmtiError::Internal)?;
        *current = cbs;
        drop(current);
        // T17.Δ — keep each per-event flag in sync with the *actual* enabled
        // state so the interpreter fast path remains correct: installing
        // callbacks enables nothing.
        self.recompute_listener_flags();
        // Installing callbacks implies the caller wants events to be delivered
        // (even if mode is not yet Enabled — match existing tests that call
        // fire_* directly). Setting the flag is safe: fire_ methods still
        // gate on the listeners' enable state before recording/dispatching.
        self.any_listener.store(true, Ordering::Release);
        publish_union_listener_flags();
        Ok(())
    }

    /// Deliver an event to every listener that has `kind` enabled for
    /// `thread` (`None`: the global state only): this manager's own callback
    /// table while its own state enables it, and each attached env's table
    /// while that env's own state does (wave 14 — JVMTI enables events per
    /// env; every attached env used to receive whatever the manager had
    /// enabled, and an env that enabled an event the manager had not got
    /// nothing). `call` runs once per listener's table. The event is counted
    /// when some listener took it.
    fn deliver(
        &self,
        kind: JvmtiEventKind,
        thread: Option<ThreadId>,
        call: impl Fn(&EventCallbacks),
    ) {
        self.deliver_with_object(kind, thread, 0, |cbs, _| call(cbs));
    }

    /// [`Self::deliver`] for an event that carries an object (`Exception`'s
    /// exception, `MethodExit`'s reference result): `call` gets, with each
    /// listener's table, the object's address as it is when THAT listener is
    /// called (`0` stays `0`).
    ///
    /// Interpreter round i1 wave 18, lane L1: every listener used to get the
    /// address the producer read before the first one ran. A listener may
    /// collect (a C agent's callback runs with a `JNIEnv*`), and the producer
    /// keeps the object in a root the collector rewrites, not in the captured
    /// address, so after a moving collection in the first callback the second
    /// listener got whatever occupied the old address. When a second listener
    /// follows the first, the object is held in a JNI global reference
    /// ([`EventObjectRoot`]) for the whole delivery and each listener gets its
    /// current address.
    fn deliver_with_object(
        &self,
        kind: JvmtiEventKind,
        thread: Option<ThreadId>,
        object: u64,
        call: impl Fn(&EventCallbacks, u64),
    ) {
        let own = self.own_event_enabled(kind, thread);
        let mut envs = self.snapshot_envs();
        envs.retain(|env| env.event_manager.own_event_enabled(kind, thread));
        if !own && envs.is_empty() {
            return;
        }
        self.record_event(kind);
        // Rooted before the first listener runs; the first one's address is
        // the producer's own, so one listener alone needs no root.
        let root = if object != 0 && usize::from(own) + envs.len() > 1 {
            EventObjectRoot::new(self.vm, object)
        } else {
            None
        };
        let current = || root.as_ref().map_or(object, EventObjectRoot::address);
        if own {
            if let Ok(cbs) = self.callbacks.read() {
                call(&*cbs, current());
            }
        }
        for env in envs {
            if let Ok(cbs) = env.event_manager.callbacks.read() {
                call(&*cbs, current());
            }
        }
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
        // Wave 13: the attached envs too, each behind `catch_unwind`, as
        // `fire_class_prepare` delivers (the manager's callback ran bare and
        // the envs got nothing).
        self.deliver(JvmtiEventKind::VmInit, None, |cbs| {
            if let Some(ref cb) = cbs.vm_init {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(&**cb));
            }
        });
    }

    pub fn fire_vm_death(&self) {
        #[cfg(feature = "experimental-debug")]
        // obsaudit D14 bridge — see the notes above `install_real_agent_env_bridge`.
        if let Some(shared) = self.bridged_shared() {
            crate::jvmti::notify_vm_death(&shared.debug.jvmti_env.lock());
        }
        if !self.own_event_enabled(JvmtiEventKind::VmDeath, None) {
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
        if !self.own_event_enabled(JvmtiEventKind::ThreadStart, Some(thread)) {
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
        if !self.own_event_enabled(JvmtiEventKind::ThreadEnd, Some(thread)) {
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
        if !self.own_event_enabled(JvmtiEventKind::ClassFileLoadHook, None) {
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
        self.post_class_load(self.vm, thread, class_id);
    }

    /// Post ClassLoad for a class of VM `owner` through this manager: to its
    /// listeners, and to the real env behind `owner`'s bridge
    /// ([`UNATTRIBUTED_VM`]: the sole live bridge, which is what
    /// [`Self::bridged_shared`] answers for the unattributed manager).
    ///
    /// [`fire_class_load_for_vm`] names the owner even when it resolved the
    /// unattributed manager (the VM has no row yet), so the bridge never
    /// guesses for an event whose VM is known (interpreter round i1 wave 22,
    /// lane L5). Before, a second VM's bootstrap class loads reached the first
    /// VM's native agent whenever the first was the only bridged VM.
    #[cfg_attr(not(feature = "experimental-debug"), allow(unused_variables))]
    fn post_class_load(&self, owner: usize, thread: ThreadId, class_id: ClassId) {
        // obsaudit D14 bridge — see the notes above `install_real_agent_env_bridge`.
        // Replaces the old hand-written call site in `vm/src/vm/vm_init.rs`
        // (a narrower helper that only covered one dynamic-load path), which
        // was removed so ClassLoad reaches the real env exactly once, from
        // every class-definition path, not just that one.
        #[cfg(feature = "experimental-debug")]
        {
            let bridge = if owner == UNATTRIBUTED_VM {
                sole_live_bridge()
            } else {
                bridge_for_vm(owner)
            };
            if let Some(shared) = bridge {
                if let Some(name) = resolve_class_name_for_bridge(&shared, class_id) {
                    crate::jvmti::notify_class_load(
                        &shared.debug.jvmti_env.lock(),
                        class_id,
                        &name,
                    );
                }
            }
        }
        self.deliver(JvmtiEventKind::ClassLoad, Some(thread), |cbs| {
            if let Some(ref cb) = cbs.class_load {
                let _ =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(thread, class_id)));
            }
        });
    }

    /// Record that `ClassPrepare` for `class_id` is being posted through this
    /// manager; `false` if it already was ([`fire_class_prepare_for_vm`]).
    fn note_class_prepare_posted(&self, class_id: ClassId) -> bool {
        self.class_prepare_posted
            .lock()
            .map(|mut posted| posted.insert(class_id))
            .unwrap_or(true)
    }

    /// Fire ClassPrepare. Production posts it through
    /// [`fire_class_prepare_for_vm`], from the VM's one prepare point, once
    /// per class; this entry point posts unconditionally (tests, and the
    /// VM-less [`fire_class_prepare`]). No lock is held while the callbacks
    /// run, so a listener may call back into the class manager.
    pub fn fire_class_prepare(&self, thread: ThreadId, class_id: ClassId) {
        // obsaudit D14 bridge — see the notes above `install_real_agent_env_bridge`.
        #[cfg(feature = "experimental-debug")]
        if let Some(shared) = self.bridged_shared() {
            if let Some(name) = resolve_class_name_for_bridge(&shared, class_id) {
                crate::jvmti::notify_class_prepare(&shared.debug.jvmti_env.lock(), class_id, &name);
            }
        }
        self.deliver(JvmtiEventKind::ClassPrepare, Some(thread), |cbs| {
            if let Some(ref cb) = cbs.class_prepare {
                let _ =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(thread, class_id)));
            }
        });
    }

    pub fn fire_method_entry(&self, thread: ThreadId, method: MethodId) {
        self.deliver(JvmtiEventKind::MethodEntry, Some(thread), |cbs| {
            if let Some(ref cb) = cbs.method_entry {
                // Panic-safe: agent callbacks may panic, don't bring down VM.
                let _ =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(thread, method)));
            }
        });
    }

    pub fn fire_method_exit(
        &self,
        thread: ThreadId,
        method: MethodId,
        was_popped_by_exception: bool,
        return_value: LocalValue,
    ) {
        // A reference result is handed to each listener at its current
        // address (wave 18, [`Self::deliver_with_object`]).
        let object = match return_value {
            LocalValue::Object(Some(address)) => address,
            _ => 0,
        };
        self.deliver_with_object(
            JvmtiEventKind::MethodExit,
            Some(thread),
            object,
            |cbs, now| {
                if let Some(ref cb) = cbs.method_exit {
                    let rv = if object == 0 {
                        return_value.clone()
                    } else {
                        LocalValue::Object((now != 0).then_some(now))
                    };
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        cb(thread, method, was_popped_by_exception, rv)
                    }));
                }
            },
        );
    }

    /// Wave 11: posted by the interpreter's unwinder
    /// ([`fire_exception_for_vm`]); it had no producer. Delivered like
    /// [`Self::fire_breakpoint`]: the manager's callback and every attached
    /// env's, each behind `catch_unwind` (it called the manager's callback
    /// bare and skipped the envs), with the exception and the catch location
    /// JVMTI's `Exception` event carries.
    pub fn fire_exception(
        &self,
        thread: ThreadId,
        method: MethodId,
        location: i64,
        exception: u64,
        catch_method: MethodId,
        catch_location: i64,
    ) {
        // Each listener gets the exception's current address (wave 18,
        // [`Self::deliver_with_object`]).
        self.deliver_with_object(
            JvmtiEventKind::Exception,
            Some(thread),
            exception,
            |cbs, now| {
                if let Some(ref cb) = cbs.exception {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        cb(thread, method, location, now, catch_method, catch_location)
                    }));
                }
            },
        );
    }

    /// Wave 11: delivered to the attached envs and behind `catch_unwind`, as
    /// [`Self::fire_exception`] is (it called the manager's callback bare).
    pub fn fire_exception_catch(&self, thread: ThreadId, method: MethodId, location: i64) {
        self.deliver(JvmtiEventKind::ExceptionCatch, Some(thread), |cbs| {
            if let Some(ref cb) = cbs.exception_catch {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    cb(thread, method, location)
                }));
            }
        });
    }

    pub fn fire_field_access(&self, thread: ThreadId, method: MethodId, field: FieldId) {
        self.deliver(JvmtiEventKind::FieldAccess, Some(thread), |cbs| {
            if let Some(ref cb) = cbs.field_access {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    cb(thread, method, field)
                }));
            }
        });
    }

    pub fn fire_field_modification(&self, thread: ThreadId, method: MethodId, field: FieldId) {
        self.deliver(JvmtiEventKind::FieldModification, Some(thread), |cbs| {
            if let Some(ref cb) = cbs.field_modification {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    cb(thread, method, field)
                }));
            }
        });
    }

    /// Delivered like [`Self::fire_single_step`], its per-bytecode twin: the
    /// manager's own callback and every attached env's, each behind
    /// `catch_unwind` (wave 6 — this called the manager's callback bare and
    /// skipped the envs). Wave 10: called by the dispatch loop's suspend
    /// point for the VM's breakpoints ([`set_breakpoint_for_vm`],
    /// [`fire_breakpoint_for_vm`]).
    pub fn fire_breakpoint(&self, thread: ThreadId, method: MethodId, location: i64) {
        self.deliver(JvmtiEventKind::Breakpoint, Some(thread), |cbs| {
            if let Some(ref cb) = cbs.breakpoint {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    cb(thread, method, location)
                }));
            }
        });
    }

    pub fn fire_single_step(&self, thread: ThreadId, method: MethodId, location: i64) {
        self.deliver(JvmtiEventKind::SingleStep, Some(thread), |cbs| {
            if let Some(ref cb) = cbs.single_step {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    cb(thread, method, location)
                }));
            }
        });
    }

    pub fn fire_frame_pop(
        &self,
        thread: ThreadId,
        method: MethodId,
        was_popped_by_exception: bool,
    ) {
        self.deliver(JvmtiEventKind::FramePop, Some(thread), |cbs| {
            if let Some(ref cb) = cbs.frame_pop {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    cb(thread, method, was_popped_by_exception)
                }));
            }
        });
    }

    /// The real-agent half of the two GC events, which fire INSIDE the pause
    /// (from `VmHeap`'s dispatcher, the world stopped).
    ///
    /// gc-common w6-f (`common-e-small-findings` item 6): this used to take
    /// `jvmti_env.lock()` unconditionally. That mutex is an ordinary
    /// mutator-side lock; a thread that holds it and then reaches a safepoint
    /// (an agent callback that allocates, a JVMTI function that walks the
    /// heap) parks holding it, and the collecting thread then waits for it
    /// with every mutator stopped — a deadlock nothing could break. Now a
    /// bounded wait: if the env is not free within
    /// `GC_EVENT_ENV_WAIT`, this event is not delivered to the real agent (one
    /// warning per process says so) and the pause proceeds. HotSpot's
    /// equivalent callbacks are likewise restricted to raw-monitor operations
    /// for exactly this reason.
    #[cfg(feature = "experimental-debug")]
    fn bridge_gc_event(shared: &crate::vm::SharedVm, start: bool) {
        const GC_EVENT_ENV_WAIT: std::time::Duration = std::time::Duration::from_millis(5);
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        match shared.debug.jvmti_env.try_lock_for(GC_EVENT_ENV_WAIT) {
            Some(env) => {
                if start {
                    crate::jvmti::notify_gc_start(&env);
                } else {
                    crate::jvmti::notify_gc_finish(&env);
                }
            }
            None => {
                if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::warn!(
                        "JVMTI: a GarbageCollection{} event was not delivered to the \
                         native agent: its env lock was held when the pause began \
                         (reported once)",
                        if start { "Start" } else { "Finish" }
                    );
                }
            }
        }
    }

    pub fn fire_gc_start(&self) {
        // obsaudit D14 bridge — see the notes above `install_real_agent_env_bridge`.
        #[cfg(feature = "experimental-debug")]
        if let Some(shared) = self.bridged_shared() {
            Self::bridge_gc_event(&shared, true);
        }
        if !self.own_event_enabled(JvmtiEventKind::GarbageCollectionStart, None) {
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
            Self::bridge_gc_event(&shared, false);
        }
        if !self.own_event_enabled(JvmtiEventKind::GarbageCollectionFinish, None) {
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
        if !self.own_event_enabled(JvmtiEventKind::MonitorContendedEnter, Some(thread)) {
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
        if !self.own_event_enabled(JvmtiEventKind::MonitorContendedEntered, Some(thread)) {
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
        if !self.own_event_enabled(JvmtiEventKind::MonitorWait, Some(thread)) {
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
        if !self.own_event_enabled(JvmtiEventKind::MonitorWaited, Some(thread)) {
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
        if !self.own_event_enabled(JvmtiEventKind::CompiledMethodLoad, None) {
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
        if !self.own_event_enabled(JvmtiEventKind::CompiledMethodUnload, None) {
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
        if !self.own_event_enabled(JvmtiEventKind::DynamicCodeGenerated, None) {
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
        if !self.own_event_enabled(JvmtiEventKind::NativeMethodBind, Some(thread)) {
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

    /// Fire ObjectFree — called once per tagged object that was collected.
    ///
    /// `tag` is the user-defined tag installed via SetTag. A tag of 0 means
    /// "no tag" and should not normally reach this path; callers (the GC's
    /// tag-sweep step) are expected to filter those out. Each attached env
    /// receives it while its own state enables `ObjectFree` (wave 14; it
    /// went to every attached env whenever this manager had it enabled).
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
        self.deliver(JvmtiEventKind::ObjectFree, None, |cbs| {
            if let Some(ref cb) = cbs.object_free {
                cb(tag);
            }
        });
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
        self.deliver(JvmtiEventKind::VMObjectAlloc, Some(thread), |cbs| {
            if let Some(ref cb) = cbs.vm_object_alloc {
                cb(thread, object_addr, class_id, size);
            }
        });
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
        self.deliver(JvmtiEventKind::SampledObjectAlloc, Some(thread), |cbs| {
            if let Some(ref cb) = cbs.sampled_object_alloc {
                cb(thread, object_addr, class_id, size);
            }
        });
    }

    /// Fire DataDumpRequest — called when a debugger asks for a heap dump.
    /// Parameterless by spec; the agent is expected to call into IterateOverHeap
    /// (or similar) from inside the callback to materialize the dump.
    pub fn fire_data_dump_request(&self) {
        if !self.has_any_listener() {
            return;
        }
        self.deliver(JvmtiEventKind::DataDumpRequest, None, |cbs| {
            if let Some(ref cb) = cbs.data_dump_request {
                cb();
            }
        });
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
    /// Test-only: no JVMTI function table entry reaches it yet.
    #[cfg(test)]
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
    ///
    /// Test-only: no production path has ever called it (the class-load path
    /// does not feed this table). It stopped being masked by a same-named
    /// method in the deleted, unwired `gc/src/class_unloading.rs` on
    /// 2026-09-23, which is when the test-only-public-API ratchet saw it.
    #[cfg(test)]
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
    /// capability this env has already declared it cannot honor. Since
    /// interpreter round i1 wave 25 every field is checked against
    /// `potentially_available()`, which also withholds the owned- and
    /// contended-monitor capabilities (no function here answers them).
    pub fn add_capabilities(&self, requested: &JvmtiCapabilities) -> JvmtiResult<()> {
        if !requested
            .subtract(&JvmtiCapabilities::potentially_available())
            .is_empty()
        {
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
    /// This VM's JVMTI breakpoints (interpreter round i1 wave 10), as
    /// `(class id, debug::jdwp_method_id of the method's name and
    /// descriptor, bytecode index)` — the key the dispatch loop's suspend
    /// point already computes for a JDWP breakpoint. See
    /// [`set_breakpoint_for_vm`]. Metadata only, like `watchpoints`.
    breakpoints: HashSet<(u64, u64, u64)>,
    /// This VM's interpreter-only answer as [`publish_union_listener_flags`]
    /// last saw it, so that function can tell the false→true edge on which
    /// the VM's JIT inline caches are flushed. Atomic because the publisher
    /// holds only the read lock.
    interp_only_seen: AtomicBool,
    /// Whether this VM's manager listened for an event HotSpot posts from
    /// its interpreter only ([`needs_every_frame_interpreted`]) when
    /// [`publish_union_listener_flags`] last looked (interpreter round i1
    /// wave 37, lane L1), so that function can tell the edges on which the
    /// VM's compiled code is withdrawn and on which compiling resumes.
    every_frame_seen: AtomicBool,
    /// The internal name of every class a JVMTI breakpoint was set in, by
    /// class id (interpreter round i1 wave 38, lane L1): the debugger gate's
    /// publication reads it ([`breakpoint_class_name_for_vm`]) to withdraw
    /// only that class's compiled dependents, since it runs under the
    /// debug-state lock and must not take the class manager; without a name
    /// every compiled body would be withdrawn. Recorded by
    /// [`set_breakpoint_for_vm`], which has the class in hand. Metadata only,
    /// one entry per class ever given a breakpoint.
    #[cfg(feature = "experimental-debug")]
    breakpoint_class_names: HashMap<u64, Arc<str>>,
}

impl VmJvmtiEnvironment {
    fn empty() -> Self {
        Self {
            manager: None,
            bridge: None,
            watchpoints: HashMap::new(),
            breakpoints: HashSet::new(),
            interp_only_seen: AtomicBool::new(false),
            every_frame_seen: AtomicBool::new(false),
            #[cfg(feature = "experimental-debug")]
            breakpoint_class_names: HashMap::new(),
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
/// `Exception` (wave 11): the unwinder's pre-filter. Joins
/// [`UNION_INTERP_ONLY`] while [`EXCEPTION_EVENTS_NEED_THE_INTERPRETER`]
/// holds (wave 18).
static UNION_EXCEPTION: AtomicBool = AtomicBool::new(false);
/// `ExceptionCatch` (interpreter round i1 wave 21, lane L2): the catch hooks'
/// pre-filter, and what makes a compiled local handler decline
/// ([`exception_catch_listener_active_for_vm`]). Not interpreter-only.
static UNION_EXCEPTION_CATCH: AtomicBool = AtomicBool::new(false);
/// The OR of the interpreter-only event mirrors above, so the
/// interpreter→compiled doors pay ONE load when no agent listens for any of
/// them. See [`interp_only_events_active_for_vm`].
static UNION_INTERP_ONLY: AtomicBool = AtomicBool::new(false);

/// Does a JVMTI `Exception` listener put its VM in interpreter-only mode, as
/// the six events only the interpreter posts do (interpreter round i1 wave
/// 18, lane L1; page
/// `docs/internal/fixed-bugs/interpreter-L1-jvmti-exception-events-lost-for-throws-caught-in-compiled-code-FIXED-20260925.md`)?
///
/// The event is posted by the interpreter's unwinder only
/// (`interpreter::deliver_exception_event_if_armed`). A throw that compiled
/// code catches itself — a compiled local handler, or one of the VM doors
/// that resumes a compiled method at its handler
/// (`helpers::every_compiled_catch_door_drains_the_leftover_native_return`
/// names all five) — never reaches the unwinder, so while compiled code runs
/// the agent misses exactly the exceptions hot methods throw and catch. With
/// this `true` the JIT's doors stand down while the event is enabled
/// (JDWP's `Exception` request does the same through `interpret_all`) and
/// running compiled frames leave at their next poll: correct, and slow only
/// while such an agent listens.
///
/// Since wave 21 (lane L2) the compiled catch doors post the event
/// themselves (`interpreter::report_exception_caught_by_compiled_door`) and
/// the compiled local handler declines while an exception client is armed,
/// so `false` would no longer LOSE an event. It stays `true` because `false`
/// would still report some at the wrong place: a throw in a compiled callee
/// reached through a baked direct call is first seen by its caller's door and
/// reported at the caller's call site, and the predicted catch location skips
/// compiled frames. What `false` needs is on
/// `docs/known-issues/interpreter/i10-L1-proposal-exception-and-watch-events-from-compiled-code-20260925.md`
/// (Progress, wave 21).
pub(crate) const EXCEPTION_EVENTS_NEED_THE_INTERPRETER: bool = true;

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
///
/// Also where a VM ENTERS interpreter-only mode, so it flushes that VM's JIT
/// inline caches on the false→true edge (interpreter round i1 wave 9): a
/// compiled frame already running when the agent enables one of those
/// events keeps calling compiled callees through its MIC/PIC slots without
/// reaching a helper, and the helpers are what stand down. After the flush
/// every such virtual call misses into its helper, which interprets the
/// callee. The helpers refill nothing while the mode holds, and nothing is
/// retired, so the slots refill after the agent disables the events. A loop
/// running in a single-pass OSR body leaves it at its next back-edge poll
/// (`interpreter::request_compiled_loop_exits`, wave 12). Baked direct CALLs
/// and the other running frames remain compiled — see
/// `docs/internal/fixed-bugs/interpreter-L5-jvmti-frames-already-compiled-finish-compiled-FIXED-20261005.md`.
fn publish_union_listener_flags() {
    let (mut any, mut me, mut mx, mut ss, mut fa, mut fm, mut fp, mut ex) =
        (false, false, false, false, false, false, false, false);
    let mut ec = false;
    let mut entered_interp_only: Vec<Arc<crate::vm::SharedVm>> = Vec::new();
    // Wave 37 (lane L1): the VMs whose every-frame answer changed.
    let mut every_frame_moved: Vec<Arc<crate::vm::SharedVm>> = Vec::new();
    {
        let guard = environments_read();
        if let Some(map) = guard.as_ref() {
            for e in map.values() {
                let Some(m) = e.manager.as_ref() else {
                    continue;
                };
                // Recorded only for a VM whose bridge is up, so an edge seen
                // before `Vm::new` installed it is taken at a later
                // publication rather than lost.
                let every_frame = needs_every_frame_interpreted(m);
                if e.every_frame_seen.load(Ordering::Acquire) != every_frame {
                    if let Some(shared) = e.bridge.as_ref().and_then(Weak::upgrade) {
                        if e.every_frame_seen.swap(every_frame, Ordering::AcqRel) != every_frame {
                            every_frame_moved.push(shared);
                        }
                    }
                }
                any |= m.has_any_listener();
                me |= m.has_method_entry_listener();
                mx |= m.has_method_exit_listener();
                ss |= m.has_single_step_listener();
                fa |= m.has_field_access_listener();
                fm |= m.has_field_modification_listener();
                fp |= m.has_frame_pop_listener();
                ex |= m.has_exception_listener();
                ec |= m.has_exception_catch_listener();
                let interp_only = m.has_interp_only_listener();
                if interp_only && !e.interp_only_seen.swap(interp_only, Ordering::AcqRel) {
                    if let Some(shared) = e.bridge.as_ref().and_then(Weak::upgrade) {
                        entered_interp_only.push(shared);
                    }
                } else if !interp_only {
                    e.interp_only_seen.store(false, Ordering::Release);
                }
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
    UNION_EXCEPTION.store(ex, Ordering::Release);
    UNION_EXCEPTION_CATCH.store(ec, Ordering::Release);
    let ex_interp_only = EXCEPTION_EVENTS_NEED_THE_INTERPRETER && ex;
    UNION_INTERP_ONLY.store(
        me || mx || ss || fa || fm || fp || ex_interp_only,
        Ordering::Release,
    );
    // Wave 37 (lane L1): withdraw the VM's compiled code on the edge where an
    // agent starts listening for an event HotSpot posts from its interpreter
    // only, and let compiling resume on the edge where the last such listener
    // goes (`interpreter::note_every_method_needs_the_interpreter`). The
    // answer is read again here, not taken from the edge: two publications
    // racing past the registry lock then both apply the latest one. After
    // the mirrors and outside the registry lock, as below, and before the
    // loop-exit pause below, so a frame polling in it reads the withdrawal.
    // The withdrawal state lives in the debugger gates, which exist only in
    // an `experimental-debug` build; without it the behaviour of waves 12-36.
    #[cfg(not(feature = "experimental-debug"))]
    drop(every_frame_moved);
    #[cfg(feature = "experimental-debug")]
    for shared in every_frame_moved {
        let armed = manager_for_vm(shared.vm_identity)
            .is_some_and(|m| needs_every_frame_interpreted(&m));
        crate::runtime::interpreter::note_every_method_needs_the_interpreter(
            &shared,
            crate::debug::WITHDRAWAL_BY_JVMTI,
            armed,
        );
    }
    // After the mirrors (a helper that misses must already see the mode, or
    // it would refill the slot it just lost) and outside the registry lock
    // (the flush takes the code cache's mutation lock).
    for shared in entered_interp_only {
        let bodies = shared.jit.jit_cache.clear_inline_caches();
        tracing::debug!(
            "JVMTI: interpreter-only events enabled; cleared the inline caches of \
             {bodies} compiled bodies (vm {})",
            shared.vm_identity
        );
        // And a loop already running in an OSR body leaves it at its next
        // back-edge poll (interpreter round i1 wave 12, lane L4).
        crate::runtime::interpreter::request_compiled_loop_exits(&shared);
    }
}

/// Does `m` listen for an event HotSpot posts from its interpreter only
/// (`MethodEntry`, `MethodExit`, `SingleStep`, `FramePop`), so that its VM's
/// compiled code already running must go back to the interpreter, not only
/// the new calls (interpreter round i1 wave 37, lane L1)? Narrower than
/// `has_interp_only_listener`: a field watch or an `Exception` listener keeps
/// the doors' answer and the loop exits without a withdrawal (the compiled
/// catch doors post `Exception` since wave 21).
fn needs_every_frame_interpreted(m: &JvmtiEventManager) -> bool {
    m.has_method_entry_listener()
        || m.has_method_exit_listener()
        || m.has_single_step_listener()
        || m.has_frame_pop_listener()
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
/// manager, and that is not redundant: the remaining VM-less sites
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
            refresh_breakpoint_union(map);
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

/// An event's object held in a JNI global reference of its VM for the whole
/// of one delivery ([`JvmtiEventManager::deliver_with_object`], interpreter
/// round i1 wave 18, lane L1), so a collection one listener causes cannot
/// leave the next with a stale address: the collector rewrites the global
/// reference, and [`Self::address`] reads it back. Released on drop.
///
/// Only for an attributed manager whose VM registered its bridge (every
/// `Vm::new` does); otherwise the listeners get the producer's address, as
/// before (an unattributed manager cannot name the VM the object lives in).
struct EventObjectRoot {
    shared: Arc<crate::vm::SharedVm>,
    handle: crate::native::jni::JObject,
}

impl EventObjectRoot {
    /// Root the object at `object` (non-zero) in VM `vm`. Called before the
    /// first listener runs, with no safepoint since the producer read the
    /// address from its own root.
    fn new(vm: usize, object: u64) -> Option<Self> {
        if vm == UNATTRIBUTED_VM {
            return None;
        }
        let shared = bridge_for_vm(vm)?;
        // SAFETY: the producer's live, rooted object (see above).
        let obj = unsafe { crate::types::ObjectRef::from_raw(object as usize as *mut u8) }; // Cast: the address the event carries
        let handle = shared.natives.jni_global_refs.lock().add(obj);
        Some(Self { shared, handle })
    }

    /// The object's current address (`0` only if the reference was deleted,
    /// which nothing but [`Drop`] does).
    fn address(&self) -> u64 {
        self.shared
            .natives
            .jni_global_refs
            .lock()
            .resolve(self.handle)
            // Cast: the heap address, as the event carries it
            .map_or(0, |obj| obj.as_ptr() as usize as u64)
    }
}

impl Drop for EventObjectRoot {
    fn drop(&mut self) {
        let _ = self
            .shared
            .natives
            .jni_global_refs
            .lock()
            .remove(self.handle);
    }
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

/// Fire ClassLoad at the global level (the unattributed manager). No
/// production caller since interpreter round i1 wave 22, which posts it
/// through [`fire_class_load_for_vm`]; kept for tests and embedders.
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

/// Fire ClassLoad for a class `vm` defined: into `vm`'s manager, or into the
/// unattributed one while `vm` has none (its bootstrap class loads before
/// `SharedVm::new` installs its row; a class manager built outside a VM).
/// The real-env bridge is `vm`'s own either way, never a guess
/// ([`JvmtiEventManager::post_class_load`]).
///
/// The production ClassLoad path since interpreter round i1 wave 22, lane
/// L5: `class_load_adapter` (`vm/src/vm/vm_init.rs`) names the VM by the
/// defining store's layout domain, which the classloading hook now carries.
/// Through the unattributed manager (the VM-less [`fire_class_load`]) no
/// ClassLoad reached any native agent while two VMs were live, because
/// [`sole_live_bridge`] refuses to pick one, and a Rust listener on that row
/// saw every VM's class ids with no way to tell them apart.
pub fn fire_class_load_for_vm(vm: usize, thread: ThreadId, class_id: ClassId) {
    if let Some(m) = manager_for_vm(vm) {
        m.post_class_load(vm, thread, class_id);
    }
}

/// Fire ClassPrepare at the global level (the unattributed manager). No
/// production caller since interpreter round i1 wave 17, which posts it
/// through [`fire_class_prepare_for_vm`]; kept for tests and embedders.
pub fn fire_class_prepare(thread: ThreadId, class_id: ClassId) {
    if let Some(m) = global_manager() {
        m.fire_class_prepare(thread, class_id);
    }
}

/// Fire ClassPrepare into `vm`'s manager: called by the VM's one prepare
/// point (`vm::vm_util::link_claimed_class`, on the preparing thread, after
/// the class is prepared and before any of its code runs), once per class
/// (interpreter round i1 wave 17). Until wave 17 the class manager's define
/// path queued it for every class it defined, before verification and
/// preparation, for classes never linked, and on the unattributed manager,
/// while the C `jvmtiEnv` and JDWP reported the prepare point.
///
/// With no listener anywhere in the process this is one load
/// ([`any_listener_active`]); with one, it resolves the VM's manager and
/// posts only while that manager has a listener, so a class prepared before
/// any listener attached is not remembered.
pub fn fire_class_prepare_for_vm(vm: usize, thread: ThreadId, class_id: ClassId) {
    if !any_listener_active() {
        return;
    }
    if let Some(m) = manager_for_vm(vm) {
        if m.has_any_listener() && m.note_class_prepare_posted(class_id) {
            m.fire_class_prepare(thread, class_id);
        }
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

/// Fire Exception at the global level (the unattributed manager). Production
/// posts through [`fire_exception_for_vm`].
pub fn fire_exception(
    thread: ThreadId,
    method: MethodId,
    location: i64,
    exception: u64,
    catch_method: MethodId,
    catch_location: i64,
) {
    if let Some(m) = global_manager() {
        m.fire_exception(
            thread,
            method,
            location,
            exception,
            catch_method,
            catch_location,
        );
    }
}

/// Fire Exception on `vm`'s manager (wave 11). Called by the interpreter's
/// unwinder once per throw, behind [`any_exception_listener_active`]; see
/// `interpreter::deliver_exception_event_if_armed`.
pub fn fire_exception_for_vm(
    vm: usize,
    thread: ThreadId,
    method: MethodId,
    location: i64,
    exception: u64,
    catch_method: MethodId,
    catch_location: i64,
) {
    if let Some(m) = manager_for_vm(vm) {
        m.fire_exception(
            thread,
            method,
            location,
            exception,
            catch_method,
            catch_location,
        );
    }
}

/// Is `Exception` enabled for thread `thread` on `vm`'s manager (wave 11)?
/// The unwinder's exact check after [`any_exception_listener_active`].
pub fn exception_event_enabled_for_vm(vm: usize, thread: ThreadId) -> bool {
    manager_for_vm(vm).is_some_and(|m| {
        m.has_exception_listener() && m.is_event_enabled(JvmtiEventKind::Exception, Some(thread))
    })
}

/// Is `ExceptionCatch` enabled for `thread` in VM `vm`? The interpreter's
/// catch hook asks it before resolving the event's method id (interpreter
/// round i1 wave 12): its only pre-filter is the any-listener union, which
/// any agent raises (a breakpoint-only C agent too), and the id costs a
/// class-manager read and a method-table scan per caught exception.
pub fn exception_catch_enabled_for_vm(vm: usize, thread: ThreadId) -> bool {
    manager_for_vm(vm)
        .is_some_and(|m| m.is_event_enabled(JvmtiEventKind::ExceptionCatch, Some(thread)))
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

/// Fast-path query: any listener anywhere interested in `Exception`? The
/// interpreter's unwinder pays this one load per throw (wave 11).
#[inline]
pub fn any_exception_listener_active() -> bool {
    UNION_EXCEPTION.load(Ordering::Acquire)
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

/// Is **this** VM listening for `Exception` (interpreter round i1 wave 21,
/// lane L2)? [`any_exception_listener_active`] first — one load when no VM
/// listens — and only then the registry read, so VM B's agent does not make
/// VM A's compiled local handlers decline
/// (`interpreter::exception_events_may_be_armed`).
#[inline]
pub fn exception_listener_active_for_vm(vm: usize) -> bool {
    any_exception_listener_active() && exception_listener_active_for_vm_exact(vm)
}

#[cold]
#[inline(never)]
fn exception_listener_active_for_vm_exact(vm: usize) -> bool {
    manager_for_vm(vm).is_some_and(|m| m.has_exception_listener())
}

/// Fast-path query: any listener anywhere interested in `ExceptionCatch`
/// (interpreter round i1 wave 21, lane L2)? The catch hooks' pre-filter
/// (`jvmti_events::fire_jvmti_exception_catch`).
#[inline]
pub fn any_exception_catch_listener_active() -> bool {
    UNION_EXCEPTION_CATCH.load(Ordering::Acquire)
}

/// Is **this** VM listening for `ExceptionCatch` (wave 21)? The union first,
/// then the registry read, as [`exception_listener_active_for_vm`]. While it
/// holds, the compiled local handler declines, so every compiled catch
/// reaches a VM door that posts the event.
#[inline]
pub fn exception_catch_listener_active_for_vm(vm: usize) -> bool {
    any_exception_catch_listener_active() && exception_catch_listener_active_for_vm_exact(vm)
}

#[cold]
#[inline(never)]
fn exception_catch_listener_active_for_vm_exact(vm: usize) -> bool {
    manager_for_vm(vm).is_some_and(|m| m.has_exception_catch_listener())
}

/// The `jmethodID`-compatible id of the method `name` + `descriptor` declared
/// by class `class_id` in VM `vm`: `(class_id << 32) | method_index`, the
/// encoding [`crate::jvmti::VmClassMethodProvider`] and JNI's
/// `encode_method_id` hand out, so an id an event carries can be passed back
/// to `GetMethodName` or used as a `jmethodID`.
///
/// Resolved through the VM's registered bridge (`install_real_agent_env_bridge`),
/// which every `Vm::new` installs. `None` when there is none (a bare `SharedVm`
/// in a unit test), the VM is gone, or the class does not declare the method;
/// the caller then falls back to its synthesized id. Only called with a
/// listener active, so its registry read, class-manager read and method scan
/// are off every no-agent path.
pub fn resolve_method_id_for_vm(
    vm: usize,
    class_id: u32,
    name: &str,
    descriptor: &str,
) -> Option<u64> {
    let shared = {
        let guard = environments_read();
        guard.as_ref()?.get(&vm)?.bridge.as_ref()?.upgrade()?
    };
    let index = {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(crate::classloading::ClassId::new(class_id))?;
        class
            .methods
            .iter()
            .position(|m| &*m.name == name && &*m.descriptor == descriptor)?
    };
    // `decode_method_id` reads the index from the low 16 bits.
    let index = u16::try_from(index).ok()?;
    // Widening: u32 class id and u16 index into their halves of the id.
    Some((u64::from(class_id) << 32) | u64::from(index))
}

/// Must `vm` run interpreted, because one of its agents listens for an event
/// only the interpreter posts (`SingleStep`, `MethodEntry`, `MethodExit`,
/// `FramePop`, `FieldAccess`, `FieldModification`, and `Exception` while
/// [`EXCEPTION_EVENTS_NEED_THE_INTERPRETER`] holds)?
///
/// HotSpot's `JvmtiThreadState::interp_only_mode`, at VM granularity: the
/// interpreter→compiled doors and the OSR door ask this before entering
/// compiled code and take their interpreted fallback while it holds, so a
/// compiled call subtree can no longer run without posting its events.
///
/// One `Acquire` load of [`UNION_INTERP_ONLY`] when no VM listens for any of
/// them; only when some VM does is the exact per-VM answer looked up (a
/// registry read), so VM B's agent does not force VM A's code off the JIT.
#[inline]
pub fn interp_only_events_active_for_vm(vm: usize) -> bool {
    UNION_INTERP_ONLY.load(Ordering::Acquire) && interp_only_events_active_for_vm_exact(vm)
}

#[cold]
#[inline(never)]
fn interp_only_events_active_for_vm_exact(vm: usize) -> bool {
    manager_for_vm(vm).is_some_and(|m| m.has_interp_only_listener())
}

/// May a debugger read the locals of `shared`'s frames? HotSpot's
/// `JvmtiExport::can_access_local_variables()`, which keeps every local alive
/// in compiled code; asked by every compile door
/// (`jit::CompileRequest::debugger_observes_locals`, interpreter round i1
/// wave 19, lane L1), whose mode exits then never resume a frame that shows a
/// dead but assigned local as `0`.
///
/// Today only the JDWP agent reads locals (`StackFrame.GetValues`,
/// `debug::debugger_local`): no JVMTI env can hold
/// `can_access_local_variables` (`JvmtiEnv::add_capabilities` refuses it and
/// the C table does not offer it). Like HotSpot's capability, which an agent
/// takes in `Agent_OnLoad`, it is a fact of the VM's start (`jdwp_port`), so
/// every body is compiled under it. An env that ever grants the capability
/// must join this answer. A config read, at compile time only.
pub(crate) fn debugger_observes_locals(shared: &crate::vm::SharedVm) -> bool {
    crate::config::JDWP_SERVER_COMPILED_IN && shared.config.jdwp_port.is_some()
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

/// How many VMs have a JDWP field watch in force (interpreter round i1
/// wave 10): `debug::publish_debugger_gates` counts each VM's edges through
/// [`note_debugger_field_watch`]. Folded into [`FIELD_WATCHPOINTS_ACTIVE`],
/// the one pre-filter every field bytecode and quickened field arm tests, so
/// a JDWP watch turns the quickened arms off and reaches the full handlers,
/// which run the JDWP hook, with no second load on the unwatched path.
static DEBUGGER_FIELD_WATCH_VMS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Serializes [`refresh_watchpoint_union`]: two refreshes racing (a JVMTI
/// watch cleared while a JDWP one is armed) could otherwise store their
/// answers in the wrong order and leave the union down under a live watch.
static WATCHPOINT_UNION_REFRESH: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Recompute [`FIELD_WATCHPOINTS_ACTIVE`] across every registered VM and the
/// JDWP watches ([`DEBUGGER_FIELD_WATCH_VMS`]).
fn refresh_watchpoint_union() {
    let _serial = WATCHPOINT_UNION_REFRESH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let active = DEBUGGER_FIELD_WATCH_VMS.load(Ordering::Acquire) > 0
        || environments_read()
            .as_ref()
            .is_some_and(|m| m.values().any(|e| !e.watchpoints.is_empty()));
    FIELD_WATCHPOINTS_ACTIVE.store(active, Ordering::Release);
}

/// A VM's JDWP field watches came into force (`armed`) or all went (wave
/// 10). Called by `debug::publish_debugger_gates` on each edge only, under
/// that VM's debug-state lock, so the count stays balanced per VM.
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
pub(crate) fn note_debugger_field_watch(armed: bool) {
    if armed {
        DEBUGGER_FIELD_WATCH_VMS.fetch_add(1, Ordering::AcqRel);
    } else {
        // Never below zero, whatever the caller did.
        let _ = DEBUGGER_FIELD_WATCH_VMS.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            Some(n.saturating_sub(1))
        });
    }
    refresh_watchpoint_union();
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

// ---------------------------------------------------------------------------
// Per-VM breakpoints (interpreter round i1 wave 10, lane L1)
// ---------------------------------------------------------------------------
//
// `JvmtiEnv::set_breakpoint` fills a table on an env no VM owns, and nothing
// fired `JvmtiEventManager::fire_breakpoint`. These put a VM's breakpoints in
// its own row and deliver them from the dispatch loop's debugger suspend
// point (`interpreter::deliver_breakpoint_if_set`), whose per-method gate
// (`debug::DebuggerGates`) now covers them: the loop checks, and the JIT's
// doors stand down, only in the methods holding one. Needs the
// `experimental-debug` surface, where that suspend point lives. Wave 12: a
// native agent reaches them through the C `jvmtiEnv` `GetEnv` hands out
// (`jvmti::native_env`: `SetBreakpoint` / `ClearBreakpoint`, and its
// `Breakpoint` callback registered as an env on the VM's manager). The
// Rust-side `jvmti::JvmtiCapabilities::potential()` keeps
// `can_generate_breakpoint_events` false: that env's event manager is not
// what delivers them; the C table negotiates its own capabilities.

/// Some VM has a JVMTI breakpoint: the dispatch loop's suspend point asks the
/// exact per-VM set only behind this one load. Recomputed under the
/// registry's write lock ([`refresh_breakpoint_union`]).
static UNION_BREAKPOINTS: AtomicBool = AtomicBool::new(false);

/// Recompute [`UNION_BREAKPOINTS`] from `map`, under the registry's write
/// lock (so two mutations cannot store their answers out of order).
fn refresh_breakpoint_union(map: &HashMap<usize, VmJvmtiEnvironment>) {
    let any = map.values().any(|e| !e.breakpoints.is_empty());
    UNION_BREAKPOINTS.store(any, Ordering::Release);
}

/// `SetBreakpoint` for VM `shared`: break before the bytecode at `location`
/// of `method`, a real `(class id << 32) | method index` id as
/// [`resolve_method_id_for_vm`] and JNI hand out. `INVALID_METHODID` for an
/// id naming no method, `INVALID_LOCATION` for a method without bytecode or
/// a location outside it, `DUPLICATE` for a breakpoint already set. The
/// event is `JvmtiEventManager::fire_breakpoint` on VM `shared`'s manager,
/// on the executing thread, before the bytecode runs.
#[cfg(feature = "experimental-debug")]
pub fn set_breakpoint_for_vm(
    shared: &crate::vm::SharedVm,
    method: MethodId,
    location: i64,
) -> JvmtiResult<()> {
    let (class_id, key, location, class_name) = breakpoint_key(shared, method, location)?;
    note_breakpoint_class_name(shared, class_id, class_name);
    add_breakpoint_key(shared, class_id, key, location)
}

/// `ClearBreakpoint` for VM `shared` (see [`set_breakpoint_for_vm`]);
/// `NOT_FOUND` for a breakpoint not set.
#[cfg(feature = "experimental-debug")]
pub fn clear_breakpoint_for_vm(
    shared: &crate::vm::SharedVm,
    method: MethodId,
    location: i64,
) -> JvmtiResult<()> {
    let (class_id, key, location, _) = breakpoint_key(shared, method, location)?;
    remove_breakpoint_key(shared, class_id, key, location)
}

/// The per-VM key of a breakpoint request (see `VmJvmtiEnvironment::breakpoints`),
/// and (wave 38) the internal name of the method's class.
#[cfg(feature = "experimental-debug")]
fn breakpoint_key(
    shared: &crate::vm::SharedVm,
    method: MethodId,
    location: i64,
) -> JvmtiResult<(u64, u64, u64, Arc<str>)> {
    // Cast: the id's halves, as `jni::decode_method_id` reads them.
    let class_id = crate::classloading::ClassId::new((method >> 32) as u32);
    // Cast: the low 16 bits are the method index.
    let index = (method & 0xFFFF) as usize;
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(class_id).ok_or(JvmtiError::InvalidMethodId)?;
    let m = class
        .methods
        .get(index)
        .ok_or(JvmtiError::InvalidMethodId)?;
    let code_len = m.code().map_or(0, |c| c.code.len());
    let location = usize::try_from(location).map_err(|_| JvmtiError::InvalidLocation)?;
    if location >= code_len {
        return Err(JvmtiError::InvalidLocation);
    }
    // Widening: a bytecode index fits u64.
    let location = location as u64;
    Ok((
        u64::from(class_id.as_u32()),
        crate::debug::jdwp_method_id(&m.name, &m.descriptor),
        location,
        Arc::clone(&class.name),
    ))
}

/// Add breakpoint `(class_id, method_key, location)` to VM `shared`'s row and
/// republish its debugger gates. `DUPLICATE` when present.
#[cfg(feature = "experimental-debug")]
pub(crate) fn add_breakpoint_key(
    shared: &crate::vm::SharedVm,
    class_id: u64,
    method_key: u64,
    location: u64,
) -> JvmtiResult<()> {
    {
        let mut guard = environments_write();
        let map = guard.get_or_insert_with(HashMap::new);
        let entry = map
            .entry(shared.vm_identity)
            .or_insert_with(VmJvmtiEnvironment::empty);
        if !entry.breakpoints.insert((class_id, method_key, location)) {
            return Err(JvmtiError::Duplicate);
        }
        refresh_breakpoint_union(map);
    }
    crate::debug::publish_debugger_gates(shared, &shared.debug.debug_state.lock());
    Ok(())
}

/// Remove breakpoint `(class_id, method_key, location)` from VM `shared`'s
/// row and republish its debugger gates. `NOT_FOUND` when absent.
#[cfg(feature = "experimental-debug")]
pub(crate) fn remove_breakpoint_key(
    shared: &crate::vm::SharedVm,
    class_id: u64,
    method_key: u64,
    location: u64,
) -> JvmtiResult<()> {
    {
        let mut guard = environments_write();
        let Some(map) = guard.as_mut() else {
            return Err(JvmtiError::NotFound);
        };
        let removed = map
            .get_mut(&shared.vm_identity)
            .is_some_and(|e| e.breakpoints.remove(&(class_id, method_key, location)));
        if !removed {
            return Err(JvmtiError::NotFound);
        }
        refresh_breakpoint_union(map);
    }
    crate::debug::publish_debugger_gates(shared, &shared.debug.debug_state.lock());
    Ok(())
}

/// Record `class_name`, the internal name of class `class_id`, for VM
/// `shared`'s JVMTI breakpoints (interpreter round i1 wave 38, lane L1;
/// `VmJvmtiEnvironment::breakpoint_class_names`), BEFORE the breakpoint is
/// added: the publication [`add_breakpoint_key`] ends with reads it to
/// withdraw only that class's compiled dependents. Kept whether or not the
/// breakpoint is then refused (a class's name does not change).
#[cfg(feature = "experimental-debug")]
fn note_breakpoint_class_name(shared: &crate::vm::SharedVm, class_id: u64, class_name: Arc<str>) {
    let mut guard = environments_write();
    let map = guard.get_or_insert_with(HashMap::new);
    map.entry(shared.vm_identity)
        .or_insert_with(VmJvmtiEnvironment::empty)
        .breakpoint_class_names
        .insert(class_id, class_name);
}

/// The internal name of class `class_id` of VM `vm` as its JVMTI breakpoint
/// recorded it (`VmJvmtiEnvironment::breakpoint_class_names`, interpreter
/// round i1 wave 38, lane L1), `None` when no JVMTI breakpoint named it.
/// Asked by `debug::publish_debugger_gates` for a class that gained a
/// breakpoint and that the JDWP session has no signature for; one read of
/// the registry, behind the one-load union flag.
#[cfg(feature = "experimental-debug")]
pub(crate) fn breakpoint_class_name_for_vm(vm: usize, class_id: u64) -> Option<Arc<str>> {
    if !UNION_BREAKPOINTS.load(Ordering::Acquire) {
        return None;
    }
    environments_read()
        .as_ref()
        .and_then(|m| m.get(&vm))
        .and_then(|e| e.breakpoint_class_names.get(&class_id).cloned())
}

/// `(class id, method key)` of every method holding one of VM `vm`'s JVMTI
/// breakpoints: folded into the per-method debugger gate by
/// `debug::publish_debugger_gates`. Empty behind one load when no VM has one.
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
pub(crate) fn breakpoint_methods_for_vm(vm: usize) -> HashSet<(u64, u64)> {
    if !UNION_BREAKPOINTS.load(Ordering::Acquire) {
        return HashSet::new();
    }
    environments_read()
        .as_ref()
        .and_then(|m| m.get(&vm))
        .map(|e| e.breakpoints.iter().map(|&(c, m, _)| (c, m)).collect())
        .unwrap_or_default()
}

/// Is a JVMTI breakpoint of VM `vm` set at bytecode `location` of the method
/// `method_key` of class `class_id`? One load when no VM has one; asked by
/// the dispatch loop's suspend point, which runs only in the methods the
/// debugger gate names.
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
#[inline]
pub(crate) fn breakpoint_at_for_vm(
    vm: usize,
    class_id: u64,
    method_key: u64,
    location: u64,
) -> bool {
    UNION_BREAKPOINTS.load(Ordering::Acquire)
        && environments_read()
            .as_ref()
            .and_then(|m| m.get(&vm))
            .is_some_and(|e| e.breakpoints.contains(&(class_id, method_key, location)))
}

/// Fire `Breakpoint` into `vm`'s environment only.
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
pub fn fire_breakpoint_for_vm(vm: usize, thread: ThreadId, method: MethodId, location: i64) {
    if let Some(m) = manager_for_vm(vm) {
        m.fire_breakpoint(thread, method, location);
    }
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
        m.any_exception_listener.store(false, Ordering::Release);
        m.any_exception_catch_listener
            .store(false, Ordering::Release);
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
                own_seen.fetch_add(1, Ordering::SeqCst);
            })),
            ..Default::default()
        })
        .unwrap();
        let env = Arc::new(JvmtiEnv::new());
        let inits = Arc::new(AtomicU32::new(0));
        let seen = inits.clone();
        env.event_manager
            .set_event_callbacks(EventCallbacks {
                vm_init: Some(Box::new(move || {
                    seen.fetch_add(1, Ordering::SeqCst);
                })),
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
                own_seen.fetch_add(1, Ordering::SeqCst);
            })),
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
                    seen.fetch_add(1, Ordering::SeqCst);
                })),
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
                    first_exit.lock().expect("first").push(address);
                }
                collect(&weak_exit);
            })),
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
                        later_exit.lock().expect("later").push(address);
                    }
                })),
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
        // VM identities no real `SharedVm` reaches, distinct from the other
        // test modules' bases.
        let vm = 0x7218_0000usize;
        let other_vm = vm + 1;
        install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
        install_manager_for_vm(other_vm, Arc::new(JvmtiEventManager::new_for_vm(other_vm)));
        let mgr = manager_for_vm(vm).expect("row was just installed");
        assert!(!interp_only_events_active_for_vm(vm));

        mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::Exception, None)
            .unwrap();
        assert!(any_exception_listener_active());
        assert_eq!(
            interp_only_events_active_for_vm(vm),
            EXCEPTION_EVENTS_NEED_THE_INTERPRETER,
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
            EXCEPTION_EVENTS_NEED_THE_INTERPRETER
        );
        mgr.unregister_env(&env).unwrap();
        assert!(!interp_only_events_active_for_vm(vm), "detached with it");

        forget_vm_jvmti_state(vm);
        forget_vm_jvmti_state(other_vm);
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
}
