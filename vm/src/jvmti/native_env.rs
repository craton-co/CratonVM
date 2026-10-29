// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The C `jvmtiEnv` a native agent reaches through `GetEnv` (interpreter
//! round i1 wave 12, lane L1; the lifecycle events, `Exception`, per-thread
//! enabling and the class functions in wave 13).
//!
//! Until wave 12 there was no C JVMTI function table at all: `GetEnv` for a
//! JVMTI version answered `JNI_EVERSION` (wave 11), so an `-agentpath:` agent
//! could negotiate nothing and set no breakpoint, although the VM keeps
//! per-VM JVMTI breakpoints and delivers them from the dispatch loop's
//! suspend point (`runtime::jvmti::set_breakpoint_for_vm`, wave 10).
//!
//! `jvmtiEnv` is, like `JNIEnv`, a pointer to a pointer to the function
//! table: [`JvmtiNativeEnv`] starts with that pointer. The table has JDK 25's
//! 156 slots (`jvmtiInterface_1_`, function 1 `reserved1` through 156
//! `SetHeapSamplingInterval`). Since round i2 wave 63 (lane L1) every slot
//! but HotSpot's reserved ones is served, by area: this file (the thread,
//! frame, local, class, method, breakpoint, capability, property and
//! class-search functions, waves 12-61), `agent_functions` (agent threads,
//! interrupt / stop, monitor usage, constant pool, source debug extension,
//! native prefixes, JNI function table, verbose flags, all-virtual-thread
//! suspension, heap sampling interval), `modules` (the module functions),
//! `heap` (tags and the heap walks), `extensions` (HotSpot's three extension
//! functions and three extension events) and `events_more` (the field
//! watches and the events posted outside the dispatch loop: monitor, GC,
//! `ObjectFree`, `VMObjectAlloc`, `SampledObjectAlloc`, `NativeMethodBind`,
//! `ResourceExhausted`, `DataDumpRequest`, virtual threads, `ClassUnload`,
//! compiled code). The history of each slot is on the page
//! `docs/feature-designs/jvmti-delivery-threading.md`.
//!
//! Capabilities follow HotSpot's model (wave 63): the always-available set,
//! the always-solo set (one env at a time), the OnLoad set and the OnLoad-solo
//! set, which only a startup env may acquire, and which it keeps as potential
//! in the live phase (`onload_acquired`); a solo capability another env holds
//! is not potential. The class-hook, retransform, redefine and frame-control
//! capabilities are potential only with their switches on, so an agent never
//! gets a capability whose events never arrive (the D14 rule,
//! once in the older `jvmti/capabilities.rs`, deleted in wave 125).
//!
//! Delivery, always on the thread the event happens on, with a `JNIEnv*`,
//! the thread's `java.lang.Thread` as a local reference (in a local frame
//! popped after the callback) and the thread's JNI context installed, so the
//! callback can call JNI ([`in_event_context`]):
//!
//! * `Breakpoint`, `Exception`, (wave 17) `MethodEntry` / `MethodExit`,
//!   (wave 22) `SingleStep`, (wave 45) `FramePop` and (wave 52)
//!   `ExceptionCatch`
//!   through the VM's `JvmtiEventManager`: the
//!   env registers a Rust-side `runtime::jvmti::JvmtiEnv` on it the first
//!   time it enables one of them, whose closures call the agent's callbacks
//!   (the manager's attached-env loop). The env mirrors its enable state
//!   into that delivery env's own state (wave 14,
//!   [`JvmtiNativeEnv::sync_event`]), which is what the manager's union
//!   flags and delivery read, so a disable or a dispose takes the event off
//!   the VM.
//! * `ClassPrepare` once per class, when the class is linked and prepared
//!   (by its initialization or, wave 17, as a supertype of a class being
//!   linked, which reports the supertype first: `vm::link_class_on_thread`),
//!   before any of its code runs (`debug::class_prepared_on_thread`, behind one per-VM load,
//!   `DebuggerGates::native_class_prepare_armed`; wave 15 moved preparation
//!   ahead of the superclass). A compile door does not link a class early
//!   while the event is enabled (`vm::link_class_without_java`).
//! * `VMInit` once per VM, on the main thread, when the VM enters the live
//!   phase at `SharedVm::set_init_level(3)` ([`enter_live_phase`], wave 14).
//!   The envs a startup agent obtained in `Agent_OnLoad`
//!   ([`collect_startup_envs`]) are bound to the VM by `Vm::new`
//!   ([`bind_startup_envs`]) and are in the start phase until then.
//! * (Round i2 wave 55, with `DebuggerGates::c_jvmti_phases`) the VM is in
//!   the primordial phase from that binding until `System.initPhase1`
//!   returns, with no class event; `VMStart` once per VM, on the main thread,
//!   when it enters the start phase at `SharedVm::set_init_level(2)`
//!   ([`enter_start_phase`]); the dead phase after `VMDeath`.
//! * `VMDeath` when the `Vm` is dropped, followed by the agents'
//!   `Agent_OnUnload` ([`shut_down_agents`]).
//!
//! A `jthread` or `jclass` an agent passes is decoded inside the JNI
//! functions' foreign-thread entry (`jni::ForeignJniEntry`, wave 19;
//! [`thread_id_of`], [`class_of`]), so an agent's own idle attached thread
//! reads no handle while a collection may move its object.
//!
//! An env is never freed: `DisposeEnvironment` clears its breakpoints,
//! unregisters its delivery and marks it disposed (every later call answers
//! `JVMTI_ERROR_INVALID_ENVIRONMENT`), so a callback in flight never reads
//! freed memory. The cost is one small allocation per `GetEnv` for the life
//! of the process.

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::{Arc, Weak};

use crate::classloading::ClassId;
use crate::native::jni::{self, JInt, JNIEnv, JObject};
use crate::runtime::interpreter::BlockedRow;
use crate::runtime::jvmti::{self as rt, EventCallbacks, JvmtiEventKind};
use crate::runtime::lambda_proxy::{LambdaProxyFacts, LambdaProxyMethod};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::types::{ObjectRef, Value};
use crate::vm::SharedVm;
use cratonvm_native_api::{NativeClassAccess as _, NativeHeapAccess as _, NativeSystemAccess as _};

// Interpreter round i2 wave 63, lane L1: the slots and events the table
// still answered with `JVMTI_ERROR_NOT_AVAILABLE` (the i51-L1 page's last
// items), by area.
mod agent_functions;
pub(crate) use agent_functions::{
    all_native_method_prefixes, run_agent_thread_body, take_pending_agent_body, AgentThreadBody,
};
mod events_more;
pub(crate) use events_more::{
    note_unloaded_classes, post_data_dump_request, post_field_watch, post_gc, post_monitor_event,
    post_native_method_bind, post_resource_exhausted, post_rust_native_method_bind,
    post_virtual_thread, post_vm_object_alloc, sample_allocation, FieldEvent, MonitorEvent,
    RustBindTracking, VirtualThreadEvent,
};
// j27 wave 5 (lane JVMTIFRAME): a compiled / OSR'd frame's locals from its
// home slots (`CRATONVM_JVMTI_C_COMPILED_FRAME_LOCALS`, default OFF).
mod compiled_locals;
mod extensions;
mod heap;
// Interpreter round i5 wave 103 (lane L1): the JDWP heap walks
// (`debug::inspect`, `canGetInstanceInfo`).
// Wave 104: `debugger_heap_scan` (`ClassesBySignature` of an array).
pub(crate) use heap::{
    debugger_heap_query, debugger_heap_scan, DebuggerHeapAnswer, DebuggerHeapQuery,
};
mod modules;

/// Slots in JDK 25's `jvmtiInterface_1_`: function 1 (`reserved1`) through
/// 156 (`SetHeapSamplingInterval`). A property of the header an agent was
/// compiled against, as `jni::JNI_FUNCTION_COUNT` is of `jni.h`.
const JVMTI_FUNCTION_COUNT: usize = 156;

/// `JVMTI_VERSION` of JDK 25's `jvmti.h` (`GetVersionNumber`).
const JVMTI_VERSION_25: JInt = 0x3019_0000;

// The `jvmtiError` values this table answers.
const ERR_NONE: JInt = 0;
const ERR_INVALID_THREAD: JInt = 10;
const ERR_THREAD_NOT_SUSPENDED: JInt = 13;
const ERR_THREAD_SUSPENDED: JInt = 14;
const ERR_NO_MORE_FRAMES: JInt = 31;
const ERR_OPAQUE_FRAME: JInt = 32;
const ERR_TYPE_MISMATCH: JInt = 34;
const ERR_INVALID_SLOT: JInt = 35;
const ERR_DUPLICATE: JInt = 40;
const ERR_THREAD_NOT_ALIVE: JInt = 15;
const ERR_INVALID_OBJECT: JInt = 20;
const ERR_INVALID_CLASS: JInt = 21;
const ERR_INVALID_METHODID: JInt = 23;
/// `JVMTI_ERROR_UNMODIFIABLE_CLASS` (wave 54, `RetransformClasses`).
const ERR_UNMODIFIABLE_CLASS: JInt = 79;
/// `JVMTI_ERROR_UNSUPPORTED_OPERATION` (wave 58, the CPU time of a virtual
/// thread).
const ERR_UNSUPPORTED_OPERATION: JInt = 73;
const ERR_NOT_AVAILABLE: JInt = 98;
const ERR_MUST_POSSESS_CAPABILITY: JInt = 99;
const ERR_NULL_POINTER: JInt = 100;
const ERR_ABSENT_INFORMATION: JInt = 101;
const ERR_INVALID_EVENT_TYPE: JInt = 102;
const ERR_ILLEGAL_ARGUMENT: JInt = 103;
const ERR_OUT_OF_MEMORY: JInt = 110;
/// `JVMTI_ERROR_NATIVE_METHOD` (104; 111 until wave 57, which is
/// `JVMTI_ERROR_ACCESS_DENIED`: MEASURED, `L1W57JvmtiLocalVariableTable`).
const ERR_NATIVE_METHOD: JInt = 104;
const ERR_WRONG_PHASE: JInt = 112;
const ERR_INTERNAL: JInt = 113;
const ERR_UNATTACHED_THREAD: JInt = 115;
const ERR_INVALID_ENVIRONMENT: JInt = 116;

/// `sizeof(jvmtiCapabilities)` is 16 bytes: bit fields packed from the low
/// bit of the first `unsigned int`, in declaration order.
const CAPABILITY_WORDS: usize = 4;
/// `can_get_line_numbers`, the 13th bit field (`GetLineNumberTable`, wave 17).
const CAN_GET_LINE_NUMBERS: u32 = 1 << 12;
/// `can_generate_single_step_events`, the 17th bit field (wave 22).
const CAN_GENERATE_SINGLE_STEP_EVENTS: u32 = 1 << 16;
/// `can_generate_exception_events`, the 18th bit field.
const CAN_GENERATE_EXCEPTION_EVENTS: u32 = 1 << 17;
/// `can_generate_frame_pop_events`, the 19th bit field (interpreter round
/// i1 wave 45, lane L1: `NotifyFramePop` and the `FramePop` event; potential
/// in the live phase too, as on HotSpot 25.0.3, measured).
const CAN_GENERATE_FRAME_POP_EVENTS: u32 = 1 << 18;
/// `can_generate_breakpoint_events`, the 20th bit field.
const CAN_GENERATE_BREAKPOINT_EVENTS: u32 = 1 << 19;
/// `can_generate_method_entry_events`, the 25th bit field (wave 17).
const CAN_GENERATE_METHOD_ENTRY_EVENTS: u32 = 1 << 24;
/// `can_generate_method_exit_events`, the 26th bit field (wave 17).
const CAN_GENERATE_METHOD_EXIT_EVENTS: u32 = 1 << 25;
/// `can_get_current_contended_monitor`, the 7th bit field (interpreter
/// round i1 wave 44, lane L1: `GetCurrentContendedMonitor`).
const CAN_GET_CURRENT_CONTENDED_MONITOR: u32 = 1 << 6;
/// `can_get_owned_monitor_info`, the 6th bit field (interpreter round i1
/// wave 51, lane L1: `GetOwnedMonitorInfo`). HotSpot groups it with the
/// three wave-44 monitor and local capabilities among its OnLoad ones
/// (`JvmtiManageCapabilities::init_onload_capabilities`), so it is
/// [`ONLOAD_ONLY`] here too.
const CAN_GET_OWNED_MONITOR_INFO: u32 = 1 << 5;
/// `can_access_local_variables`, the 15th bit field (wave 44: the
/// `GetLocal*` / `SetLocal*` functions and `GetLocalInstance`).
const CAN_ACCESS_LOCAL_VARIABLES: u32 = 1 << 14;
/// `can_get_owned_monitor_stack_depth_info`, the 35th bit field: bit 2 of
/// the second word (wave 44: `GetOwnedMonitorStackDepthInfo`).
const CAN_GET_OWNED_MONITOR_STACK_DEPTH_INFO: u32 = 1 << 2;
/// `can_retransform_classes`, the 38th bit field: bit 5 of the second word
/// (interpreter round i2 wave 54, lane L1: `RetransformClasses` and
/// `ClassFileLoadHook` at a retransformation; potential in the OnLoad and
/// the live phase while `DebuggerGates::c_retransform` is on).
const CAN_RETRANSFORM_CLASSES: u32 = 1 << 5;
/// `can_redefine_classes`, the 10th bit field (interpreter round i2 wave 57,
/// lane L1: `RedefineClasses`; potential in the OnLoad and the live phase
/// while `DebuggerGates::c_redefine` is on).
const CAN_REDEFINE_CLASSES: u32 = 1 << 9;
/// `can_generate_all_class_hook_events`, the 27th bit field (interpreter
/// round i2 wave 55, lane L1): potential in the OnLoad and the live phase
/// while `DebuggerGates::c_class_hooks` is on, as HotSpot lists it in both
/// (measured, `L1W54JvmtiRetransform`'s first port run and
/// `L1W55JvmtiPhases`). The hook is offered every class file the load path
/// offers (`runtime::instrument::run_chain_over_bytes`) with or without it;
/// a C instrumentation agent that asks for it in `AddCapabilities` was
/// refused all its capabilities (98) until wave 55.
const CAN_GENERATE_ALL_CLASS_HOOK_EVENTS: u32 = 1 << 26;
/// `can_pop_frame`, the 9th bit field (interpreter round i2 wave 61, lane
/// L1: `PopFrame`; potential while `DebuggerGates::c_frame_control` is on).
const CAN_POP_FRAME: u32 = 1 << 8;
/// `can_force_early_return`, the 34th bit field: bit 1 of the second word
/// (wave 61: `ForceEarlyReturn*`; as [`CAN_POP_FRAME`]).
const CAN_FORCE_EARLY_RETURN: u32 = 1 << 1;
/// `can_suspend`, the 21st bit field (interpreter round i1 wave 45, lane
/// L1: `SuspendThread`, `ResumeThread`, `SuspendThreadList`,
/// `ResumeThreadList`).
const CAN_SUSPEND: u32 = 1 << 20;
/// `jvmtiCapabilities` bit field `n` (0-based, declaration order) as a
/// capability set.
const fn cap(n: usize) -> [u32; CAPABILITY_WORDS] {
    let mut set = [0; CAPABILITY_WORDS];
    set[n / 32] = 1 << (n % 32);
    set
}

/// The union of `sets`.
const fn caps_of(sets: &[[u32; CAPABILITY_WORDS]]) -> [u32; CAPABILITY_WORDS] {
    let mut out = [0; CAPABILITY_WORDS];
    let mut i = 0;
    while i < sets.len() {
        let mut w = 0;
        while w < CAPABILITY_WORDS {
            out[w] |= sets[i][w];
            w += 1;
        }
        i += 1;
    }
    out
}

// The capability bit fields (0-based, `jvmtiCapabilities` declaration order)
// the interpreter round i2 wave 63 rows name (the i51-L1 page's last items).
const CAP_TAG_OBJECTS: usize = 0;
const CAP_FIELD_MODIFICATION_EVENTS: usize = 1;
const CAP_FIELD_ACCESS_EVENTS: usize = 2;
const CAP_GET_MONITOR_INFO: usize = 7;
const CAP_SIGNAL_THREAD: usize = 10;
const CAP_GET_SOURCE_DEBUG_EXTENSION: usize = 13;
const CAP_MAINTAIN_ORIGINAL_METHOD_ORDER: usize = 15;
const CAP_REDEFINE_ANY_CLASS: usize = 21;
const CAP_COMPILED_METHOD_LOAD_EVENTS: usize = 27;
const CAP_MONITOR_EVENTS: usize = 28;
const CAP_VM_OBJECT_ALLOC_EVENTS: usize = 29;
const CAP_NATIVE_METHOD_BIND_EVENTS: usize = 30;
const CAP_GARBAGE_COLLECTION_EVENTS: usize = 31;
const CAP_OBJECT_FREE_EVENTS: usize = 32;
const CAP_GET_CONSTANT_POOL: usize = 35;
const CAP_SET_NATIVE_METHOD_PREFIX: usize = 36;
const CAP_RETRANSFORM_ANY_CLASS: usize = 38;
const CAP_RESOURCE_EXHAUSTION_HEAP_EVENTS: usize = 39;
const CAP_RESOURCE_EXHAUSTION_THREADS_EVENTS: usize = 40;
const CAP_EARLY_VMSTART: usize = 41;
const CAP_EARLY_CLASS_HOOK_EVENTS: usize = 42;
const CAP_SAMPLED_OBJECT_ALLOC_EVENTS: usize = 43;
const CAP_SUPPORT_VIRTUAL_THREADS: usize = 44;

/// Does `set` hold capability bit field `n`?
fn has_cap(set: &[u32; CAPABILITY_WORDS], n: usize) -> bool {
    set[n / 32] & (1 << (n % 32)) != 0
}

/// HotSpot 25's capability sets (`JvmtiManageCapabilities`), MEASURED with
/// `L1W63JvmtiAgentFunctions`' `caps` rows and a live env next to a startup
/// agent that added nothing (interpreter round i2 wave 63, lane L1):
///
/// * [`ALWAYS`]: potential in the OnLoad and the live phase;
/// * [`ALWAYS_SOLO`]: the same, but one env at a time (`can_suspend`,
///   `can_generate_sampled_object_alloc_events`);
/// * [`ONLOAD`]: potential in the OnLoad phase, and in the live phase once a
///   startup env acquired them (for good: HotSpot moves an acquired OnLoad
///   capability into its always set);
/// * [`ONLOAD_SOLO`]: as [`ONLOAD`], one env at a time (the field watch
///   events and `can_generate_breakpoint_events`).
///
/// Until wave 63 the table's live set held the event capabilities of
/// [`ONLOAD`] / [`ONLOAD_SOLO`] (breakpoints, steps, exceptions, method
/// events) whether or not a startup agent had acquired them, and none of the
/// capabilities of the functions it did not answer.
const ALWAYS: [u32; CAPABILITY_WORDS] = caps_of(&[
    cap(CAP_TAG_OBJECTS),
    [CAN_GET_BYTECODES, 0, 0, 0],
    [CAN_GET_SYNTHETIC_ATTRIBUTE, 0, 0, 0],
    cap(CAP_GET_MONITOR_INFO),
    [CAN_REDEFINE_CLASSES, 0, 0, 0],
    cap(CAP_SIGNAL_THREAD),
    [CAN_GET_SOURCE_FILE_NAME, 0, 0, 0],
    [CAN_GET_LINE_NUMBERS, 0, 0, 0],
    cap(CAP_REDEFINE_ANY_CLASS),
    [CAN_GET_CURRENT_THREAD_CPU_TIME, 0, 0, 0],
    [CAN_GET_THREAD_CPU_TIME, 0, 0, 0],
    [CAN_GENERATE_ALL_CLASS_HOOK_EVENTS, 0, 0, 0],
    cap(CAP_COMPILED_METHOD_LOAD_EVENTS),
    cap(CAP_MONITOR_EVENTS),
    cap(CAP_VM_OBJECT_ALLOC_EVENTS),
    cap(CAP_NATIVE_METHOD_BIND_EVENTS),
    cap(CAP_GARBAGE_COLLECTION_EVENTS),
    cap(CAP_OBJECT_FREE_EVENTS),
    cap(CAP_GET_CONSTANT_POOL),
    cap(CAP_SET_NATIVE_METHOD_PREFIX),
    [0, CAN_RETRANSFORM_CLASSES, 0, 0],
    cap(CAP_RETRANSFORM_ANY_CLASS),
    cap(CAP_RESOURCE_EXHAUSTION_HEAP_EVENTS),
    cap(CAP_RESOURCE_EXHAUSTION_THREADS_EVENTS),
    cap(CAP_SUPPORT_VIRTUAL_THREADS),
]);
/// See [`ALWAYS`].
const ALWAYS_SOLO: [u32; CAPABILITY_WORDS] =
    caps_of(&[[CAN_SUSPEND, 0, 0, 0], cap(CAP_SAMPLED_OBJECT_ALLOC_EVENTS)]);
/// See [`ALWAYS`].
const ONLOAD: [u32; CAPABILITY_WORDS] = caps_of(&[
    [CAN_GET_OWNED_MONITOR_INFO, 0, 0, 0],
    [CAN_GET_CURRENT_CONTENDED_MONITOR, 0, 0, 0],
    [CAN_POP_FRAME, 0, 0, 0],
    cap(CAP_GET_SOURCE_DEBUG_EXTENSION),
    [CAN_ACCESS_LOCAL_VARIABLES, 0, 0, 0],
    cap(CAP_MAINTAIN_ORIGINAL_METHOD_ORDER),
    [CAN_GENERATE_SINGLE_STEP_EVENTS, 0, 0, 0],
    [CAN_GENERATE_EXCEPTION_EVENTS, 0, 0, 0],
    [CAN_GENERATE_FRAME_POP_EVENTS, 0, 0, 0],
    [CAN_GENERATE_METHOD_ENTRY_EVENTS, 0, 0, 0],
    [CAN_GENERATE_METHOD_EXIT_EVENTS, 0, 0, 0],
    [0, CAN_FORCE_EARLY_RETURN, 0, 0],
    [0, CAN_GET_OWNED_MONITOR_STACK_DEPTH_INFO, 0, 0],
    cap(CAP_EARLY_VMSTART),
    cap(CAP_EARLY_CLASS_HOOK_EVENTS),
]);
/// See [`ALWAYS`].
const ONLOAD_SOLO: [u32; CAPABILITY_WORDS] = caps_of(&[
    cap(CAP_FIELD_MODIFICATION_EVENTS),
    cap(CAP_FIELD_ACCESS_EVENTS),
    [CAN_GENERATE_BREAKPOINT_EVENTS, 0, 0, 0],
]);
/// The capabilities one env at a time may hold.
const SOLO: [u32; CAPABILITY_WORDS] = caps_of(&[ALWAYS_SOLO, ONLOAD_SOLO]);
/// What a startup env may acquire that the live phase then offers: the
/// OnLoad sets (wave 46 measured three of them; wave 63 the rest).
const ONLOAD_ONLY: [u32; CAPABILITY_WORDS] = caps_of(&[ONLOAD, ONLOAD_SOLO]);

// The `jvmtiThreadState` bits `GetThreadState` answers (wave 45).
const THREAD_STATE_ALIVE: JInt = 0x0001;
const THREAD_STATE_TERMINATED: JInt = 0x0002;
const THREAD_STATE_RUNNABLE: JInt = 0x0004;
const THREAD_STATE_WAITING_INDEFINITELY: JInt = 0x0010;
const THREAD_STATE_WAITING_WITH_TIMEOUT: JInt = 0x0020;
const THREAD_STATE_WAITING: JInt = 0x0080;
const THREAD_STATE_SLEEPING: JInt = 0x0040;
const THREAD_STATE_IN_OBJECT_WAIT: JInt = 0x0100;
const THREAD_STATE_PARKED: JInt = 0x0200;
const THREAD_STATE_BLOCKED_ON_MONITOR_ENTER: JInt = 0x0400;
const THREAD_STATE_SUSPENDED: JInt = 0x0010_0000;
const THREAD_STATE_INTERRUPTED: JInt = 0x0020_0000;
const THREAD_STATE_IN_NATIVE: JInt = 0x0040_0000;

/// Everything this table can grant: every set of [`ALWAYS`]'s model (what
/// a lone env is offered in the OnLoad phase).
const POTENTIAL: [u32; CAPABILITY_WORDS] = caps_of(&[ALWAYS, ALWAYS_SOLO, ONLOAD, ONLOAD_SOLO]);

/// `JVM_RECOGNIZED_METHOD_MODIFIERS`: what `GetMethodModifiers` answers of a
/// method's access flags (public through synthetic, without the bits the
/// class-file format reserves).
const RECOGNIZED_METHOD_MODIFIERS: u16 = 0x1DFF;
/// `JVMTI_CLASS_STATUS_ARRAY` / `JVMTI_CLASS_STATUS_PRIMITIVE`; the other
/// `jvmtiClassStatus` bits are JDWP's (`debug::jdwp_class_status`).
const CLASS_STATUS_ARRAY: JInt = 16;
const CLASS_STATUS_PRIMITIVE: JInt = 32;

/// `jvmtiLineNumberEntry`: `{ jlocation start_location; jint line_number; }`,
/// 16 bytes with its padding.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LineNumberRow {
    start_location: i64,
    line_number: JInt,
}

/// `jvmtiLocalVariableEntry`: `{ jlocation start_location; jint length;
/// char* name; char* signature; char* generic_signature; jint slot; }`, 48
/// bytes with its padding (interpreter round i2 wave 57).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LocalVariableRow {
    start_location: i64,
    length: JInt,
    name: *mut u8,
    signature: *mut u8,
    generic_signature: *mut u8,
    slot: JInt,
}

/// `jvmtiFrameInfo`: `{ jmethodID method; jlocation location; }`, 16 bytes
/// (wave 22).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FrameInfo {
    method: u64,
    location: i64,
}

/// `JVMTI_MIN_EVENT_TYPE_VAL` .. `JVMTI_MAX_EVENT_TYPE_VAL` of JDK 25
/// (`VirtualThreadEnd` is the last).
const MIN_EVENT: JInt = 50;
const MAX_EVENT: JInt = 88;
/// HotSpot's first extension event number (`EXT_MIN_EVENT_TYPE_VAL`: the
/// three below [`MIN_EVENT`], `VirtualThreadUnmount`, `VirtualThreadMount`,
/// `ClassUnload`), the low end of what `SetEventNotificationMode` accepts
/// (interpreter round i2 wave 61, lane L1; MEASURED).
const MIN_EXTENSION_EVENT: JInt = 47;
/// The event numbers `jvmti.h` leaves reserved in the range (wave 61).
const RESERVED_EVENTS: [JInt; 5] = [72, 77, 78, 79, 85];
// The `jvmtiEvent` numbers of the events this table delivers (`jvmti.h`).
// Until wave 13 `Breakpoint` was taken as 60 — `SingleStep` — and its
// callback read from `SingleStep`'s member of `jvmtiEventCallbacks`, so an
// agent compiled against `jvmti.h` could not enable it.
const EVENT_VM_INIT: JInt = 50;
const EVENT_VM_DEATH: JInt = 51;
/// `ThreadStart` / `ThreadEnd` (interpreter round i1 wave 52, lane L1).
const EVENT_THREAD_START: JInt = 52;
const EVENT_THREAD_END: JInt = 53;
/// `ClassFileLoadHook` / `ClassLoad` (interpreter round i2 wave 53, lane L1,
/// behind `DebuggerGates::c_class_hooks`, `CRATONVM_JVMTI_C_CLASS_HOOKS`).
const EVENT_CLASS_FILE_LOAD_HOOK: JInt = 54;
const EVENT_CLASS_LOAD: JInt = 55;
const EVENT_CLASS_PREPARE: JInt = 56;
/// `VMStart` (interpreter round i2 wave 55, lane L1, behind
/// `DebuggerGates::c_jvmti_phases`, `CRATONVM_JVMTI_C_PHASES`).
const EVENT_VM_START: JInt = 57;
const EVENT_EXCEPTION: JInt = 58;
/// Interpreter round i1 wave 52, lane L1 (behind
/// `DebuggerGates::c_exception_catch`, `CRATONVM_JVMTI_C_EXCEPTION_CATCH`).
const EVENT_EXCEPTION_CATCH: JInt = 59;
const EVENT_SINGLE_STEP: JInt = 60;
const EVENT_FRAME_POP: JInt = 61;
const EVENT_BREAKPOINT: JInt = 62;
const EVENT_METHOD_ENTRY: JInt = 65;
const EVENT_METHOD_EXIT: JInt = 66;
/// The events delivered through the env's delivery env on the VM's manager
/// ([`JvmtiNativeEnv::sync_event`]), each with the capability it needs.
const MANAGER_EVENTS: [(JInt, u32); 7] = [
    (EVENT_BREAKPOINT, CAN_GENERATE_BREAKPOINT_EVENTS),
    (EVENT_SINGLE_STEP, CAN_GENERATE_SINGLE_STEP_EVENTS),
    (EVENT_EXCEPTION, CAN_GENERATE_EXCEPTION_EVENTS),
    // Interpreter round i1 wave 52, lane L1.
    (EVENT_EXCEPTION_CATCH, CAN_GENERATE_EXCEPTION_EVENTS),
    (EVENT_METHOD_ENTRY, CAN_GENERATE_METHOD_ENTRY_EVENTS),
    (EVENT_METHOD_EXIT, CAN_GENERATE_METHOD_EXIT_EVENTS),
    // Interpreter round i1 wave 45, lane L1.
    (EVENT_FRAME_POP, CAN_GENERATE_FRAME_POP_EVENTS),
];
/// `jvmtiEventCallbacks` holds one pointer per event from `VMInit` (50) on,
/// in event-number order; this copy has room for more than JDK 25 declares.
const CALLBACK_SLOTS: usize = 48;

/// `void (JNICALL *VMInit)(jvmtiEnv*, JNIEnv*, jthread)`.
type VmInitCallback = unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject);
/// `void (JNICALL *VMDeath)(jvmtiEnv*, JNIEnv*)`.
type VmDeathCallback = unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv);
/// `void (JNICALL *ClassPrepare)(jvmtiEnv*, JNIEnv*, jthread, jclass)`.
type ClassPrepareCallback = unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject, JObject);
/// `void (JNICALL *Exception)(jvmtiEnv*, JNIEnv*, jthread, jmethodID,
/// jlocation, jobject exception, jmethodID catch_method, jlocation
/// catch_location)`.
type ExceptionCallback =
    unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject, u64, i64, JObject, u64, i64);
/// `void (JNICALL *ExceptionCatch)(jvmtiEnv*, JNIEnv*, jthread, jmethodID,
/// jlocation, jobject exception)` (wave 52).
type ExceptionCatchCallback =
    unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject, u64, i64, JObject);
/// `void (JNICALL *Breakpoint)(jvmtiEnv*, JNIEnv*, jthread, jmethodID,
/// jlocation)`.
type BreakpointCallback = unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject, u64, i64);
/// `void (JNICALL *MethodEntry)(jvmtiEnv*, JNIEnv*, jthread, jmethodID)`.
type MethodEntryCallback = unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject, u64);
/// `void (JNICALL *MethodExit)(jvmtiEnv*, JNIEnv*, jthread, jmethodID,
/// jboolean was_popped_by_exception, jvalue return_value)`. The 8-byte
/// `jvalue` union goes in an integer register under both 64-bit C calling
/// conventions (SysV merges its integer and floating members to INTEGER;
/// Windows passes an 8-byte aggregate by value in a register), so it is a
/// `u64` here.
type MethodExitCallback = unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject, u64, u8, u64);
/// `void (JNICALL *FramePop)(jvmtiEnv*, JNIEnv*, jthread, jmethodID,
/// jboolean was_popped_by_exception)` (wave 45).
type FramePopCallback = unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject, u64, u8);

/// A native agent's `jvmtiEnv`. `#[repr(C)]` with the table pointer first:
/// an agent calls `(*env)->SetBreakpoint(env, ...)`.
#[repr(C)]
pub(crate) struct JvmtiNativeEnv {
    functions: *const usize,
    state: parking_lot::Mutex<EnvState>,
    /// Serialises mirroring the env's `Breakpoint` / `Exception` state into
    /// its delivery env ([`JvmtiNativeEnv::sync_event`]) and disposing it,
    /// so two agent threads cannot publish their changes out of order. Never
    /// taken by a delivery.
    sync: parking_lot::Mutex<()>,
}

/// Everything an env holds, under one lock (agents may call from any thread).
struct EnvState {
    /// The capabilities this env possesses.
    capabilities: [u32; CAPABILITY_WORDS],
    /// The agent's `jvmtiEventCallbacks`, copied (`SetEventCallbacks`).
    callbacks: [usize; CALLBACK_SLOTS],
    /// Bit `event - MIN_EVENT` of every event enabled for all threads.
    enabled: u64,
    /// `(event, thread id)` of every event enabled for one thread.
    thread_enabled: Vec<(JInt, u64)>,
    /// The VM this env acts on, once one was in hand. Never replaced: an env
    /// whose VM is gone acts on none.
    vm: Option<Weak<SharedVm>>,
    /// Obtained by a startup agent in `Agent_OnLoad` and not bound yet: it
    /// acts on the VM being built, which `Vm::new` binds it to
    /// ([`bind_startup_envs`]), never on another VM the thread can reach.
    startup_pending: bool,
    /// The delivery registered on that VM's manager.
    delivery: Option<(Arc<rt::JvmtiEventManager>, Arc<rt::JvmtiEnv>)>,
    /// The breakpoints this env set, cleared when it is disposed.
    breakpoints: Vec<(u64, i64)>,
    /// `SetEnvironmentLocalStorage`'s pointer (wave 52).
    local_storage: usize,
    /// `SetThreadLocalStorage`'s pointers, by VM thread id (wave 52).
    thread_storage: Vec<(u64, usize)>,
    /// `SetExtensionEventCallback`'s callbacks of the extension events 47-49
    /// (interpreter round i2 wave 63, lane L1; `extensions`).
    ext_callbacks: [usize; 3],
    /// `SetNativeMethodPrefix(es)`'s prefixes, in the agent's order (wave
    /// 63).
    native_prefixes: Vec<String>,
    /// The tags this env set on objects (wave 63; `heap`).
    tags: heap::TagTable,
    /// zt t23/x19 (review page item 28e): a startup env's VM's effective
    /// class-unloading bit, copied from its OnLoad scope
    /// ([`StartupScope::class_unloading`]); what `IsClassUnloadingEnabled`
    /// answers before the env has a VM. `None` for a live-phase env.
    onload_class_unloading: Option<bool>,
    disposed: bool,
}

impl EnvState {
    /// Is `event` enabled for thread `tid` (globally or for that thread)?
    fn enabled_for(&self, event: JInt, tid: u64) -> bool {
        self.enabled & event_bit(event) != 0 || self.thread_enabled.contains(&(event, tid))
    }

    /// Is `event` enabled for any thread?
    fn enabled_anywhere(&self, event: JInt) -> bool {
        self.enabled & event_bit(event) != 0 || self.thread_enabled.iter().any(|&(e, _)| e == event)
    }

    /// Disable `event` globally and for every thread.
    fn disable_everywhere(&mut self, event: JInt) {
        self.enabled &= !event_bit(event);
        self.thread_enabled.retain(|&(e, _)| e != event);
    }
}

/// The bit of `event` in [`EnvState::enabled`]; 0 outside the event range.
fn event_bit(event: JInt) -> u64 {
    // Wave 63: from the extension events (47) on, so they enable too.
    u32::try_from(event - MIN_EXTENSION_EVENT)
        .ok()
        .and_then(|shift| 1u64.checked_shl(shift))
        .unwrap_or(0)
}

/// The capability an event this table delivers needs (0: none), or `None`
/// for an event it does not deliver.
fn required_capability(event: JInt) -> Option<u32> {
    match event {
        EVENT_VM_INIT | EVENT_VM_DEATH | EVENT_CLASS_PREPARE => Some(0),
        // Wave 55, while the VM's switch is on ([`set_event_notification_mode`]).
        EVENT_VM_START => Some(0),
        EVENT_THREAD_START | EVENT_THREAD_END => Some(0),
        // Wave 53: neither needs a capability (an env without
        // `can_retransform_classes` is not offered a retransformation, wave
        // 54).
        EVENT_CLASS_FILE_LOAD_HOOK | EVENT_CLASS_LOAD => Some(0),
        EVENT_EXCEPTION | EVENT_EXCEPTION_CATCH => Some(CAN_GENERATE_EXCEPTION_EVENTS),
        EVENT_BREAKPOINT => Some(CAN_GENERATE_BREAKPOINT_EVENTS),
        EVENT_SINGLE_STEP => Some(CAN_GENERATE_SINGLE_STEP_EVENTS),
        EVENT_METHOD_ENTRY => Some(CAN_GENERATE_METHOD_ENTRY_EVENTS),
        EVENT_METHOD_EXIT => Some(CAN_GENERATE_METHOD_EXIT_EVENTS),
        EVENT_FRAME_POP => Some(CAN_GENERATE_FRAME_POP_EVENTS),
        _ => None,
    }
}

/// [`required_capability`] as a capability set, with the events of
/// interpreter round i2 wave 63 (`events_more::event_capability`), whose
/// capabilities are not all in the first word.
fn event_capability_set(event: JInt) -> Option<[u32; CAPABILITY_WORDS]> {
    required_capability(event)
        .map(|word0| [word0, 0, 0, 0])
        .or_else(|| events_more::event_capability(event))
}

thread_local! {
    /// While a VM loads its startup agents on this thread
    /// ([`collect_startup_envs`]), the envs their `GetEnv` calls hand out,
    /// and (wave 61) the VM properties of the OnLoad phase and what the
    /// agents did to the VM being built.
    static STARTUP_ENVS: RefCell<Option<StartupScope>> = const { RefCell::new(None) };
}

/// The OnLoad phase of a VM being built ([`STARTUP_ENVS`]).
#[derive(Default)]
struct StartupScope {
    envs: Vec<usize>,
    /// The VM properties `GetSystemProperty` answers in `Agent_OnLoad`
    /// (interpreter round i2 wave 61, lane L1; [`onload_vm_properties`]),
    /// each with whether `SetSystemProperty` may write it.
    properties: Vec<(String, String, bool)>,
    effects: OnLoadEffects,
    /// zt t23/x19 (review page item 28e): the VM's effective
    /// `-XX:±ClassUnloading` (`VmConfig::class_unloading` under
    /// `CRATONVM_CLASS_UNLOADING_OFF_KEEPS_CLASSES`), noted by the VM being
    /// built before its agents load ([`note_onload_class_unloading`]), so
    /// `IsClassUnloadingEnabled` answers it in `Agent_OnLoad`, before the VM
    /// exists. `None`: not noted (a test scope).
    class_unloading: Option<bool>,
}

/// zt t23/x19 (review page item 28e): note the VM's effective class-unloading
/// bit in the OnLoad scope open on this thread ([`collect_startup_envs_with`]),
/// for the envs its startup agents obtain. A no-op outside a scope.
pub(crate) fn note_onload_class_unloading(on: bool) {
    STARTUP_ENVS.with(|c| {
        if let Some(scope) = c.borrow_mut().as_mut() {
            scope.class_unloading = Some(on);
        }
    });
}

/// What the startup agents' `Agent_OnLoad`s did to the VM being built,
/// applied when `Vm::new` binds their envs ([`bind_startup_envs`];
/// interpreter round i2 wave 61, lane L1): their `SetSystemProperty` writes,
/// in order, and their class search additions.
#[derive(Default, Debug)]
pub(crate) struct OnLoadEffects {
    pub(crate) properties: Vec<(String, String)>,
    pub(crate) boot_segments: Vec<String>,
    pub(crate) system_segments: Vec<String>,
    /// `SetHeapSamplingInterval` in `Agent_OnLoad` (interpreter round i2
    /// wave 63, lane L1).
    pub(crate) heap_sampling_interval: Option<JInt>,
}

/// Run `load` (the startup agents' `Agent_OnLoad`s) and answer the envs they
/// obtained through `GetEnv` meanwhile, on this thread. Those envs act on the
/// VM being built: they stay unbound (live-phase functions answer
/// `JVMTI_ERROR_WRONG_PHASE`) until `Vm::new` binds them
/// ([`bind_startup_envs`]). Before wave 13 such an env bound itself to
/// whatever VM the thread could reach (`process_vm()`), which is another VM
/// when one is already running in the process, and no `VMInit` reached it.
pub(crate) fn collect_startup_envs(load: impl FnOnce()) -> Vec<usize> {
    collect_startup_envs_with(Vec::new(), load).0
}

/// [`collect_startup_envs`] with the VM properties the OnLoad phase answers
/// (wave 61, [`onload_vm_properties`]); also answers what the agents did
/// to the VM ([`OnLoadEffects`]).
pub(crate) fn collect_startup_envs_with(
    properties: Vec<(String, String, bool)>,
    load: impl FnOnce(),
) -> (Vec<usize>, OnLoadEffects) {
    /// Puts back the collection this one replaced, also on a panic (a
    /// failed startup agent panics the boot).
    struct Restore(Option<Option<StartupScope>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(outer) = self.0.take() {
                let _ = STARTUP_ENVS.try_with(|c| *c.borrow_mut() = outer);
            }
        }
    }
    let scope = StartupScope {
        properties,
        ..StartupScope::default()
    };
    let outer = STARTUP_ENVS.with(|c| c.replace(Some(scope)));
    let restore = Restore(Some(outer));
    load();
    let collected = STARTUP_ENVS
        .with(|c| c.borrow_mut().take())
        .unwrap_or_default();
    drop(restore);
    (collected.envs, collected.effects)
}

/// `GetEnv(vm, &env, version)` for a JVMTI `version`: a new env, or `None`
/// for a version this VM does not provide (`JNI_EVERSION`). HotSpot accepts
/// 1.0 through 1.2 and 9, 11 .. its own major; so does this, up to 25.
pub(crate) fn new_env_for_version(version: JInt) -> Option<*mut c_void> {
    if version & jni::JNI_VERSION_INTERFACE_MASK != 0x3000_0000 {
        return None;
    }
    let major = (version >> 16) & 0x0FFF;
    let minor = (version >> 8) & 0xFF;
    // HotSpot's `JvmtiExport::get_jvmti_interface`: 9 and 11 with minor 0
    // only, 10 and 12 not at all, the minor ignored from 13 on (interpreter
    // round i2 wave 53, lane L1, measured: `L1W53JvmtiClassHooks`' `GetEnv`
    // row; 9.1, 11.1 and 12.0 were accepted until then).
    let supported = match major {
        1 => minor <= 2,
        9 | 11 => minor == 0,
        13..=25 => true,
        _ => false,
    };
    if !supported {
        return None;
    }
    let startup = STARTUP_ENVS.with(|c| c.borrow().is_some());
    // zt t23/x19: the OnLoad answer of `IsClassUnloadingEnabled`.
    let onload_class_unloading =
        STARTUP_ENVS.with(|c| c.borrow().as_ref().and_then(|scope| scope.class_unloading));
    let env = Box::new(JvmtiNativeEnv {
        functions: function_table(),
        state: parking_lot::Mutex::new(EnvState {
            capabilities: [0; CAPABILITY_WORDS],
            callbacks: [0; CALLBACK_SLOTS],
            enabled: 0,
            thread_enabled: Vec::new(),
            vm: None,
            startup_pending: startup,
            delivery: None,
            breakpoints: Vec::new(),
            local_storage: 0,
            thread_storage: Vec::new(),
            ext_callbacks: [0; 3],
            native_prefixes: Vec::new(),
            tags: heap::TagTable::default(),
            onload_class_unloading,
            disposed: false,
        }),
        sync: parking_lot::Mutex::new(()),
    });
    // OWNERSHIP: handed to the agent and never freed (see the module doc).
    let env = Box::into_raw(env);
    if startup {
        STARTUP_ENVS.with(|c| {
            if let Some(scope) = c.borrow_mut().as_mut() {
                scope.envs.push(env as usize); // Cast: the env's address, its registry key
            }
        });
    } else if let Some(shared) = jni::calling_thread_vm() {
        // SAFETY: allocated just above and never freed.
        unsafe { &*env }.attach_to(&shared);
    }
    Some(env.cast::<c_void>())
}

/// The process's one function table, built on first use. Immutable, like
/// the JNI table (`jni::get_jni_env`).
fn function_table() -> *const usize {
    static TABLE: std::sync::OnceLock<Box<[usize; JVMTI_FUNCTION_COUNT]>> =
        std::sync::OnceLock::new();
    TABLE.get_or_init(build_function_table).as_ptr()
}

fn build_function_table() -> Box<[usize; JVMTI_FUNCTION_COUNT]> {
    let mut table = Box::new([not_available as *const () as usize; JVMTI_FUNCTION_COUNT]);
    let slots: [(usize, usize); 151] = [
        // Interpreter round i2 wave 63, lane L1: the rest of
        // `jvmtiInterface_1_` (the i51-L1 page's last items).
        (3, modules::get_all_modules as *const () as usize),
        (7, agent_functions::stop_thread as *const () as usize),
        (8, agent_functions::interrupt_thread as *const () as usize),
        (12, agent_functions::run_agent_thread as *const () as usize),
        (40, modules::get_named_module as *const () as usize),
        (
            41,
            events_more::set_field_access_watch as *const () as usize,
        ),
        (
            42,
            events_more::clear_field_access_watch as *const () as usize,
        ),
        (
            43,
            events_more::set_field_modification_watch as *const () as usize,
        ),
        (
            44,
            events_more::clear_field_modification_watch as *const () as usize,
        ),
        (
            59,
            agent_functions::get_object_monitor_usage as *const () as usize,
        ),
        (
            73,
            agent_functions::set_native_method_prefix as *const () as usize,
        ),
        (
            74,
            agent_functions::set_native_method_prefixes as *const () as usize,
        ),
        (
            90,
            agent_functions::get_source_debug_extension as *const () as usize,
        ),
        (94, modules::add_module_reads as *const () as usize),
        (95, modules::add_module_exports as *const () as usize),
        (96, modules::add_module_opens as *const () as usize),
        (97, modules::add_module_uses as *const () as usize),
        (98, modules::add_module_provides as *const () as usize),
        (99, modules::is_modifiable_module as *const () as usize),
        (106, heap::get_tag as *const () as usize),
        (107, heap::set_tag as *const () as usize),
        (108, heap::force_garbage_collection as *const () as usize),
        (
            109,
            heap::iterate_over_objects_reachable_from_object as *const () as usize,
        ),
        (
            110,
            heap::iterate_over_reachable_objects as *const () as usize,
        ),
        (111, heap::iterate_over_heap as *const () as usize),
        (
            112,
            heap::iterate_over_instances_of_class as *const () as usize,
        ),
        (114, heap::get_objects_with_tags as *const () as usize),
        (115, heap::follow_references as *const () as usize),
        (116, heap::iterate_through_heap as *const () as usize),
        (
            118,
            agent_functions::suspend_all_virtual_threads as *const () as usize,
        ),
        (
            119,
            agent_functions::resume_all_virtual_threads as *const () as usize,
        ),
        (
            120,
            agent_functions::set_jni_function_table as *const () as usize,
        ),
        (
            121,
            agent_functions::get_jni_function_table as *const () as usize,
        ),
        (123, events_more::generate_events as *const () as usize),
        (
            124,
            extensions::get_extension_functions as *const () as usize,
        ),
        (125, extensions::get_extension_events as *const () as usize),
        (
            126,
            extensions::set_extension_event_callback as *const () as usize,
        ),
        (
            146,
            agent_functions::get_constant_pool as *const () as usize,
        ),
        (150, agent_functions::set_verbose_flag as *const () as usize),
        (
            156,
            agent_functions::set_heap_sampling_interval as *const () as usize,
        ),
        // Interpreter round i2 wave 61, lane L1: frame control (behind
        // `DebuggerGates::c_frame_control`) ...
        (67, clear_all_frame_pops as *const () as usize),
        (80, pop_frame as *const () as usize),
        (81, force_early_return_object as *const () as usize),
        (82, force_early_return_int as *const () as usize),
        (83, force_early_return_long as *const () as usize),
        (84, force_early_return_float as *const () as usize),
        (85, force_early_return_double as *const () as usize),
        (86, force_early_return_void as *const () as usize),
        // ... and what agents call at startup.
        (58, get_object_hash_code as *const () as usize),
        (130, get_system_properties as *const () as usize),
        (131, get_system_property as *const () as usize),
        (132, set_system_property as *const () as usize),
        (138, get_timer_info as *const () as usize),
        (
            149,
            add_to_bootstrap_class_loader_search as *const () as usize,
        ),
        (151, add_to_system_class_loader_search as *const () as usize),
        (154, get_object_size as *const () as usize),
        // Interpreter round i2 wave 56, lane L1.
        (15, get_thread_group_children as *const () as usize),
        (91, is_method_obsolete as *const () as usize),
        (134, get_current_thread_cpu_timer_info as *const () as usize),
        (135, get_current_thread_cpu_time as *const () as usize),
        (136, get_thread_cpu_timer_info as *const () as usize),
        (137, get_thread_cpu_time as *const () as usize),
        // Interpreter round i2 wave 54, lane L1.
        (45, is_modifiable_class as *const () as usize),
        (79, get_class_loader_classes as *const () as usize),
        (152, retransform_classes as *const () as usize),
        // Interpreter round i2 wave 53, lane L1.
        (78, get_loaded_classes as *const () as usize),
        (2, set_event_notification_mode as *const () as usize),
        (4, get_all_threads as *const () as usize),
        (5, suspend_thread as *const () as usize),
        (6, resume_thread as *const () as usize),
        // Interpreter round i1 wave 52, lane L1: what agents call first.
        (9, get_thread_info as *const () as usize),
        (13, get_top_thread_groups as *const () as usize),
        (14, get_thread_group_info as *const () as usize),
        (31, create_raw_monitor as *const () as usize),
        (32, destroy_raw_monitor as *const () as usize),
        (33, raw_monitor_enter as *const () as usize),
        (34, raw_monitor_exit as *const () as usize),
        (35, raw_monitor_wait as *const () as usize),
        (36, raw_monitor_notify_one as *const () as usize),
        (37, raw_monitor_notify_all as *const () as usize),
        (50, get_source_file_name as *const () as usize),
        (51, get_class_modifiers as *const () as usize),
        (53, get_class_fields as *const () as usize),
        (54, get_implemented_interfaces as *const () as usize),
        (55, is_interface as *const () as usize),
        (56, is_array_class as *const () as usize),
        (57, get_class_loader as *const () as usize),
        (60, get_field_name as *const () as usize),
        (61, get_field_declaring_class as *const () as usize),
        (62, get_field_modifiers as *const () as usize),
        (63, is_field_synthetic as *const () as usize),
        (68, get_max_locals as *const () as usize),
        (69, get_arguments_size as *const () as usize),
        (77, is_method_synthetic as *const () as usize),
        (129, get_jlocation_format as *const () as usize),
        (133, get_phase as *const () as usize),
        (139, get_time as *const () as usize),
        (144, get_available_processors as *const () as usize),
        (145, get_class_version_numbers as *const () as usize),
        (102, get_thread_local_storage as *const () as usize),
        (103, set_thread_local_storage as *const () as usize),
        (75, get_bytecodes as *const () as usize),
        (147, get_environment_local_storage as *const () as usize),
        (148, set_environment_local_storage as *const () as usize),
        (10, get_owned_monitor_info as *const () as usize),
        (11, get_current_contended_monitor as *const () as usize),
        (16, get_frame_count as *const () as usize),
        (17, get_thread_state as *const () as usize),
        (18, get_current_thread as *const () as usize),
        (19, get_frame_location as *const () as usize),
        (20, notify_frame_pop as *const () as usize),
        (21, get_local_object as *const () as usize),
        (22, get_local_int as *const () as usize),
        (23, get_local_long as *const () as usize),
        (24, get_local_float as *const () as usize),
        (25, get_local_double as *const () as usize),
        (26, set_local_object as *const () as usize),
        (27, set_local_int as *const () as usize),
        (28, set_local_long as *const () as usize),
        (29, set_local_float as *const () as usize),
        (30, set_local_double as *const () as usize),
        (38, set_breakpoint as *const () as usize),
        (39, clear_breakpoint as *const () as usize),
        (46, allocate as *const () as usize),
        (47, deallocate as *const () as usize),
        (48, get_class_signature as *const () as usize),
        (49, get_class_status as *const () as usize),
        (52, get_class_methods as *const () as usize),
        (64, get_method_name as *const () as usize),
        (65, get_method_declaring_class as *const () as usize),
        (66, get_method_modifiers as *const () as usize),
        (70, get_line_number_table as *const () as usize),
        (72, get_local_variable_table as *const () as usize),
        (87, redefine_classes as *const () as usize),
        (71, get_method_location as *const () as usize),
        (76, is_method_native as *const () as usize),
        (88, get_version_number as *const () as usize),
        (89, get_capabilities as *const () as usize),
        (92, suspend_thread_list as *const () as usize),
        (93, resume_thread_list as *const () as usize),
        (100, get_all_stack_traces as *const () as usize),
        (101, get_thread_list_stack_traces as *const () as usize),
        (104, get_stack_trace as *const () as usize),
        (122, set_event_callbacks as *const () as usize),
        (127, dispose_environment as *const () as usize),
        (128, get_error_name as *const () as usize),
        (140, get_potential_capabilities as *const () as usize),
        (142, add_capabilities as *const () as usize),
        (143, relinquish_capabilities as *const () as usize),
        (
            153,
            get_owned_monitor_stack_depth_info as *const () as usize,
        ),
        (155, get_local_instance as *const () as usize),
    ];
    for (slot, function) in slots {
        // Slot n (1-based, as `jvmti.h` numbers them) is entry n - 1.
        table[slot - 1] = function;
    }
    table
}

/// Every slot this table does not implement. The 64-bit C calling
/// conventions leave the arguments to the caller, so one function serves
/// every arity.
extern "C" fn not_available(_env: *mut JvmtiNativeEnv) -> JInt {
    ERR_NOT_AVAILABLE
}

/// The env at `addr`, an address [`new_env_for_version`] handed out.
///
/// # Safety
///
/// `addr` is such an address; envs are never freed.
unsafe fn env_at<'a>(addr: usize) -> &'a JvmtiNativeEnv {
    &*(addr as *const JvmtiNativeEnv) // Cast: the address back to the env it names
}

/// The env behind a `jvmtiEnv*`, when it is one this table handed out and
/// not disposed; else the error to answer.
fn live_env<'a>(env: *mut JvmtiNativeEnv) -> Result<&'a JvmtiNativeEnv, JInt> {
    if env.is_null() {
        return Err(ERR_INVALID_ENVIRONMENT);
    }
    // SAFETY: a non-null `jvmtiEnv*` passed to this table is one
    // `new_env_for_version` handed out, and envs are never freed.
    let env = unsafe { &*env };
    if env.state.lock().disposed {
        return Err(ERR_INVALID_ENVIRONMENT);
    }
    Ok(env)
}

impl JvmtiNativeEnv {
    fn addr(&self) -> usize {
        self as *const JvmtiNativeEnv as usize // Cast: the env's address, its registry key
    }

    /// The VM a call acts on: the one this env is bound to (while it lives),
    /// else the calling thread's, which it is bound to from now on. `None`
    /// before the VM exists (a startup agent's `OnLoad` phase) and after it
    /// is gone, which the live-phase functions answer with
    /// `JVMTI_ERROR_WRONG_PHASE`.
    fn vm(&self) -> Option<Arc<SharedVm>> {
        {
            let state = self.state.lock();
            if state.vm.is_some() || state.startup_pending {
                return state.vm.as_ref().and_then(Weak::upgrade);
            }
        }
        let shared = jni::calling_thread_vm()?;
        self.attach_to(&shared);
        let state = self.state.lock();
        state.vm.as_ref().and_then(Weak::upgrade)
    }

    /// zt t23/x19 (review page item 28e): the effective class-unloading bit
    /// of the VM a startup env was obtained for, noted in its OnLoad scope
    /// ([`note_onload_class_unloading`]); `None` for any other env.
    fn onload_class_unloading(&self) -> Option<bool> {
        self.state.lock().onload_class_unloading
    }

    fn possesses(&self, capability: u32) -> bool {
        self.state.lock().capabilities[0] & capability != 0
    }

    /// What this env can be granted now, as HotSpot answers it
    /// (`JvmtiManageCapabilities::get_potential_capabilities`; interpreter
    /// round i2 wave 63, lane L1, the model of [`ALWAYS`]): [`ALWAYS`], the
    /// solo capabilities no other env holds, in the OnLoad phase (a startup
    /// agent's env not bound yet) the OnLoad sets, in the live phase what a
    /// startup env of the VM acquired of them (`CTableRecord::onload_acquired`,
    /// wave 46), and whatever this env already holds. A capability whose
    /// per-VM switch is off is not offered (the switches of waves 54-61).
    fn potential(&self) -> [u32; CAPABILITY_WORDS] {
        let (startup, own) = {
            let state = self.state.lock();
            (state.startup_pending, state.capabilities)
        };
        let shared = if startup { None } else { self.vm() };
        let onload = if startup {
            ONLOAD_ONLY
        } else {
            shared
                .as_ref()
                .map_or([0; CAPABILITY_WORDS], |shared| onload_acquired_of(shared))
        };
        let others = self.solos_held_by_others(shared.as_deref());
        let mut caps = [0u32; CAPABILITY_WORDS];
        for w in 0..CAPABILITY_WORDS {
            caps[w] = (ALWAYS[w] | ALWAYS_SOLO[w] | (onload[w] & ONLOAD_ONLY[w]))
                & !(SOLO[w] & others[w]);
        }
        // The per-VM switches (in `Agent_OnLoad`, before the VM's gates
        // exist, the switch they will read).
        let (retransform, redefine, class_hooks, frame_control) = match shared.as_ref() {
            Some(shared) => {
                let gates = &shared.debug.debugger_gates;
                (
                    gates.c_retransform(),
                    gates.c_redefine(),
                    gates.c_class_hooks(),
                    gates.c_frame_control(),
                )
            }
            None => (
                crate::debug::c_retransform_switch(),
                crate::debug::c_redefine_switch(),
                crate::debug::c_class_hooks_switch(),
                crate::debug::c_frame_control_switch(),
            ),
        };
        let mut off = [0u32; CAPABILITY_WORDS];
        if !retransform {
            off = caps_of(&[
                off,
                [0, CAN_RETRANSFORM_CLASSES, 0, 0],
                cap(CAP_RETRANSFORM_ANY_CLASS),
            ]);
        }
        if !redefine {
            off = caps_of(&[
                off,
                [CAN_REDEFINE_CLASSES, 0, 0, 0],
                cap(CAP_REDEFINE_ANY_CLASS),
            ]);
        }
        if !class_hooks {
            off = caps_of(&[off, [CAN_GENERATE_ALL_CLASS_HOOK_EVENTS, 0, 0, 0]]);
        }
        if !frame_control {
            off = caps_of(&[off, [CAN_POP_FRAME, CAN_FORCE_EARLY_RETURN, 0, 0]]);
        }
        for w in 0..CAPABILITY_WORDS {
            caps[w] = (caps[w] & !off[w]) | own[w];
        }
        caps
    }

    /// The [`SOLO`] capabilities the other envs of this env's VM (in the
    /// OnLoad phase: the other startup envs) hold.
    fn solos_held_by_others(&self, shared: Option<&SharedVm>) -> [u32; CAPABILITY_WORDS] {
        let me = self.addr();
        let envs: Vec<usize> = match shared {
            Some(shared) => {
                let jvmti = shared.debug.jvmti_env.lock();
                jvmti
                    .c_table
                    .native_envs
                    .iter()
                    .chain(jvmti.c_table.startup_native_envs.iter())
                    .copied()
                    .collect()
            }
            None => STARTUP_ENVS.with(|c| {
                c.borrow()
                    .as_ref()
                    .map(|scope| scope.envs.clone())
                    .unwrap_or_default()
            }),
        };
        let mut held = [0u32; CAPABILITY_WORDS];
        for addr in envs.into_iter().filter(|&addr| addr != me) {
            // SAFETY: an address `new_env_for_version` handed out.
            let caps = unsafe { env_at(addr) }.state.lock().capabilities;
            for w in 0..CAPABILITY_WORDS {
                held[w] |= caps[w] & SOLO[w];
            }
        }
        held
    }

    /// Does this env hold `can_retransform_classes` (wave 54)?
    fn can_retransform(&self) -> bool {
        self.state.lock().capabilities[1] & CAN_RETRANSFORM_CLASSES != 0
    }

    /// Is the VM this env acts on outside JVMTI's live phase: in the start
    /// phase (wave 14: bound by `Vm::new`, `VMInit` not sent yet,
    /// [`enter_live_phase`]), the primordial phase before it or (round i2
    /// wave 55, `DebuggerGates::c_jvmti_phases`) the dead phase after
    /// `VMDeath`? The functions JVMTI allows only in the OnLoad and live
    /// phases, and the live-only ones ([`live_phase_vm`]), answer
    /// `JVMTI_ERROR_WRONG_PHASE` then, as HotSpot's do. An env still in
    /// `Agent_OnLoad` (no VM yet) is not.
    fn outside_live_phase(&self) -> bool {
        self.vm()
            .is_some_and(|shared| shared.debug.debugger_gates.jvmti_outside_live())
    }

    /// [`Self::vm`] for a function `jvmti.xml` allows in the start and live
    /// phases (`phase="start"`: the class, field, method and object facts,
    /// `GetCurrentThread`, the thread-local storage, the current thread's CPU
    /// time), `None` in the primordial and the dead phase, where HotSpot's
    /// `jvmtiEnter` wrapper answers `JVMTI_ERROR_WRONG_PHASE` before the
    /// function runs (interpreter round i4 wave 94, lane L1; stage 2 of
    /// `docs/feature-designs/jvmti-delivery-threading.md`).
    /// Those functions answered in both until then (an agent thread before
    /// `VMStart`, `Agent_OnUnload` or an agent thread after `VMDeath`). Every
    /// caller answers `None` with `WRONG_PHASE`, as it did for an env with no
    /// VM. With `CRATONVM_JVMTI_C_PHASES=0` the VM is never primordial nor
    /// dead, so this is [`Self::vm`].
    fn start_or_live_vm(&self) -> Option<Arc<SharedVm>> {
        self.vm().filter(|shared| {
            !matches!(
                shared.debug.debugger_gates.jvmti_phase(),
                crate::debug::JVMTI_PHASE_PRIMORDIAL | crate::debug::JVMTI_PHASE_DEAD
            )
        })
    }

    /// Bind this env to `shared`, once: listed as one of its native envs,
    /// and the events enabled before it had a VM (`Agent_OnLoad`) armed.
    fn attach_to(&self, shared: &Arc<SharedVm>) {
        {
            let mut state = self.state.lock();
            if state.disposed || state.vm.is_some() {
                return;
            }
            state.vm = Some(Arc::downgrade(shared));
            state.startup_pending = false;
        }
        shared
            .debug
            .jvmti_env
            .lock()
            .c_table
            .native_envs
            .push(self.addr());
        // Wave 45 (lane L1): blocked threads open their inspection window for
        // this table's stack functions from now on.
        // Wave 133 (lane L1, `CRATONVM_JVMTI_C_FUNNEL_RECORD_FROM_EXCUSE`):
        // does the excuse word carry the funnel native's callback?
        let barrier = &shared.mem.gc_barrier;
        let excuse_carries_callback =
            barrier.native_excuse_enabled() && barrier.excused_native_callback();
        shared.debug.debugger_gates.set_native_env_present(excuse_carries_callback);
        // The i54-L1 retirement: an env bound after lambda proxies existed
        // (an agent attached to the live VM) finds calls in progress that
        // recorded no proxy frame (the listing infers them), and compiled
        // code whose inline-cache thunks would keep recording none.
        if !shared.classes.lambda_proxies.read().is_empty() {
            shared.debug.debugger_gates.set_proxy_rows_incomplete();
            // JIT r23 wave 1 review (L13): an optimizing body may also have
            // spliced a proxy's SAM forwarder (`CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE`,
            // `jit_bridge::resolve_ir_lambda_sam_site`), which records no proxy
            // frame either and is counted by no adapter install.
            if crate::runtime::interpreter::lambda_jit_adapter_installs() > 0
                || cratonvm_jit::ir::ir_lambda_sam_splice_enabled()
            {
                crate::runtime::interpreter::withdraw_bodies_for_lambda_proxy_frames(shared);
            }
        }
        for (event, _) in MANAGER_EVENTS {
            let _ = self.sync_event(shared, event);
        }
        refresh_class_prepare_gate(shared);
        // Wave 63: the events `events_more` posts, enabled in `Agent_OnLoad`.
        events_more::refresh_event_gates(shared);
    }

    /// Publish this env's enable state for `event` to `shared`, after every
    /// change of it (and at binding and at the live transition):
    /// `ClassPrepare` on the VM's gate; the [`MANAGER_EVENTS`] into
    /// this env's delivery env on the VM's manager, enabled there exactly
    /// while this env has them enabled, for the same threads, and the VM is
    /// in the live phase (JVMTI posts neither in the start phase). The
    /// delivery env's enable is part of the manager's union flags, so a
    /// disable, a relinquished capability or a dispose takes the event off
    /// the VM: until wave 14 the event was enabled on the manager itself and
    /// stayed there, and the unwinder kept posting every throw of the VM.
    fn sync_event(&self, shared: &Arc<SharedVm>, event: JInt) -> JInt {
        let kind = match event {
            EVENT_BREAKPOINT => JvmtiEventKind::Breakpoint,
            EVENT_SINGLE_STEP => JvmtiEventKind::SingleStep,
            EVENT_EXCEPTION => JvmtiEventKind::Exception,
            EVENT_EXCEPTION_CATCH => JvmtiEventKind::ExceptionCatch,
            EVENT_METHOD_ENTRY => JvmtiEventKind::MethodEntry,
            EVENT_METHOD_EXIT => JvmtiEventKind::MethodExit,
            EVENT_FRAME_POP => JvmtiEventKind::FramePop,
            EVENT_CLASS_PREPARE
            | EVENT_THREAD_START
            | EVENT_THREAD_END
            | EVENT_CLASS_FILE_LOAD_HOOK
            | EVENT_CLASS_LOAD => {
                refresh_class_prepare_gate(shared);
                return ERR_NONE;
            }
            // Wave 63: the events `events_more` posts.
            _ => {
                events_more::refresh_event_gates(shared);
                return ERR_NONE;
            }
        };
        let _serial = self.sync.lock();
        // Wave 52: `ExceptionCatch` is mirrored only while the VM's switch
        // is on (an enable is refused then, [`set_event_notification_mode`];
        // one made in `Agent_OnLoad` stays unarmed).
        let live = !shared.debug.debugger_gates.jvmti_start_phase()
            && (event != EVENT_EXCEPTION_CATCH || shared.debug.debugger_gates.c_exception_catch());
        let (global, threads, bound) = {
            let state = self.state.lock();
            let armed = live && !state.disposed;
            let threads: Vec<u64> = state
                .thread_enabled
                .iter()
                .filter(|&&(e, _)| armed && e == event)
                .map(|&(_, tid)| tid)
                .collect();
            (
                armed && state.enabled & event_bit(event) != 0,
                threads,
                state.delivery.is_some(),
            )
        };
        if !global && threads.is_empty() && !bound {
            // Never armed: nothing to take back.
            return ERR_NONE;
        }
        let delivery = match self.bind_delivery(shared) {
            Ok(delivery) => delivery,
            Err(code) => return code,
        };
        let enabled = delivery
            .event_manager
            .set_event_enablement(kind, global, &threads);
        // Interpreter round i1 wave 27, lane L1: the native-call funnel
        // reports a native method's `MethodEntry` / `MethodExit` while its
        // gate is up.
        // Wave 29: and runs a stood-in Java method's bytecode while an agent
        // steps (`DebuggerGates::jvmti_frames_armed`).
        if matches!(
            kind,
            JvmtiEventKind::MethodEntry | JvmtiEventKind::MethodExit | JvmtiEventKind::SingleStep
        ) {
            refresh_method_events_gate(shared);
        }
        match enabled {
            Ok(()) => ERR_NONE,
            Err(_) => ERR_INTERNAL,
        }
    }

    /// Register this env's delivery of [`MANAGER_EVENTS`] on `shared`'s
    /// manager, once; answer the delivery env, whose own enable state
    /// [`Self::sync_event`] sets. Refused for a disposed env, so a sync that
    /// races `DisposeEnvironment` cannot register a new delivery.
    fn bind_delivery(&self, shared: &Arc<SharedVm>) -> Result<Arc<rt::JvmtiEnv>, JInt> {
        if let Some((_, delivery)) = self.state.lock().delivery.as_ref() {
            return Ok(Arc::clone(delivery));
        }
        let Some(manager) = rt::manager_for_vm(shared.vm_identity) else {
            return Err(ERR_INTERNAL);
        };
        // Built outside the env's lock: delivery takes that lock while the
        // manager holds its own callback locks.
        let delivery = Arc::new(rt::JvmtiEnv::new());
        let env_addr = self.addr();
        let (vm_bp, vm_ex) = (Arc::downgrade(shared), Arc::downgrade(shared));
        let (vm_entry, vm_exit) = (Arc::downgrade(shared), Arc::downgrade(shared));
        let vm_step = Arc::downgrade(shared);
        let vm_pop = Arc::downgrade(shared);
        let vm_catch = Arc::downgrade(shared);
        let installed = delivery.event_manager.set_event_callbacks(EventCallbacks {
            breakpoint: Some(Box::new(move |thread, method, location| {
                deliver_breakpoint(env_addr, &vm_bp, thread, method, location);
            })),
            single_step: Some(Box::new(move |thread, method, location| {
                deliver_single_step(env_addr, &vm_step, thread, method, location);
            })),
            method_entry: Some(Box::new(move |thread, method| {
                deliver_method_entry(env_addr, &vm_entry, thread, method);
            })),
            method_exit: Some(Box::new(move |thread, method, popped, value| {
                deliver_method_exit(env_addr, &vm_exit, thread, method, popped, &value);
            })),
            frame_pop: Some(Box::new(move |thread, method, popped| {
                deliver_frame_pop(env_addr, &vm_pop, thread, method, popped);
            })),
            exception_catch: Some(Box::new(move |thread, method, location, exception| {
                deliver_exception_catch(env_addr, &vm_catch, thread, (method, location), exception);
            })),
            exception: Some(Box::new(
                move |thread, method, location, exception, catch_method, catch_location| {
                    deliver_exception(
                        env_addr,
                        &vm_ex,
                        thread,
                        (method, location),
                        exception,
                        (catch_method, catch_location),
                    );
                },
            )),
            ..Default::default()
        });
        if installed.is_err() {
            return Err(ERR_INTERNAL);
        }
        let mut state = self.state.lock();
        if state.disposed {
            return Err(ERR_INVALID_ENVIRONMENT);
        }
        if let Some((_, bound)) = state.delivery.as_ref() {
            // Another thread bound it meanwhile; ours was never registered.
            return Ok(Arc::clone(bound));
        }
        if manager.register_env(&delivery).is_err() {
            return Err(ERR_INTERNAL);
        }
        state.delivery = Some((manager, Arc::clone(&delivery)));
        Ok(delivery)
    }
}

/// What `shared`'s startup envs acquired of [`ONLOAD_ONLY`] (wave 46;
/// [`bind_startup_envs`]).
fn onload_acquired_of(shared: &SharedVm) -> [u32; CAPABILITY_WORDS] {
    shared.debug.jvmti_env.lock().c_table.onload_acquired
}

/// May a C agent env of `shared` hold `can_access_local_variables`? The
/// capability is [`ONLOAD_ONLY`], so an env holds it in the live phase only
/// if a startup env of this VM acquired it in `Agent_OnLoad`
/// ([`bind_startup_envs`] records that in `onload_acquired`, and the record is
/// never lowered): a per-VM superset of "possessed now", never a false
/// negative. JIT round 21 wave 24 (lane flipprep, flip review item 2): the
/// optimizing tier's fresh-allocation volatile-store fence elision
/// (`CRATONVM_JIT_IR_FRESH_VOLATILE_STORE_UNFENCED`) is computed empty while
/// this answers `true` (HotSpot 16+ instead keeps escape analysis and
/// deoptimizes on access through escape barriers, JDK-8227745; with no such
/// barrier here, the elision declines). Asked once per compile request, and only under that switch;
/// `try_lock`, so a request built while this thread holds the VM's JVMTI
/// lock answers `true` (fail-closed: the fence stays) instead of deadlocking.
pub(crate) fn locals_capability_may_be_held(shared: &SharedVm) -> bool {
    match shared.debug.jvmti_env.try_lock() {
        Some(jvmti) => jvmti.c_table.onload_acquired[0] & CAN_ACCESS_LOCAL_VARIABLES != 0,
        None => true,
    }
}

/// Raise or lower `shared`'s `ClassPrepare` gate
/// (`DebuggerGates::native_class_prepare_armed`) from its native envs'
/// state. Under the VM's JVMTI lock, so two envs changing their state at
/// once cannot publish out of order.
fn refresh_class_prepare_gate(shared: &SharedVm) {
    let jvmti = shared.debug.jvmti_env.lock();
    let armed_for = |event: JInt| {
        jvmti.c_table.native_envs.iter().any(|&addr| {
            // SAFETY: the list holds only addresses `new_env_for_version`
            // handed out.
            let state = unsafe { env_at(addr) }.state.lock();
            !state.disposed && state.enabled_anywhere(event)
        })
    };
    // Wave 55: none of the three class events in the primordial phase
    // (`DebuggerGates::c_jvmti_phases`), as HotSpot's `post_class_load`,
    // `post_class_prepare` and (without `can_generate_early_class_hook_events`)
    // the hook return early in it; [`enter_start_phase`] refreshes them.
    let gates = &shared.debug.debugger_gates;
    let primordial = gates.jvmti_phase() == crate::debug::JVMTI_PHASE_PRIMORDIAL;
    gates.set_native_class_prepare(!primordial && armed_for(EVENT_CLASS_PREPARE));
    // Wave 52: and the `ThreadStart` / `ThreadEnd` gate.
    // (Wave 55: not in the primordial phase either; HotSpot posts no
    // `ThreadStart` before `VMStart`, measured: `L1W55JvmtiPhases`.)
    gates.set_native_thread_events(
        !primordial && (armed_for(EVENT_THREAD_START) || armed_for(EVENT_THREAD_END)),
    );
    // Wave 53: and the `ClassLoad` / `ClassFileLoadHook` gates, while the
    // VM's switch is on (an enable is refused while it is off,
    // [`set_event_notification_mode`]; one made in `Agent_OnLoad` stays
    // unarmed). The hook only in the live phase: the load path runs it
    // inside the transformer chain, which runs Java (the defining loader,
    // the domain), and the start phase has no Java to run. HotSpot posts it
    // in the start phase too; classes loaded then are not offered here.
    // Since wave 54, with `DebuggerGates::c_start_phase_hook`, in the start
    // phase too, for the bootstrap loader's classes, whose walk runs no
    // Java (`instrument::start_phase_withholds`).
    // Wave 63: in the primordial phase too with the early class hook
    // (`can_generate_early_class_hook_events`, [`bind_startup_envs`]).
    let hooks = gates.c_class_hooks() && (!primordial || jvmti.early_class_hooks);
    let phase_allows = !gates.jvmti_start_phase() || gates.c_start_phase_hook();
    gates.set_native_class_hooks(
        hooks && armed_for(EVENT_CLASS_LOAD),
        hooks && phase_allows && armed_for(EVENT_CLASS_FILE_LOAD_HOOK),
    );
}

/// Raise or lower `shared`'s JVMTI half of the native-call funnel's method
/// event gate (`DebuggerGates::jvmti_method_events_armed`, interpreter round
/// i1 wave 27, lane L1) from its JVMTI manager: whether an env it delivers to
/// listens for `MethodEntry` or `MethodExit`, and (wave 29) whether one
/// listens for `SingleStep` (`DebuggerGates::jvmti_frames_armed`: the
/// funnel's hook then runs a stood-in Java method's bytecode, so the agent
/// steps into it). After every change of an env's enable state for any of
/// the three and after a dispose; under the VM's JVMTI lock, so two envs
/// changing their state at once cannot publish out of order (each reads the
/// manager after its own change).
fn refresh_method_events_gate(shared: &SharedVm) {
    let _jvmti = shared.debug.jvmti_env.lock();
    let vm = shared.vm_identity;
    let armed = rt::any_method_entry_listener_active_for_vm(vm)
        || rt::any_method_exit_listener_active_for_vm(vm);
    shared.debug.debugger_gates.set_jvmti_method_events(armed);
    shared
        .debug
        .debugger_gates
        .set_jvmti_single_step(rt::any_single_step_listener_active_for_vm(vm));
}

/// The agent's callback for `event` on thread `tid`, when the env at
/// `env_addr` is live and has the event enabled for that thread.
fn callback_for(env_addr: usize, event: JInt, tid: u64) -> Option<usize> {
    // SAFETY: an address `new_env_for_version` handed out.
    let env = unsafe { env_at(env_addr) };
    let state = env.state.lock();
    if state.disposed || !state.enabled_for(event, tid) {
        return None;
    }
    let slot = usize::try_from(event - MIN_EVENT).ok()?;
    state.callbacks.get(slot).copied().filter(|&f| f != 0)
}

/// Run an agent callback on VM thread `tid` (the executing thread): a local
/// frame popped afterwards, the thread's `java.lang.Thread` as a local
/// reference (NULL while it has none), the VM's JNI context and the thread's
/// own `JvmThread` installed, as around a native call, so the callback can
/// call JNI. `call` gets the `JNIEnv*` and the `jthread`, and may create
/// more local references.
fn in_event_context(shared: &SharedVm, tid: u64, call: impl FnOnce(JNIEnv, JObject)) {
    let registry = &shared.threads.thread_registry;
    let thread_id = ThreadId(tid);
    // gci i12/ev: a poster that installed this thread's own handle is a
    // flush point for the `ClassLoad`s deferred on it, delivered first (as
    // HotSpot posted them, before this event). One load while none waits.
    if shared
        .debug
        .deferred_class_loads_pending
        .load(std::sync::atomic::Ordering::Acquire)
        && own_handle_installed(
            registry.own_jvm_thread_addr(thread_id),
            jni::installed_jni_thread_addr(),
        )
    {
        flush_deferred_class_loads(shared, tid);
    }
    let depth = jni::local_frame_depth();
    jni::push_local_frame(16);
    // No safepoint between the registry read and the handle.
    let thread = registry
        .java_thread_obj(thread_id)
        .map_or(0, jni::new_local_handle);
    let prev_vm = jni::replace_jni_context(shared);
    // The executing thread's own `JvmThread`, which only it may use.
    //
    // gci i11/ja (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
    // item 1): the poster holds this thread's `&mut` up the stack. When the
    // pointer already installed here is that same thread -- the native funnel
    // installed it, or the poster did (`post_on_own_thread`) -- it was derived
    // from that live `&mut` and is kept: a GC-safe JVMTI function the
    // callback calls reborrows it. Only when none is installed (or another)
    // is the registry's address installed, as before.
    let prev_thread = event_thread_to_install(
        registry.own_jvm_thread_addr(thread_id),
        jni::installed_jni_thread_addr(),
    )
    // SAFETY: this thread's published `JvmThread`, valid while it runs.
    .map(|addr| unsafe { jni::replace_jni_thread(addr as *mut JvmThread) });
    // gcd d4/k2: the agent callback may hold raw local refs; the pinned young
    // copy must not relocate while it runs (gc_quiescence raw-locals count).
    let raw_jni_locals = jni::RawLocalsDispatch::enter();
    call(jni::get_jni_env(), thread);
    drop(raw_jni_locals);
    if let Some(prev) = prev_thread {
        // SAFETY: the pointer this thread had installed before.
        unsafe { jni::restore_jni_thread(prev) };
    }
    jni::restore_jni_context(prev_vm);
    jni::truncate_local_frames(depth);
}

/// gci i11/ja: the address [`in_event_context`] must install for an agent
/// callback: the registry's published address of the executing thread
/// (`own`), unless the pointer already installed (`installed`) is that same
/// thread, which is kept (its provenance is the poster's live `&mut`). `None`:
/// install nothing.
fn event_thread_to_install(own: Option<usize>, installed: Option<usize>) -> Option<usize> {
    own.filter(|&addr| installed != Some(addr))
}

/// The `Breakpoint` event for the env at `env_addr`, on the executing
/// thread (`JvmtiEventManager::fire_breakpoint`).
fn deliver_breakpoint(env_addr: usize, vm: &Weak<SharedVm>, tid: u64, method: u64, location: i64) {
    let Some(callback) = callback_for(env_addr, EVENT_BREAKPOINT, tid) else {
        return;
    };
    let Some(shared) = vm.upgrade() else {
        return;
    };
    // SAFETY: the agent registered this `jvmtiEventCallbacks` member as its
    // `Breakpoint` callback.
    let callback = unsafe { std::mem::transmute::<usize, BreakpointCallback>(callback) };
    in_event_context(&shared, tid, |jni_env, thread| {
        // SAFETY: a C function of the agent's with the `Breakpoint` signature.
        unsafe {
            callback(
                env_addr as *mut JvmtiNativeEnv,
                jni_env,
                thread,
                method,
                location,
            );
        }
    });
}

/// The `SingleStep` event for the env at `env_addr`, on the executing thread,
/// before the bytecode at `location` runs (`JvmtiEventManager::fire_single_step`,
/// posted by the dispatch loop once per bytecode while a listener exists;
/// interpreter round i1 wave 22, lane L1). The callback has `Breakpoint`'s
/// signature.
fn deliver_single_step(env_addr: usize, vm: &Weak<SharedVm>, tid: u64, method: u64, location: i64) {
    let Some(callback) = callback_for(env_addr, EVENT_SINGLE_STEP, tid) else {
        return;
    };
    let Some(shared) = vm.upgrade() else {
        return;
    };
    // SAFETY: the agent registered this `jvmtiEventCallbacks` member as its
    // `SingleStep` callback, whose signature is `Breakpoint`'s.
    let callback = unsafe { std::mem::transmute::<usize, BreakpointCallback>(callback) };
    in_event_context(&shared, tid, |jni_env, thread| {
        // SAFETY: a C function of the agent's with the `SingleStep` signature.
        unsafe {
            callback(
                env_addr as *mut JvmtiNativeEnv,
                jni_env,
                thread,
                method,
                location,
            );
        }
    });
}

/// The `Exception` event for the env at `env_addr`, on the throwing thread
/// (`JvmtiEventManager::fire_exception`, posted by the interpreter's
/// unwinder). `exception` is the thrown object's address, which the unwinder
/// pins while the callbacks run (`interpreter::post_jvmti_exception`), as it
/// is when this env is called (the manager re-reads it after each listener,
/// `JvmtiEventManager::deliver_with_object`, wave 18); the agent gets it as a
/// local reference. No known catch is a NULL method and
/// location 0, as JVMTI specifies (the manager carries `0` / `-1`).
fn deliver_exception(
    env_addr: usize,
    vm: &Weak<SharedVm>,
    tid: u64,
    (method, location): (u64, i64),
    exception: u64,
    (catch_method, catch_location): (u64, i64),
) {
    let Some(callback) = callback_for(env_addr, EVENT_EXCEPTION, tid) else {
        return;
    };
    let Some(shared) = vm.upgrade() else {
        return;
    };
    // SAFETY: the agent registered this `jvmtiEventCallbacks` member as its
    // `Exception` callback.
    let callback = unsafe { std::mem::transmute::<usize, ExceptionCallback>(callback) };
    let catch_location = if catch_method == 0 { 0 } else { catch_location };
    in_event_context(&shared, tid, |jni_env, thread| {
        let exception = if exception == 0 {
            0
        } else {
            // SAFETY: the live, pinned exception's address (no safepoint since
            // the unwinder read it).
            let obj = unsafe { ObjectRef::from_raw(exception as usize as *mut u8) }; // Cast: the address the manager carries
            jni::new_local_handle(obj)
        };
        // SAFETY: a C function of the agent's with the `Exception` signature.
        unsafe {
            callback(
                env_addr as *mut JvmtiNativeEnv,
                jni_env,
                thread,
                method,
                location,
                exception,
                catch_method,
                catch_location,
            );
        }
    });
}

/// The `ExceptionCatch` event for the env at `env_addr`, on the catching
/// thread, once the handler frame holds the exception as its only operand
/// (`JvmtiEventManager::fire_exception_catch`, posted by the unwinder and the
/// compiled catch doors: `jvmti_events::fire_jvmti_exception_catch`).
/// `exception` is the caught object's address, which that frame roots while
/// the callbacks run (re-read after each listener,
/// `JvmtiEventManager::deliver_with_object`); the agent gets it as a local
/// reference. Interpreter round i1 wave 52, lane L1.
fn deliver_exception_catch(
    env_addr: usize,
    vm: &Weak<SharedVm>,
    tid: u64,
    (method, location): (u64, i64),
    exception: u64,
) {
    let Some(callback) = callback_for(env_addr, EVENT_EXCEPTION_CATCH, tid) else {
        return;
    };
    let Some(shared) = vm.upgrade() else {
        return;
    };
    // SAFETY: the agent registered this `jvmtiEventCallbacks` member as its
    // `ExceptionCatch` callback.
    let callback = unsafe { std::mem::transmute::<usize, ExceptionCatchCallback>(callback) };
    in_event_context(&shared, tid, |jni_env, thread| {
        let exception = if exception == 0 {
            0
        } else {
            // SAFETY: the live, rooted exception's address (no safepoint
            // since the manager read it).
            let obj = unsafe { ObjectRef::from_raw(exception as usize as *mut u8) }; // Cast: the address the manager carries
            jni::new_local_handle(obj)
        };
        // SAFETY: a C function of the agent's with the `ExceptionCatch`
        // signature.
        unsafe {
            callback(
                env_addr as *mut JvmtiNativeEnv,
                jni_env,
                thread,
                method,
                location,
                exception,
            );
        }
    });
}

/// The `MethodEntry` event for the env at `env_addr`, on the executing thread
/// (`JvmtiEventManager::fire_method_entry`, posted by the interpreter once the
/// callee's frame is pushed; wave 17). `method` is a real `jmethodID`
/// (`interpreter::jvmti_method_id`).
fn deliver_method_entry(env_addr: usize, vm: &Weak<SharedVm>, tid: u64, method: u64) {
    let Some(callback) = callback_for(env_addr, EVENT_METHOD_ENTRY, tid) else {
        return;
    };
    let Some(shared) = vm.upgrade() else {
        return;
    };
    // SAFETY: the agent registered this `jvmtiEventCallbacks` member as its
    // `MethodEntry` callback.
    let callback = unsafe { std::mem::transmute::<usize, MethodEntryCallback>(callback) };
    in_event_context(&shared, tid, |jni_env, thread| {
        // SAFETY: a C function of the agent's with the `MethodEntry` signature.
        unsafe { callback(env_addr as *mut JvmtiNativeEnv, jni_env, thread, method) };
    });
}

/// A frame this env asked `FramePop` for (`NotifyFramePop`, wave 45) was
/// popped on thread `tid`: `method` is its method, `popped` whether an
/// exception ended it.
fn deliver_frame_pop(env_addr: usize, vm: &Weak<SharedVm>, tid: u64, method: u64, popped: bool) {
    let Some(callback) = callback_for(env_addr, EVENT_FRAME_POP, tid) else {
        return;
    };
    let Some(shared) = vm.upgrade() else {
        return;
    };
    // SAFETY: the agent registered this `jvmtiEventCallbacks` member as its
    // `FramePop` callback.
    let callback = unsafe { std::mem::transmute::<usize, FramePopCallback>(callback) };
    in_event_context(&shared, tid, |jni_env, thread| {
        // SAFETY: a C function of the agent's with the `FramePop` signature.
        unsafe {
            callback(
                env_addr as *mut JvmtiNativeEnv,
                jni_env,
                thread,
                method,
                u8::from(popped),
            )
        };
    });
}

/// The `jvalue` bits of a `MethodExit` return value: the primitive in the
/// union's low bytes (a `boolean` .. `int` result as its `jint`), a
/// reference as `handle`, the local reference the caller made of it.
fn return_jvalue(value: &rt::LocalValue, handle: JObject) -> u64 {
    match value {
        // Cast: the jint's bits, zero-extended (the union's upper bytes are
        // not part of a jint).
        rt::LocalValue::Int(i) => u64::from(*i as u32),
        // Cast: the jlong's bits.
        rt::LocalValue::Long(l) => *l as u64,
        rt::LocalValue::Float(f) => u64::from(f.to_bits()),
        rt::LocalValue::Double(d) => d.to_bits(),
        rt::LocalValue::Object(_) => handle,
    }
}

/// The `MethodExit` event for the env at `env_addr`, on the executing thread
/// (`JvmtiEventManager::fire_method_exit`; wave 17). A reference result is
/// the returned object's address, which the interpreter keeps rooted while
/// the callbacks run (`interpreter::fire_method_exit_keeping_value` and the
/// parent-stack push before the event), as it is when this env is called
/// (wave 18, as for `Exception`); the agent gets it as a local
/// reference. An exit by exception has no value (`jvalue` 0), as JVMTI
/// specifies.
fn deliver_method_exit(
    env_addr: usize,
    vm: &Weak<SharedVm>,
    tid: u64,
    method: u64,
    was_popped_by_exception: bool,
    value: &rt::LocalValue,
) {
    let Some(callback) = callback_for(env_addr, EVENT_METHOD_EXIT, tid) else {
        return;
    };
    let Some(shared) = vm.upgrade() else {
        return;
    };
    // SAFETY: the agent registered this `jvmtiEventCallbacks` member as its
    // `MethodExit` callback.
    let callback = unsafe { std::mem::transmute::<usize, MethodExitCallback>(callback) };
    in_event_context(&shared, tid, |jni_env, thread| {
        let handle = match value {
            rt::LocalValue::Object(Some(address)) if !was_popped_by_exception => {
                // SAFETY: the live, rooted result's address (no safepoint
                // since the interpreter read it).
                let obj = unsafe { ObjectRef::from_raw(*address as usize as *mut u8) }; // Cast: the address the manager carries
                jni::new_local_handle(obj)
            }
            _ => 0,
        };
        let bits = if was_popped_by_exception {
            0
        } else {
            return_jvalue(value, handle)
        };
        // SAFETY: a C function of the agent's with the `MethodExit` signature.
        unsafe {
            callback(
                env_addr as *mut JvmtiNativeEnv,
                jni_env,
                thread,
                method,
                u8::from(was_popped_by_exception),
                bits,
            );
        }
    });
}

/// Class `class_id` of `shared` was just prepared on VM thread `tid`
/// (`debug::class_prepared_on_thread`, before any of its code runs): the
/// `ClassPrepare` callback of every native env of the VM that has it
/// enabled for that thread. Asked only while
/// `DebuggerGates::native_class_prepare_armed` is up. Once per class (wave
/// 15): a class whose initialization is retried after a supertype's link
/// failure is prepared again, but was prepared, and reported, already.
pub(crate) fn post_class_prepare(shared: &SharedVm, tid: u64, class_id: ClassId) {
    let envs = {
        let mut state = shared.debug.jvmti_env.lock();
        if !state.c_table.class_prepare_posted.insert(class_id) {
            return;
        }
        state.c_table.native_envs.clone()
    };
    let class = jni::class_id_to_jclass(class_id);
    for env_addr in envs {
        let Some(callback) = callback_for(env_addr, EVENT_CLASS_PREPARE, tid) else {
            continue;
        };
        // SAFETY: the agent registered this `jvmtiEventCallbacks` member as
        // its `ClassPrepare` callback.
        let callback = unsafe { std::mem::transmute::<usize, ClassPrepareCallback>(callback) };
        in_event_context(shared, tid, |jni_env, thread| {
            // SAFETY: a C function of the agent's with the `ClassPrepare`
            // signature.
            unsafe { callback(env_addr as *mut JvmtiNativeEnv, jni_env, thread, class) };
        });
    }
}

/// `void (JNICALL *ThreadStart)(jvmtiEnv*, JNIEnv*, jthread)`, also
/// `ThreadEnd`'s (wave 52).
type ThreadEventCallback = unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject);

/// Platform thread `tid` of `shared` is about to run its `run()` (`start`)
/// or has returned from it (`!start`), on that thread: the `ThreadStart` /
/// `ThreadEnd` callback of every native env of the VM that has it enabled
/// for the thread, as HotSpot posts them on the thread itself (interpreter
/// round i1 wave 52, lane L1). Asked only while
/// `DebuggerGates::native_thread_events_armed` is up. A virtual thread posts
/// neither (HotSpot posts `VirtualThreadStart` / `End` for it, which this
/// table does not deliver). A thread that attached through JNI posts through
/// [`post_attached_thread_event`] (wave 86).
pub(crate) fn post_thread_event(shared: &SharedVm, tid: u64, start: bool) {
    let event = if start {
        EVENT_THREAD_START
    } else {
        EVENT_THREAD_END
    };
    let envs = shared.debug.jvmti_env.lock().c_table.native_envs.clone();
    for env_addr in envs {
        let Some(callback) = callback_for(env_addr, event, tid) else {
            continue;
        };
        // SAFETY: the agent registered this `jvmtiEventCallbacks` member as
        // its `ThreadStart` / `ThreadEnd` callback.
        let callback = unsafe { std::mem::transmute::<usize, ThreadEventCallback>(callback) };
        in_event_context(shared, tid, |jni_env, thread| {
            // SAFETY: a C function of the agent's with that signature.
            unsafe { callback(env_addr as *mut JvmtiNativeEnv, jni_env, thread) };
        });
    }
}

/// [`post_thread_event`] for a thread that attached through JNI
/// `AttachCurrentThread` (`start`) or is detaching through
/// `DetachCurrentThread`, on that thread (interpreter round i4 wave 86,
/// lane L1): HotSpot's `attach_current_thread` posts JVMTI `ThreadStart`,
/// and the detach's `JavaThread::exit` `ThreadEnd`. The thread is idle (in
/// its blocked region, as an attached thread is between JNI calls), so the
/// `jthread` handle is made inside a foreign JNI entry, which holds off a
/// collection while the object is read, and the callback runs outside it,
/// idle again, exactly as the agent's own code runs after any JNI call that
/// returned a local reference: a callback that blocks does not stall a
/// collection. Asked only while `DebuggerGates::native_thread_events_armed`
/// is up.
pub(crate) fn post_attached_thread_event(shared: &SharedVm, tid: u64, start: bool) {
    let event = if start {
        EVENT_THREAD_START
    } else {
        EVENT_THREAD_END
    };
    let envs = shared.debug.jvmti_env.lock().c_table.native_envs.clone();
    for env_addr in envs {
        let Some(callback) = callback_for(env_addr, event, tid) else {
            continue;
        };
        // SAFETY: the agent registered this `jvmtiEventCallbacks` member as
        // its `ThreadStart` / `ThreadEnd` callback.
        let callback = unsafe { std::mem::transmute::<usize, ThreadEventCallback>(callback) };
        let depth = jni::local_frame_depth();
        jni::push_local_frame(16);
        let thread = {
            let _fx = jni::ForeignJniEntry::enter();
            shared
                .threads
                .thread_registry
                .java_thread_obj(ThreadId(tid))
                .map_or(0, jni::new_local_handle)
        };
        let raw_jni_locals = jni::RawLocalsDispatch::enter();
        // SAFETY: a C function of the agent's with that signature, on the
        // thread it names, with this thread's JNI environment.
        unsafe { callback(env_addr as *mut JvmtiNativeEnv, jni::get_jni_env(), thread) };
        drop(raw_jni_locals);
        jni::truncate_local_frames(depth);
    }
}

/// Class `class_id` of `shared` was just defined on VM thread `tid` (the
/// class manager's deferred `ClassLoad` hook, drained once the defining
/// write guard is released: `runtime::jvmti::JvmtiEventManager::post_class_load`):
/// the `ClassLoad` callback of every native env of the VM that has it enabled
/// for that thread, with `ClassPrepare`'s signature, before the class is
/// linked (interpreter round i2 wave 53, lane L1). Asked only while
/// `DebuggerGates::native_class_load_armed` is up. An array class posts none
/// (HotSpot posts none either): it is synthesised, not defined.
pub(crate) fn post_class_load(shared: &SharedVm, tid: u64, class_id: ClassId) {
    // gci i12/ev (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
    // item 1c): this post runs where the class manager's write guard drops,
    // inside whatever load or define the thread is doing. When nobody up the
    // stack installed the thread's own handle, the callback would get the
    // registry's address of a thread whose `&mut` is live there. Under
    // `CRATONVM_JVMTI_DEFER_UNOWNED_CLASS_LOAD` such a post waits for the
    // thread's next own-handle point instead (`flush_deferred_class_loads`).
    if class_load_deferred(
        shared
            .threads
            .thread_registry
            .own_jvm_thread_addr(ThreadId(tid)),
        jni::installed_jni_thread_addr(),
        defer_unowned_class_load_enabled,
    ) {
        let mut state = shared.debug.jvmti_env.lock();
        state.deferred_class_loads.push((tid, class_id));
        shared
            .debug
            .deferred_class_loads_pending
            .store(true, std::sync::atomic::Ordering::Release);
        return;
    }
    let envs = shared.debug.jvmti_env.lock().c_table.native_envs.clone();
    let class = jni::class_id_to_jclass(class_id);
    for env_addr in envs {
        let Some(callback) = callback_for(env_addr, EVENT_CLASS_LOAD, tid) else {
            continue;
        };
        // SAFETY: the agent registered this `jvmtiEventCallbacks` member as
        // its `ClassLoad` callback, whose signature is `ClassPrepare`'s.
        let callback = unsafe { std::mem::transmute::<usize, ClassPrepareCallback>(callback) };
        in_event_context(shared, tid, |jni_env, thread| {
            // SAFETY: a C function of the agent's with the `ClassLoad`
            // signature.
            unsafe { callback(env_addr as *mut JvmtiNativeEnv, jni_env, thread, class) };
        });
    }
}

/// i53-L1 wave 124 (lane L1): `CRATONVM_JVMTI_C_CLASS_LOAD_AT_ADAPTER`
/// (default OFF, `runtime_flag_on`). On, the C table's `ClassLoad` is posted
/// by the class-load hook's adapter (`vm_init::class_load_adapter`, through
/// [`post_class_load_at_adapter`]), which names the VM by the defining
/// store's layout domain, instead of by the D14 bridge
/// (`runtime::jvmti::post_class_load_to_bridge`), which skips its C half for
/// that define. Read once per class defined, by the adapter, which hands the
/// answer down to the bridge, so both sides of one define agree.
///
/// What changes with it on is only *when the post is reachable*: the bridge
/// posts only while the VM has a JVMTI manager row and a registered bridge
/// (`Vm::new`), the adapter while the VM is in the hook registry
/// (`set_global_shared_vm_for_hooks`). The order within one define is
/// unchanged (`-Xlog:class+load`, the C table, the older manager, the
/// manager's Rust listeners). This is the step that lets the bridge be
/// retired; it stays off until the C JVMTI battery (MSVC agents) has run
/// with it on.
pub(crate) fn class_load_at_adapter_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JVMTI_C_CLASS_LOAD_AT_ADAPTER")
}

/// The class-load hook adapter's half of the C table's `ClassLoad` (wave
/// 124, lane L1; asked only while [`class_load_at_adapter_enabled`]): class
/// `class_id` was defined, on VM thread `tid`, by the class store with layout
/// domain `owner_domain`. Posts through [`post_class_load`] (whose deferral
/// for a thread with no own handle installed is unchanged) when that VM is
/// live in the hook registry and a C env listens for `ClassLoad`
/// (`DebuggerGates::native_class_load_armed`, one load). One registry probe
/// otherwise, the cost the bridge's lookup has today.
pub(crate) fn post_class_load_at_adapter(owner_domain: u32, tid: u64, class_id: u32) {
    let Some(shared) = crate::vm::vm_init::hook_vm_for_domain(owner_domain) else {
        return;
    };
    if shared.debug.debugger_gates.native_class_load_armed() {
        post_class_load(&shared, tid, ClassId::new(class_id));
    }
}

/// gci i12/ev: `CRATONVM_JVMTI_DEFER_UNOWNED_CLASS_LOAD` (default OFF,
/// `runtime_flag_on`), asked only for a `ClassLoad` posted with no handle of
/// the thread's own installed (a C env listening).
pub(crate) fn defer_unowned_class_load_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JVMTI_DEFER_UNOWNED_CLASS_LOAD")
}

/// gci i12/ev: is the pointer installed as this thread's JNI binding
/// (`installed`) its own published `JvmThread` (`own`), i.e. installed by a
/// poster or a native funnel from the live `&mut`?
fn own_handle_installed(own: Option<usize>, installed: Option<usize>) -> bool {
    own.is_some() && installed == own
}

/// gci i12/ev: does a `ClassLoad` posted on a published thread whose own
/// handle is not installed (`own`, `installed`) wait for a flush point?
/// Only while the switch is on (`switch_on`, asked last).
fn class_load_deferred(
    own: Option<usize>,
    installed: Option<usize>,
    switch_on: impl FnOnce() -> bool,
) -> bool {
    own.is_some() && !own_handle_installed(own, installed) && switch_on()
}

/// gci i12/ev: deliver the `ClassLoad`s deferred on VM thread `tid`
/// ([`post_class_load`]), in post order. Called only where this thread's own
/// handle is installed: [`in_event_context`] (any C event posted under
/// `post_on_own_thread`), the native funnel's `JniContextGuard::install`,
/// `vm_util::link_claimed_class` before `ClassPrepare`, and the platform
/// thread body before `ThreadEnd`. One load while nothing waits.
pub(crate) fn flush_deferred_class_loads(shared: &SharedVm, tid: u64) {
    if !shared
        .debug
        .deferred_class_loads_pending
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return;
    }
    // The gate drops, under the lock, only with the queue empty; a post
    // raises it under the same lock.
    let mine = {
        let mut state = shared.debug.jvmti_env.lock();
        let mine = take_deferred_class_loads(&mut state.deferred_class_loads, tid);
        if state.deferred_class_loads.is_empty() {
            shared
                .debug
                .deferred_class_loads_pending
                .store(false, std::sync::atomic::Ordering::Release);
        }
        mine
    };
    for class_id in mine {
        post_class_load(shared, tid, class_id);
    }
}

/// gci i12/ev: remove and answer `tid`'s entries of `queue`, in order.
fn take_deferred_class_loads(queue: &mut Vec<(u64, ClassId)>, tid: u64) -> Vec<ClassId> {
    let mut mine = Vec::new();
    queue.retain(|&(owner, class_id)| {
        if owner == tid {
            mine.push(class_id);
            false
        } else {
            true
        }
    });
    mine
}

/// `void (JNICALL *ClassFileLoadHook)(jvmtiEnv*, JNIEnv*, jclass
/// class_being_redefined, jobject loader, const char* name, jobject
/// protection_domain, jint class_data_len, const unsigned char* class_data,
/// jint* new_class_data_len, unsigned char** new_class_data)` (wave 53).
type ClassFileLoadHookCallback = unsafe extern "C" fn(
    *mut JvmtiNativeEnv,
    JNIEnv,
    JObject,
    JObject,
    *const u8,
    JObject,
    JInt,
    *const u8,
    *mut JInt,
    *mut *mut u8,
);

/// The VM of identity `vm` when a C env of it takes `ClassFileLoadHook`
/// (`DebuggerGates::native_class_file_load_hook_armed`), for the transformer
/// chain, which knows its VM only by identity (`runtime::instrument`). The
/// calling thread's VM first (one thread-local read), the VM registry only
/// when that is another VM. Asked once per class file the chain is offered.
pub(crate) fn class_file_load_hook_vm(vm: usize) -> Option<Arc<SharedVm>> {
    let shared = match jni::calling_thread_vm() {
        Some(shared) if shared.vm_identity == vm => shared,
        _ => jni::vm_for_identity(vm)?,
    };
    shared
        .debug
        .debugger_gates
        .native_class_file_load_hook_armed()
        .then_some(shared)
}

/// The JNI global references a `ClassFileLoadHook` delivery holds its three
/// objects in, across every env's callback: a callback runs JNI, so a
/// collection may move them between two envs. Deleted on drop.
struct HookObjects<'a> {
    shared: &'a SharedVm,
    handles: [JObject; 3],
}

impl<'a> HookObjects<'a> {
    fn new(shared: &'a SharedVm, objects: [Option<ObjectRef>; 3]) -> Self {
        let mut globals = shared.natives.jni_global_refs.lock();
        let handles = objects.map(|obj| obj.map_or(0, |obj| globals.add(obj)));
        drop(globals);
        Self { shared, handles }
    }

    /// Local references of the three objects' current addresses, in the
    /// callback's local frame (NULL for an absent one).
    fn locals(&self) -> [JObject; 3] {
        let globals = self.shared.natives.jni_global_refs.lock();
        let current = self
            .handles
            .map(|h| if h == 0 { None } else { globals.resolve(h) });
        drop(globals);
        current.map(|obj| obj.map_or(0, jni::new_local_handle))
    }
}

impl Drop for HookObjects<'_> {
    fn drop(&mut self) {
        let mut globals = self.shared.natives.jni_global_refs.lock();
        for handle in self.handles {
            if handle != 0 {
                let _ = globals.remove(handle);
            }
        }
    }
}

/// Offer the class file `bytes` of class `name` (internal form) to the
/// `ClassFileLoadHook` callback of every native env of `shared` that has it
/// enabled, on VM thread `tid`, in env order, each env handed the previous
/// one's answer; the last answer, or `None` when no callback replaced the
/// bytes (interpreter round i2 wave 53, lane L1). JVMTI's
/// `ClassFileLoadHook`, as `JvmtiClassFileLoadHookPoster` posts it: at load
/// `class_being_redefined` is NULL and `loader` the defining loader (NULL
/// for the bootstrap loader); at a redefinition the class and its loader.
/// A callback's replacement is a block of its env's `Allocate`, freed here
/// once copied, as HotSpot deallocates it.
///
/// Run inside the transformer chain `java.lang.instrument` uses
/// (`runtime::instrument::run_chain_over_bytes`), one group of envs at a
/// time, as `JvmtiClassFileLoadHookPoster::post_all_envs` walks them: the
/// envs without `can_retransform_classes` (`retransformable` false) before
/// the Java transformers registered without `canRetransform`, the envs with
/// it (wave 54) before the retransform-capable ones; a retransformation
/// offers the class to the second group only.
pub(crate) fn post_class_file_load_hook(
    shared: &SharedVm,
    tid: u64,
    name: &str,
    [class_being_redefined, loader, domain]: [Option<ObjectRef>; 3],
    bytes: &[u8],
    retransformable: bool,
) -> Option<Vec<u8>> {
    let envs: Vec<usize> = {
        let jvmti = shared.debug.jvmti_env.lock();
        jvmti
            .c_table
            .native_envs
            .iter()
            .copied()
            // SAFETY: an address `new_env_for_version` handed out.
            .filter(|&addr| unsafe { env_at(addr) }.can_retransform() == retransformable)
            .collect()
    };
    if envs.is_empty() {
        return None;
    }
    let name_utf8 = modified_utf8(name);
    let objects = HookObjects::new(shared, [class_being_redefined, loader, domain]);
    let mut current: Option<Vec<u8>> = None;
    for env_addr in envs {
        let Some(callback) = callback_for(env_addr, EVENT_CLASS_FILE_LOAD_HOOK, tid) else {
            continue;
        };
        let data = current.as_deref().unwrap_or(bytes);
        let Ok(data_len) = JInt::try_from(data.len()) else {
            break;
        };
        // SAFETY: the agent registered this `jvmtiEventCallbacks` member as
        // its `ClassFileLoadHook` callback.
        let callback = unsafe { std::mem::transmute::<usize, ClassFileLoadHookCallback>(callback) };
        let mut new_len: JInt = 0;
        let mut new_data: *mut u8 = std::ptr::null_mut();
        in_event_context(shared, tid, |jni_env, _thread| {
            let [class, loader, domain] = objects.locals();
            // SAFETY: a C function of the agent's with the
            // `ClassFileLoadHook` signature; `name_utf8` and `data` outlive
            // the call, the two out-parameters are this frame's.
            unsafe {
                callback(
                    env_addr as *mut JvmtiNativeEnv,
                    jni_env,
                    class,
                    loader,
                    name_utf8.as_ptr(),
                    domain,
                    data_len,
                    data.as_ptr(),
                    &mut new_len,
                    &mut new_data,
                );
            }
        });
        if new_data.is_null() {
            continue;
        }
        let replaced = usize::try_from(new_len).ok().map(|len| {
            // SAFETY: the agent's `Allocate` block of at least `new_len`
            // bytes, as JVMTI requires of `new_class_data`.
            unsafe { std::slice::from_raw_parts(new_data, len) }.to_vec()
        });
        // SAFETY: JVMTI requires `new_class_data` to come from `Allocate`,
        // whose blocks this table frees.
        unsafe { free_block(new_data) };
        // An empty answer changes nothing.
        if let Some(replaced) = replaced.filter(|r| !r.is_empty()) {
            current = Some(replaced);
        }
    }
    drop(objects);
    current
}

/// `Vm::new`, once the VM and its main thread exist: bind the envs the
/// startup agents obtained in `Agent_OnLoad` ([`collect_startup_envs`]) to
/// `shared`. The VM is then in JVMTI's start phase (wave 14) until it reaches
/// the live phase at `SharedVm::set_init_level(3)`, after `System.initPhase2`
/// ([`enter_live_phase`]), which sends `VMInit` — HotSpot posts it after
/// `initPhase3`, with `System` initialized and the main thread's
/// `java.lang.Thread` in hand. Until wave 14 `VMInit` was sent from here,
/// before `System` was initialized and with a NULL thread. In the start
/// phase `ClassPrepare` is delivered, `Breakpoint` and `Exception` are not
/// (JVMTI posts them in the live phase only), `SetBreakpoint` and the
/// OnLoad-or-live functions answer `JVMTI_ERROR_WRONG_PHASE`, and the class
/// functions work, as in HotSpot.
///
/// A VM whose launcher never reaches level 3 would stay in the start phase,
/// so the phase is entered only when a startup agent is there to see it: an
/// env obtained later through `GetEnv` in a VM with no startup agent acts
/// on the VM as live, as before wave 14.
pub(crate) fn bind_startup_envs(shared: &Arc<SharedVm>) {
    let envs = std::mem::take(&mut shared.debug.jvmti_env.lock().c_table.startup_native_envs);
    if envs.is_empty() {
        return;
    }
    // Wave 61 (lane L1): what the agents set and added in `Agent_OnLoad`.
    let effects = std::mem::take(&mut shared.debug.jvmti_env.lock().startup_onload_effects);
    apply_onload_effects(shared, effects);
    // Wave 55 (`DebuggerGates::c_jvmti_phases`): HotSpot's primordial phase
    // until `System.initPhase1` returns (init level 2, [`enter_start_phase`]);
    // without the switch, the start phase at once, as since wave 14.
    let gates = &shared.debug.debugger_gates;
    let level = shared.get_init_level();
    if level < 3 {
        let phase = if gates.c_jvmti_phases() && level < 2 {
            crate::debug::JVMTI_PHASE_PRIMORDIAL
        } else {
            crate::debug::JVMTI_PHASE_START
        };
        gates.advance_jvmti_phase(crate::debug::JVMTI_PHASE_LIVE, phase);
    }
    // Wave 46 (lane L1): what the startup envs acquired of [`ONLOAD_ONLY`]
    // stays potential in the live phase, for every env of the VM.
    let mut acquired = [0u32; CAPABILITY_WORDS];
    for &env_addr in &envs {
        // SAFETY: an address `new_env_for_version` handed out.
        let held = unsafe { env_at(env_addr) }.state.lock().capabilities;
        for ((got, have), only) in acquired.iter_mut().zip(held).zip(ONLOAD_ONLY) {
            *got |= have & only;
        }
    }
    {
        let mut jvmti = shared.debug.jvmti_env.lock();
        for (record, got) in jvmti.c_table.onload_acquired.iter_mut().zip(acquired) {
            *record |= got;
        }
    }
    note_onload_local_access(shared, &acquired);
    // t1 wave 3, lane M: `can_tag_objects` is not OnLoad-only, so it is not
    // in `acquired`; a startup env holding it marks the VM as walking its
    // heap before the first compile.
    if envs.iter().any(|&addr| {
        // SAFETY: an address `new_env_for_version` handed out.
        has_cap(&unsafe { env_at(addr) }.state.lock().capabilities, CAP_TAG_OBJECTS)
    }) {
        shared.debug.debugger_gates.note_c_agent_walks_heap();
    }
    // Interpreter round i2 wave 63, lane L1: the two early capabilities
    // HotSpot grants in the OnLoad phase (MEASURED,
    // `L1W63JvmtiEarlyHooks`): with `can_generate_early_class_hook_events`
    // (and `can_generate_all_class_hook_events`) the hook is offered the
    // bootstrap classes of the primordial phase; with
    // `can_generate_early_vmstart`, `VMStart` comes at once instead of after
    // `System.initPhase1`.
    let held = |n: usize| {
        envs.iter()
            // SAFETY: addresses `new_env_for_version` handed out.
            .any(|&addr| has_cap(&unsafe { env_at(addr) }.state.lock().capabilities, n))
    };
    let early_hooks = held(CAP_EARLY_CLASS_HOOK_EVENTS) && gates.c_class_hooks();
    let early_vmstart = held(CAP_EARLY_VMSTART);
    shared.debug.jvmti_env.lock().early_class_hooks = early_hooks;
    // Interpreter round i3 wave 83, lane L1: a `NativeMethodBind` enabled in
    // `Agent_OnLoad` starts the funnel's tracking of the registered natives
    // as they first run (no Java code has run yet, so none is linked);
    // `events_more::refresh_event_gates`, from `attach_to`, reads the window.
    shared.debug.jvmti_env.lock().rust_native_bind_startup = true;
    for env_addr in &envs {
        // SAFETY: an address `new_env_for_version` handed out.
        unsafe { env_at(*env_addr) }.attach_to(shared);
    }
    shared.debug.jvmti_env.lock().rust_native_bind_startup = false;
    if early_hooks && gates.jvmti_phase() == crate::debug::JVMTI_PHASE_PRIMORDIAL {
        replay_early_class_hooks(shared);
    }
    if early_vmstart {
        enter_start_phase(shared);
    }
}

/// [`bind_startup_envs`]: when the startup envs `acquired` (of
/// [`ONLOAD_ONLY`]) hold `can_access_local_variables`, every local of
/// `shared` is a GC root from now on, as HotSpot keeps every local alive
/// under that capability (`JvmtiExport::can_access_local_variables`).
/// Interpreter round i6 wave 132, lane L1 (i126-L1's last hole;
/// `CRATONVM_JVMTI_C_AGENT_KEEPS_LOCALS_ALIVE`, default on, `=0` off):
/// `jvmti::debugger_observes_locals` answers true, so the root scans and
/// blocking deposits root a liveness-dead reference local too and a blocked
/// thread's dead slot is read through its tracked address
/// (`interpreter::blocked_dead_slots_read`, `BlockedFrames::current_reference`):
/// `LockSupport.park(Object)`'s `blocker` answers the blocker to
/// `GetLocalObject` rather than `null` (`L1W132JvmtiParkBlockerLocal`).
/// Sound because it runs in `Vm::new` before any Java frame exists, so no
/// slot was ever left out of a scan; it is never undone (the capability is
/// OnLoad-only, and a relinquished one does not make a dead referent safe
/// to answer again).
fn note_onload_local_access(shared: &SharedVm, acquired: &[u32; CAPABILITY_WORDS]) {
    if acquired[0] & CAN_ACCESS_LOCAL_VARIABLES != 0 {
        shared.debug.debugger_gates.note_c_agent_observes_locals();
    }
    if onload_acquired_observes_monitors(acquired) {
        shared.debug.debugger_gates.note_c_agent_observes_monitors();
    }
}

/// t1 wave 2, lane M: does `acquired` (of [`ONLOAD_ONLY`]) hold a capability
/// under which HotSpot's C2 annotates safepoints for its escape barrier
/// (`ConnectionGraph::compute_escape`: `should_retain_local_variables`, i.e.
/// `can_access_local_variables`, or `jvmti_can_get_owned_monitor_info`, which
/// `JvmtiManageCapabilities::update` sets from either owned-monitor
/// capability)? An eliminated lock is then relocked before the owned-monitor
/// answers, so it is listed; the JIT keeps the lock instead
/// (`DebuggerGates::c_agent_observes_monitors`).
fn onload_acquired_observes_monitors(acquired: &[u32; CAPABILITY_WORDS]) -> bool {
    acquired[0] & (CAN_ACCESS_LOCAL_VARIABLES | CAN_GET_OWNED_MONITOR_INFO) != 0
        || acquired[1] & CAN_GET_OWNED_MONITOR_STACK_DEPTH_INFO != 0
}

/// The early class hook (interpreter round i2 wave 63, lane L1;
/// `can_generate_early_class_hook_events`): the bootstrap classes this VM
/// loaded before it bound its startup agents (`SharedVm::new` loads
/// `java.lang.Object`, `Class`, `String` and the boxes before any agent can
/// be bound) offered to the hook now, in class-store order, each with the
/// class file it was defined from (MEASURED: HotSpot offers
/// `java/lang/Object` before `VMStart`). A class file the hook replaces
/// is installed as a redefinition (`RedefineClasses`' path), which takes
/// what a redefinition may change; nothing of these classes ran yet.
fn replay_early_class_hooks(shared: &Arc<SharedVm>) {
    let loaded: Vec<(ClassId, Arc<str>, Vec<u8>)> = {
        let cm = shared.classes.class_manager.read();
        cm.class_store
            .iter()
            .filter(|c| {
                c.loader_id == crate::classloading::ClassLoaderId::Bootstrap
                    && !c.name.starts_with('[')
            })
            .filter_map(|c| {
                crate::debug::defined_class_file(&cm, c.id, &c.name)
                    .map(|b| (c.id, Arc::clone(&c.name), b))
            })
            .collect()
    };
    let mut replaced: Vec<(ClassId, Vec<u8>)> = Vec::new();
    for (class_id, name, bytes) in loaded {
        let mut current: Option<Vec<u8>> = None;
        for retransformable in [false, true] {
            let offered = current.as_deref().unwrap_or(&bytes).to_vec();
            if let Some(answer) = post_class_file_load_hook(
                shared,
                0,
                &name,
                [None, None, None],
                &offered,
                retransformable,
            ) {
                current = Some(answer);
            }
        }
        if let Some(answer) = current {
            replaced.push((class_id, answer));
        }
    }
    if replaced.is_empty() {
        return;
    }
    let Some(addr) = shared
        .threads
        .thread_registry
        .own_jvm_thread_addr(ThreadId(0))
    else {
        return;
    };
    // gci i18/na: not under `safe_native_call_impl`; the redefinition
    // allocates, so it runs inside the scope and a refused allocation keeps
    // the original classes (warned) instead of the infallible arm's abort.
    let redefined = crate::runtime::native_oom::catch_alloc_oom_on(
        // SAFETY: the main thread's published `JvmThread`; `Vm::new` binds
        // the startup agents on it, and nothing else runs it meanwhile.
        unsafe { &mut *(addr as *mut JvmThread) },
        |thread| {
            let mut ctx = crate::vm::vm_exec::NativeContextImpl { shared, thread };
            crate::runtime::instrument::redefine_classes_for_jvmti(&mut ctx, &replaced)
        },
    );
    match redefined {
        Ok(Some(refused)) => {
            tracing::warn!(
                "early ClassFileLoadHook: a replaced class file was refused: {refused:?}"
            );
        }
        Ok(None) => {}
        Err(oom) => {
            tracing::warn!(
                "early ClassFileLoadHook: redefinition failed: java.lang.OutOfMemoryError: {}",
                oom.java_message()
            );
        }
    }
}

/// `SharedVm::set_init_level` reached 3 while the VM's C envs are in the
/// start phase ([`bind_startup_envs`]): enter the live phase, once per VM —
/// the [`MANAGER_EVENTS`] enables of every native env of the VM are
/// armed — and send `VMInit` to each env that has it enabled, on the calling
/// (main, id 0) thread. Needs the VM's `Arc` (every `Vm::new`-built VM has
/// its self-reference); without it the phase is left as it is.
pub(crate) fn enter_live_phase(shared: &SharedVm) {
    let Some(shared) = shared.try_get_arc() else {
        return;
    };
    // Wave 55: a VM still primordial (a launcher that raised the level to 3
    // without 2, as one without a JDK does) passes through the start phase
    // first, so `VMStart` precedes `VMInit`, as in HotSpot.
    enter_start_phase(&shared);
    let gates = &shared.debug.debugger_gates;
    if !gates.advance_jvmti_phase(
        crate::debug::JVMTI_PHASE_START,
        crate::debug::JVMTI_PHASE_LIVE,
    ) {
        // Another thread made the transition.
        return;
    }
    post_vm_init(&shared, 0);
}

/// `SharedVm::set_init_level` reached 2 (`System.initPhase1` returned)
/// while the VM's C envs are in the primordial phase ([`bind_startup_envs`]
/// with `DebuggerGates::c_jvmti_phases`; interpreter round i2 wave 55, lane
/// L1): enter the start phase, once per VM — `ClassLoad`, `ClassPrepare`
/// and (with `DebuggerGates::c_start_phase_hook`) `ClassFileLoadHook` are
/// armed from here on, as HotSpot posts none of them in the primordial phase
/// — and send `VMStart` to each env that has it enabled, on the calling
/// (main, id 0) thread, as `JvmtiExport::post_vm_start` does before
/// `System.initPhase2`. Nothing in any other phase.
pub(crate) fn enter_start_phase(shared: &SharedVm) {
    let gates = &shared.debug.debugger_gates;
    if !gates.advance_jvmti_phase(
        crate::debug::JVMTI_PHASE_PRIMORDIAL,
        crate::debug::JVMTI_PHASE_START,
    ) {
        return;
    }
    refresh_class_prepare_gate(shared);
    let tid = 0;
    let envs = shared.debug.jvmti_env.lock().c_table.native_envs.clone();
    for env_addr in envs {
        let Some(callback) = callback_for(env_addr, EVENT_VM_START, tid) else {
            continue;
        };
        // SAFETY: the agent registered this `jvmtiEventCallbacks` member as
        // its `VMStart` callback, whose signature is `VMDeath`'s.
        let callback = unsafe { std::mem::transmute::<usize, VmDeathCallback>(callback) };
        in_event_context(shared, tid, |jni_env, _thread| {
            // SAFETY: a C function of the agent's with the `VMStart` signature.
            unsafe { callback(env_addr as *mut JvmtiNativeEnv, jni_env) };
        });
    }
}

/// The live phase's first act on `shared`'s native envs: arm the events the
/// start phase held back, then `VMInit` on thread `tid`.
fn post_vm_init(shared: &Arc<SharedVm>, tid: u64) {
    let envs = shared.debug.jvmti_env.lock().c_table.native_envs.clone();
    for &env_addr in &envs {
        // SAFETY: an address `new_env_for_version` handed out.
        let env = unsafe { env_at(env_addr) };
        for (event, _) in MANAGER_EVENTS {
            let _ = env.sync_event(shared, event);
        }
    }
    // Wave 53: `ClassFileLoadHook` is armed in the live phase only.
    refresh_class_prepare_gate(shared);
    for env_addr in envs {
        let Some(callback) = callback_for(env_addr, EVENT_VM_INIT, tid) else {
            continue;
        };
        // SAFETY: the agent registered this `jvmtiEventCallbacks` member as
        // its `VMInit` callback.
        let callback = unsafe { std::mem::transmute::<usize, VmInitCallback>(callback) };
        in_event_context(shared, tid, |jni_env, thread| {
            // SAFETY: a C function of the agent's with the `VMInit` signature.
            unsafe { callback(env_addr as *mut JvmtiNativeEnv, jni_env, thread) };
        });
    }
    // Interpreter round i5 wave 104 hand-back (lane L1;
    // `L1W104JvmtiDestroyJavaVmThreadEvents`, w104): HotSpot's
    // `jni_CreateJavaVM` posts `ThreadStart` for the main thread right after
    // `VMInit` (`JvmtiExport::post_thread_start(thread)`), so an agent that
    // enabled it in `Agent_OnLoad` sees `main` start; this table posted none.
    post_thread_event(shared, tid, true);
}

/// The VM is shutting down (`Drop for Vm`, right after the Rust manager's
/// `VMDeath`, on the thread `tid` dropping it): the `VMDeath` callback of
/// every native env of the VM that has it enabled, then the VM's agents'
/// `Agent_OnUnload`, as HotSpot orders them. The agent registry is taken out
/// of the VM first, so each agent is unloaded once and its `Agent_OnUnload`
/// may call back into this table (e.g. `DisposeEnvironment`, which takes
/// the VM's JVMTI lock).
pub(crate) fn shut_down_agents(shared: &SharedVm, tid: u64) {
    // `VMDeath` is a live-phase event (wave 14): a VM dropped before it
    // reached the live phase (`enter_live_phase`) sent no `VMInit` and sends
    // no `VMDeath`, as HotSpot does for a VM whose creation failed; nor
    // does one already dead (wave 55).
    let envs = if shared.debug.debugger_gates.jvmti_phase() != crate::debug::JVMTI_PHASE_LIVE {
        Vec::new()
    } else {
        shared.debug.jvmti_env.lock().c_table.native_envs.clone()
    };
    // Interpreter round i5 wave 104 hand-back (lane L1;
    // `L1W104JvmtiDestroyJavaVmThreadEvents`, w104): the `DestroyJavaVM`
    // thread's `ThreadEnd`, before `VMDeath`, as HotSpot posts it for the
    // thread that destroys the VM. Only on that thread (the launcher's, id 0)
    // and once it carries the `DestroyJavaVM` mirror
    // (`vm_exec::list_destroy_java_vm_thread`), with
    // `CRATONVM_DEBUG_DESTROY_JAVA_VM_IDENTITY` on.
    if !envs.is_empty()
        && tid == 0
        && shared.threads.thread_registry.destroy_java_vm_listed()
        && shared.debug.debug_state.lock().destroy_java_vm_identity
    {
        post_thread_event(shared, 0, false);
    }
    for env_addr in envs {
        let Some(callback) = callback_for(env_addr, EVENT_VM_DEATH, tid) else {
            continue;
        };
        // SAFETY: the agent registered this `jvmtiEventCallbacks` member as
        // its `VMDeath` callback.
        let callback = unsafe { std::mem::transmute::<usize, VmDeathCallback>(callback) };
        in_event_context(shared, tid, |jni_env, _thread| {
            // SAFETY: a C function of the agent's with the `VMDeath` signature.
            unsafe { callback(env_addr as *mut JvmtiNativeEnv, jni_env) };
        });
    }
    // Wave 55 (`DebuggerGates::c_jvmti_phases`): the dead phase once the
    // `VMDeath` callbacks returned, as `JvmtiExport::post_vm_death` sets it,
    // so `Agent_OnUnload`'s `GetPhase` answers it.
    let gates = &shared.debug.debugger_gates;
    if gates.c_jvmti_phases() {
        gates.advance_jvmti_phase(
            crate::debug::JVMTI_PHASE_LIVE,
            crate::debug::JVMTI_PHASE_DEAD,
        );
    }
    let mut agents = std::mem::take(&mut shared.debug.jvmti_env.lock().c_table.agent_registry);
    agents.unload_all();
}

/// The live VM thread a `jthread` handle names in `shared`; else
/// `JVMTI_ERROR_THREAD_NOT_ALIVE` for a thread the registry knows as
/// terminated, `JVMTI_ERROR_INVALID_THREAD` for anything else, as HotSpot's
/// `get_threadOop_and_JavaThread` answers (wave 19; a terminated thread was
/// accepted, and events were enabled for a thread that could never post
/// them). A not-yet-started `Thread` has no registry entry; since wave 45
/// it is `THREAD_NOT_ALIVE`, as HotSpot answers it (it was `INVALID_THREAD`;
/// measured, `tools/probes/interp/L1/L1W45JvmtiSuspendAndOtherStacks.java`).
///
/// Found by the mirror's unique `Thread.tid` first, as
/// `vm_exec::resolve_thread_id_from_thread_obj` does: a terminated thread's
/// retained entry may hold a mirror address a collection has since recycled
/// for a live `Thread`.
///
/// Decoded inside the JNI functions' foreign-thread entry (wave 19): an
/// agent's own attached thread calling outside an event callback is idle
/// (GC-blocked) between calls with `CRATONVM_JNI_FOREIGN_TRANSITIONS` on, so
/// without it a collection could move the mirror while the handle is read
/// and compared with the registry's (post-move) mirrors. Only the id leaves
/// the entry's scope; inert on every other thread and with the flag off.
fn thread_id_of(shared: &SharedVm, thread: JObject) -> Result<u64, JInt> {
    match lookup_thread(shared, thread)? {
        ThreadLookup::Live(tid) => Ok(tid),
        ThreadLookup::Dead | ThreadLookup::Unstarted => Err(ERR_THREAD_NOT_ALIVE),
    }
}

/// What a `jthread` handle names ([`lookup_thread`], interpreter round i1
/// wave 45, lane L1): `GetThreadState` tells a terminated thread from one
/// never started.
enum ThreadLookup {
    /// A live VM thread, by its id.
    Live(u64),
    /// A thread the registry knows as terminated.
    Dead,
    /// A `java.lang.Thread` the registry does not know: not started.
    Unstarted,
}

/// [`thread_id_of`]'s lookup, with the three answers apart;
/// `JVMTI_ERROR_INVALID_THREAD` for a handle that names no `Thread`.
fn lookup_thread(shared: &SharedVm, thread: JObject) -> Result<ThreadLookup, JInt> {
    // Interpreter round i6 wave 130, lane L1b (`CRATONVM_JVMTI_THREAD_LOOKUP_VM_ONLY`,
    // performance switch, default ON since i7, `=0` off): the lookup runs no Java, mints no
    // local and cannot collect (it decodes the handle, reads `Thread.tid` and
    // asks the registry), as the tag functions' `with_object_tags`. So from a
    // JNI native in native it leaves native quietly and goes back
    // incrementally, not with a full root deposit per call -- the bulk of a
    // sampler's `GetThreadState` (6.3 us against HotSpot's 159 ns). Page:
    // docs/feature-designs/jvmti-delivery-threading.md.
    let _fx = if shared.debug.debugger_gates.jvmti_thread_lookup_vm_only() {
        jni::ForeignJniEntry::enter_vm_only()
    } else {
        jni::ForeignJniEntry::enter()
    };
    // The handle is resolved in `shared`'s JNI context (an agent thread
    // outside a callback has none).
    let prev = jni::replace_jni_context(shared);
    let obj = jni::jobject_to_obj(thread);
    jni::restore_jni_context(prev);
    let obj = obj.ok_or(ERR_INVALID_THREAD)?;
    let registry = &shared.threads.thread_registry;
    let tid = match crate::vm::vm_exec::read_java_thread_tid(shared, obj) {
        Some(java_tid) => registry
            .find_thread_id_by_java_tid(java_tid)
            .or_else(|| registry.find_thread_id_by_thread_obj_tid_checked(obj, java_tid)),
        None => registry.find_thread_id_by_thread_obj(obj),
    };
    match tid {
        Some(tid) if registry.is_alive(tid) => Ok(ThreadLookup::Live(tid.0)),
        Some(_) => Ok(ThreadLookup::Dead),
        None if is_thread_object(shared, obj) => Ok(ThreadLookup::Unstarted),
        None => Err(ERR_INVALID_THREAD),
    }
}

/// Is `obj` a `java.lang.Thread` (wave 45)? Read inside the caller's
/// foreign-thread entry, as the handle is.
fn is_thread_object(shared: &SharedVm, obj: ObjectRef) -> bool {
    let class_id = shared.mem.heap.class_id_of(obj);
    shared
        .classes
        .class_manager
        .read()
        .is_subclass_of_by_name(class_id, "java/lang/Thread")
}

/// The loaded class a `jclass` handle names in `shared`, if any: a `jclass`
/// or a `java.lang.Class` mirror, nothing else (wave 19: JNI's decode falls
/// back to the handle's low 32 bits, which named an arbitrary class for any
/// other handle, so `GetClassSignature` of a `String` could answer some
/// class's signature where HotSpot answers `JVMTI_ERROR_INVALID_CLASS`). A
/// mirror handle is read inside the foreign-thread entry, as in
/// [`thread_id_of`].
fn class_of(shared: &SharedVm, klass: JObject) -> Option<ClassId> {
    if klass == 0 {
        return None;
    }
    let fx = jni::ForeignJniEntry::enter();
    let prev = jni::replace_jni_context(shared);
    let class_id = jni::jclass_class_id_exact(klass);
    jni::restore_jni_context(prev);
    drop(fx);
    let class_id = class_id?;
    let loaded = shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .is_some();
    loaded.then_some(class_id)
}

/// Slot 2: `SetEventNotificationMode(env, mode, event_type, event_thread,
/// ...)`, for the events this table delivers (`VMInit`, `VMDeath`, `ClassPrepare`,
/// `Exception`, `Breakpoint`), globally (a NULL thread) or for one thread;
/// another event is `JVMTI_ERROR_NOT_AVAILABLE`. Allowed in `Agent_OnLoad`
/// for all threads: the event is armed when the VM binds the env.
extern "C" fn set_event_notification_mode(
    env: *mut JvmtiNativeEnv,
    mode: JInt,
    event_type: JInt,
    event_thread: JObject,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    // OnLoad or live phase only, checked first as HotSpot does.
    if env.outside_live_phase() {
        return ERR_WRONG_PHASE;
    }
    // `JVMTI_ENABLE`; any other mode disables, as HotSpot reads it
    // (`enabled = (mode == JVMTI_ENABLE)`; interpreter round i2 wave 61, lane
    // L1, MEASURED: `L1W61JvmtiEventAndBreakpointErrors`' `mode 7` row is 0
    // on HotSpot 25.0.3, `ILLEGAL_ARGUMENT` here until then).
    let enable = mode == 1;
    // Wave 61: HotSpot's valid range starts at its extension events (47-49,
    // `JvmtiEventController::is_valid_event_type`; MEASURED: event 49 is 0,
    // 89 `INVALID_EVENT_TYPE`).
    if !(MIN_EXTENSION_EVENT..=MAX_EVENT).contains(&event_type) {
        return ERR_INVALID_EVENT_TYPE;
    }
    // Wave 61: a reserved event number (MEASURED: 72 and 85 are 0 on
    // HotSpot, with no capability): accepted with no effect. Wave 63: the
    // extension events (47-49) are enabled here and reach a callback
    // `SetExtensionEventCallback` sets (`extensions`).
    if RESERVED_EVENTS.contains(&event_type) {
        if event_thread != 0 {
            let Some(shared) = env.vm() else {
                return ERR_WRONG_PHASE;
            };
            if let Err(code) = thread_id_of(&shared, event_thread) {
                return code;
            }
        }
        return ERR_NONE;
    }
    let Some(capability) = event_capability_set(event_type) else {
        return ERR_NOT_AVAILABLE;
    };
    // Wave 52: `ExceptionCatch` only while the VM's switch is on.
    if event_type == EVENT_EXCEPTION_CATCH
        && env
            .vm()
            .is_some_and(|shared| !shared.debug.debugger_gates.c_exception_catch())
    {
        return ERR_NOT_AVAILABLE;
    }
    // Wave 53: `ClassFileLoadHook` and `ClassLoad` only while the VM's
    // switch is on.
    if matches!(event_type, EVENT_CLASS_FILE_LOAD_HOOK | EVENT_CLASS_LOAD)
        && env
            .vm()
            .is_some_and(|shared| !shared.debug.debugger_gates.c_class_hooks())
    {
        return ERR_NOT_AVAILABLE;
    }
    // Wave 55: `VMStart` only while the VM's switch is on (in
    // `Agent_OnLoad`, before the VM's gates exist, the switch they will
    // read).
    if event_type == EVENT_VM_START
        && !env
            .vm()
            .map_or_else(crate::debug::c_jvmti_phases_switch, |shared| {
                shared.debug.debugger_gates.c_jvmti_phases()
            })
    {
        return ERR_NOT_AVAILABLE;
    }
    if enable && {
        let held = env.state.lock().capabilities;
        held.iter().zip(capability).any(|(&h, c)| h & c != c)
    } {
        return ERR_MUST_POSSESS_CAPABILITY;
    }
    let thread = if event_thread == 0 {
        None
    } else {
        // Threads exist only in the live phase.
        let Some(shared) = env.vm() else {
            return ERR_WRONG_PHASE;
        };
        match thread_id_of(&shared, event_thread) {
            Ok(tid) => Some(tid),
            Err(code) => return code,
        }
    };
    // HotSpot's global events (`JvmtiEventController::is_global_event`) are
    // not controllable per thread, checked once the thread is decoded:
    // `VMInit`, `VMDeath` and (interpreter round i2 wave 53, lane L1,
    // measured: `L1W53JvmtiClassHooks`' `per thread` row) `ThreadStart` and
    // `ClassFileLoadHook`; `ThreadEnd`, `ClassLoad` and `ClassPrepare` are.
    // (Wave 55: `VMStart`, global as `VMInit` is.)
    if thread.is_some()
        && (matches!(
            event_type,
            EVENT_VM_INIT
                | EVENT_VM_START
                | EVENT_VM_DEATH
                | EVENT_THREAD_START
                | EVENT_CLASS_FILE_LOAD_HOOK
        ) || events_more::is_global_event(event_type))
    {
        return ERR_ILLEGAL_ARGUMENT;
    }
    {
        let mut state = env.state.lock();
        let bit = event_bit(event_type);
        match (thread, enable) {
            (None, true) => state.enabled |= bit,
            (None, false) => state.enabled &= !bit,
            (Some(tid), _) => {
                state.thread_enabled.retain(|&set| set != (event_type, tid));
                if enable {
                    state.thread_enabled.push((event_type, tid));
                }
            }
        }
    }
    // Before the VM exists (`Agent_OnLoad`) there is nothing to arm yet.
    let Some(shared) = env.vm() else {
        return ERR_NONE;
    };
    // Enabling and disabling alike (wave 14: a disable used to leave the
    // event enabled on the VM's manager).
    env.sync_event(&shared, event_type)
}

/// Slot 122: `SetEventCallbacks(env, callbacks, size_of_callbacks)`. Copies
/// the agent's structure (as much of it as this table knows); NULL clears.
extern "C" fn set_event_callbacks(
    env: *mut JvmtiNativeEnv,
    callbacks: *const u8,
    size_of_callbacks: JInt,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    // OnLoad or live phase only.
    if env.outside_live_phase() {
        return ERR_WRONG_PHASE;
    }
    let Ok(size) = usize::try_from(size_of_callbacks) else {
        return ERR_ILLEGAL_ARGUMENT;
    };
    let mut slots = [0usize; CALLBACK_SLOTS];
    if !callbacks.is_null() {
        let bytes = size.min(std::mem::size_of_val(&slots));
        // SAFETY: the agent's structure is `size_of_callbacks` bytes long,
        // and `bytes` is no more than that or than `slots`.
        unsafe {
            std::ptr::copy_nonoverlapping(callbacks, slots.as_mut_ptr().cast::<u8>(), bytes);
        }
    }
    env.state.lock().callbacks = slots;
    ERR_NONE
}

/// Slot 38: `SetBreakpoint(env, method, location)`, through
/// `runtime::jvmti::set_breakpoint_for_vm` (`INVALID_METHODID`,
/// `INVALID_LOCATION`, `DUPLICATE` as the spec says). The event reaches the
/// agent while it has `Breakpoint` enabled.
extern "C" fn set_breakpoint(env: *mut JvmtiNativeEnv, method: u64, location: i64) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    // Live phase only (wave 14: also not in the start phase), checked
    // before the capability as HotSpot does.
    let Some(shared) = env.vm() else {
        return ERR_WRONG_PHASE;
    };
    if shared.debug.debugger_gates.jvmti_outside_live() {
        return ERR_WRONG_PHASE;
    }
    if !env.possesses(CAN_GENERATE_BREAKPOINT_EVENTS) {
        return ERR_MUST_POSSESS_CAPABILITY;
    }
    // Interpreter round i2 wave 61, lane L1: a NULL `jmethodID` decodes to
    // method 0 of class id 0, a stored class (MEASURED on HotSpot 25.0.3:
    // `INVALID_METHODID`, `L1W61JvmtiEventAndBreakpointErrors`).
    if method == 0 {
        return ERR_INVALID_METHODID;
    }
    if let Err(error) = rt::set_breakpoint_for_vm(&shared, method, location) {
        return error as JInt;
    }
    env.state.lock().breakpoints.push((method, location));
    ERR_NONE
}

/// Slot 39: `ClearBreakpoint(env, method, location)` (`NOT_FOUND` for one
/// not set).
extern "C" fn clear_breakpoint(env: *mut JvmtiNativeEnv, method: u64, location: i64) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    // Live phase only (wave 14: also not in the start phase), checked
    // before the capability as HotSpot does.
    let Some(shared) = env.vm() else {
        return ERR_WRONG_PHASE;
    };
    if shared.debug.debugger_gates.jvmti_outside_live() {
        return ERR_WRONG_PHASE;
    }
    if !env.possesses(CAN_GENERATE_BREAKPOINT_EVENTS) {
        return ERR_MUST_POSSESS_CAPABILITY;
    }
    // Interpreter round i2 wave 61, lane L1: a NULL `jmethodID` decodes to
    // method 0 of class id 0, a stored class (MEASURED on HotSpot 25.0.3:
    // `INVALID_METHODID`, `L1W61JvmtiEventAndBreakpointErrors`).
    if method == 0 {
        return ERR_INVALID_METHODID;
    }
    if let Err(error) = rt::clear_breakpoint_for_vm(&shared, method, location) {
        return error as JInt;
    }
    env.state
        .lock()
        .breakpoints
        .retain(|&set| set != (method, location));
    ERR_NONE
}

/// The bytes in front of every `Allocate` block: its layout size, which
/// `Deallocate` needs back.
const BLOCK_HEADER: usize = 16;

fn allocate_block(size: usize) -> *mut u8 {
    let Some(total) = size.checked_add(BLOCK_HEADER) else {
        return std::ptr::null_mut();
    };
    let Ok(layout) = std::alloc::Layout::from_size_align(total, BLOCK_HEADER) else {
        return std::ptr::null_mut();
    };
    // SAFETY: `layout` has a non-zero size (at least the header).
    let base = unsafe { std::alloc::alloc(layout) };
    if base.is_null() {
        return base;
    }
    // SAFETY: `base` is a fresh block of `total >= BLOCK_HEADER` bytes,
    // aligned for a `usize`.
    unsafe {
        base.cast::<usize>().write(total);
        base.add(BLOCK_HEADER)
    }
}

/// # Safety
///
/// `mem` came from [`allocate_block`] and was not freed since.
unsafe fn free_block(mem: *mut u8) {
    let base = mem.sub(BLOCK_HEADER);
    let total = base.cast::<usize>().read();
    std::alloc::dealloc(
        base,
        std::alloc::Layout::from_size_align_unchecked(total, BLOCK_HEADER),
    );
}

/// `text` as a NUL-terminated modified UTF-8 string in an `Allocate` block
/// (NUL as `C0 80`, a supplementary character as its two surrogates), or
/// null when out of memory.
fn allocate_modified_utf8(text: &str) -> *mut u8 {
    let bytes = modified_utf8(text);
    let mem = allocate_block(bytes.len());
    if !mem.is_null() {
        // SAFETY: `mem` is a fresh block of `bytes.len()` bytes.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), mem, bytes.len()) };
    }
    mem
}

/// `text` as NUL-terminated modified UTF-8 (NUL as `C0 80`, a supplementary
/// character as its two surrogates).
fn modified_utf8(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() + 1);
    for c in text.chars() {
        let code = u32::from(c);
        if code == 0 {
            bytes.extend_from_slice(&[0xC0, 0x80]);
        } else if code > 0xFFFF {
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units).iter() {
                let u = u32::from(*unit);
                // Truncation: each byte takes the bits its mask selects.
                bytes.push((0xE0 | (u >> 12)) as u8);
                bytes.push((0x80 | ((u >> 6) & 0x3F)) as u8);
                bytes.push((0x80 | (u & 0x3F)) as u8);
            }
        } else {
            let mut buf = [0u8; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    bytes.push(0);
    bytes
}

/// Slot 46: `Allocate(env, size, mem_ptr)`.
extern "C" fn allocate(env: *mut JvmtiNativeEnv, size: i64, mem_ptr: *mut *mut u8) -> JInt {
    if let Err(code) = live_env(env) {
        return code;
    }
    if mem_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let Ok(size) = usize::try_from(size) else {
        return ERR_ILLEGAL_ARGUMENT;
    };
    let mem = if size == 0 {
        std::ptr::null_mut()
    } else {
        let mem = allocate_block(size);
        if mem.is_null() {
            return ERR_OUT_OF_MEMORY;
        }
        mem
    };
    // SAFETY: `mem_ptr` is the agent's non-null out-parameter.
    unsafe { *mem_ptr = mem };
    ERR_NONE
}

/// Slot 47: `Deallocate(env, mem)`: a block `Allocate` (or a function of
/// this table returning memory) handed out; NULL is ignored.
extern "C" fn deallocate(env: *mut JvmtiNativeEnv, mem: *mut u8) -> JInt {
    if let Err(code) = live_env(env) {
        return code;
    }
    if !mem.is_null() {
        // SAFETY: the spec allows only memory this env allocated here.
        unsafe { free_block(mem) };
    }
    ERR_NONE
}

/// The JVM type signature of the class whose binary name (internal form) is
/// `name`: `Ljava/lang/String;`, an array's own name, a primitive's letter.
fn class_signature(name: &str) -> String {
    let primitive = match name {
        "boolean" => "Z",
        "byte" => "B",
        "char" => "C",
        "short" => "S",
        "int" => "I",
        "long" => "J",
        "float" => "F",
        "double" => "D",
        "void" => "V",
        _ if name.starts_with('[') => name,
        _ => return format!("L{name};"),
    };
    primitive.to_string()
}

/// Slot 48: `GetClassSignature(env, klass, signature_ptr, generic_ptr)`.
/// Either out-parameter may be NULL. The generic signature is the class's
/// `Signature` attribute (JVMS §4.7.9), NULL when it has none, as the JVMTI
/// specification and HotSpot answer it; it was always NULL until interpreter
/// round i1 wave 41 (lane L1).
extern "C" fn get_class_signature(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    signature_ptr: *mut *mut u8,
    generic_ptr: *mut *mut u8,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let Some(shared) = env.start_or_live_vm() else {
        return ERR_WRONG_PHASE;
    };
    let (signature, generic) = match jvmti_class(&shared, klass) {
        Ok(JvmtiClass::Stored(class_id)) => {
            let Some((name, generic)) = shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .map(|c| (c.name.to_string(), c.signature.clone()))
            else {
                return ERR_INVALID_CLASS;
            };
            (class_signature(&name), generic)
        }
        // Wave 55: a lambda proxy class, with no generic signature.
        Ok(JvmtiClass::LambdaProxy(id)) => match lambda_proxy_signature(&shared, id) {
            Some(signature) => (signature, None),
            None => return ERR_INVALID_CLASS,
        },
        // Wave 56: a primitive class's mirror, its letter (measured).
        Ok(JvmtiClass::Primitive(name)) => (class_signature(&name), None),
        Err(code) => return code,
    };
    let out = [
        (signature_ptr, Some(signature.as_str())),
        (generic_ptr, generic.as_deref()),
    ];
    if write_modified_utf8_outs(&out) {
        ERR_NONE
    } else {
        ERR_OUT_OF_MEMORY
    }
}

/// Store each `(out-parameter, text)` of a JVMTI function that answers
/// strings: a fresh modified-UTF-8 `Allocate` block for `Some(text)`, NULL for
/// `None`, nothing for a NULL out-parameter. All or nothing: `false` (and no
/// out-parameter written, no block leaked) when a block cannot be allocated.
fn write_modified_utf8_outs(out: &[(*mut *mut u8, Option<&str>)]) -> bool {
    let mut blocks = vec![std::ptr::null_mut::<u8>(); out.len()];
    let mut short = false;
    for (block, &(ptr, text)) in blocks.iter_mut().zip(out) {
        if let (false, Some(text)) = (ptr.is_null(), text) {
            *block = allocate_modified_utf8(text);
            short |= block.is_null();
        }
    }
    if short {
        for block in blocks {
            if !block.is_null() {
                // SAFETY: allocated just above, not handed out.
                unsafe { free_block(block) };
            }
        }
        return false;
    }
    for (&(ptr, _), block) in out.iter().zip(blocks) {
        if !ptr.is_null() {
            // SAFETY: the agent's non-null out-parameter.
            unsafe { *ptr = block };
        }
    }
    true
}

/// The generic signature of the method a real `(class id << 32) | index`
/// `jmethodID` names: its `Signature` attribute (JVMS §4.7.9), decoded on the
/// fly if the class kept it raw; `None` when it has none.
fn method_generic_signature(shared: &SharedVm, method: u64) -> Option<String> {
    use cratonvm_reader::attribute::Attribute;
    // Cast: the id's halves, as `jni::decode_method_id` reads them.
    let class_id = ClassId::new((method >> 32) as u32);
    // Cast: the low 16 bits are the method index.
    let index = (method & 0xFFFF) as usize;
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(class_id)?;
    let m = class.methods.get(index)?;
    m.attributes.iter().find_map(
        |a| match a.decoded_or_decode(&class.constant_pool)?.as_ref() {
            Attribute::Signature(s) => Some(s.to_string()),
            _ => None,
        },
    )
}

/// Slot 52: `GetClassMethods(env, klass, method_count_ptr, methods_ptr)`:
/// the `jmethodID`s of the methods the class declares (constructors and the
/// static initializer included), in class-file order, in an `Allocate`
/// block — the ids `SetBreakpoint` and `GetMethodName` take;
/// `JVMTI_ERROR_CLASS_NOT_PREPARED` before the class is prepared (wave 52).
extern "C" fn get_class_methods(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    method_count_ptr: *mut JInt,
    methods_ptr: *mut *mut u64,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    // Interpreter round i4 wave 95 (lane L1): HotSpot's `jvmtiEnter` wrapper
    // checks the phase, then the parameters in declaration order -- the
    // class (`JVMTI_ERROR_INVALID_CLASS`), then each out-parameter
    // (`JVMTI_ERROR_NULL_POINTER`); the NULL check came first until then.
    let Some(shared) = env.start_or_live_vm() else {
        return ERR_WRONG_PHASE;
    };
    let class = jvmti_class(&shared, klass);
    if class.is_ok() && (method_count_ptr.is_null() || methods_ptr.is_null()) {
        return ERR_NULL_POINTER;
    }
    let class_id = match class {
        Ok(JvmtiClass::Stored(class_id)) => class_id,
        // Wave 56: a lambda proxy class's methods.
        Ok(JvmtiClass::LambdaProxy(id)) => {
            return match lambda_proxy_method_ids(&shared, id) {
                Some(ids) => write_id_array(method_count_ptr, methods_ptr, &ids),
                None => ERR_INVALID_CLASS,
            };
        }
        // Wave 56: a primitive class declares none (measured).
        Ok(JvmtiClass::Primitive(_)) => {
            return write_id_array::<u64>(method_count_ptr, methods_ptr, &[]);
        }
        Err(code) => return code,
    };
    // Wave 52: `JVMTI_ERROR_CLASS_NOT_PREPARED` before the class is
    // prepared, as HotSpot answers (measured, L1W52JvmtiThreadAndClassFacts).
    let count = match shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .map(|c| prepared(c).map(|()| c.methods.len()))
    {
        Some(Ok(count)) => count,
        Some(Err(code)) => return code,
        None => 0,
    };
    // `jni::decode_method_id` reads the index from the low 16 bits; a class
    // file declares at most 65535 methods.
    let ids: Vec<u64> = (0..count)
        .filter_map(|index| u16::try_from(index).ok())
        .map(|index| (u64::from(class_id.as_u32()) << 32) | u64::from(index))
        .collect();
    let Ok(count) = JInt::try_from(ids.len()) else {
        return ERR_INTERNAL;
    };
    let Some(bytes) = ids.len().checked_mul(std::mem::size_of::<u64>()) else {
        return ERR_OUT_OF_MEMORY;
    };
    let mem = allocate_block(bytes);
    if mem.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    // SAFETY: `mem` is a fresh block of `bytes` bytes, 16-byte aligned; the
    // out-parameters are the agent's, non-null.
    unsafe {
        std::ptr::copy_nonoverlapping(ids.as_ptr(), mem.cast::<u64>(), ids.len());
        *method_count_ptr = count;
        *methods_ptr = mem.cast::<u64>();
    }
    ERR_NONE
}

/// The declaring class and method of a real `(class id << 32) | index`
/// `jmethodID` in `shared`.
fn method_of(shared: &SharedVm, method: u64) -> Option<(ClassId, Arc<str>, Arc<str>)> {
    with_method(shared, method, |class_id, m| {
        (class_id, Arc::clone(&m.name), Arc::clone(&m.descriptor))
    })
}

/// [`method_of`], or (wave 56) the lambda proxy method a `jmethodID` names.
fn method_or_proxy_method(shared: &SharedVm, method: u64) -> Option<(ClassId, Arc<str>, Arc<str>)> {
    or_proxy_method(shared, method, method_of(shared, method), |class_id, m| {
        (class_id, Arc::clone(&m.name), Arc::clone(&m.descriptor))
    })
}

/// `read` of the declaring class and the method a real `(class id << 32) |
/// index` `jmethodID` names in `shared`, under the class-manager read lock;
/// `None` for an id that names no method, and for NULL (interpreter round
/// i2 wave 57: a NULL `jmethodID` decoded as method 0 of class id 0, a
/// stored class, where HotSpot answers `JVMTI_ERROR_INVALID_METHODID`;
/// MEASURED, `L1W57JvmtiLocalVariableTable`'s `null-method`).
fn with_method<R>(
    shared: &SharedVm,
    method: u64,
    read: impl FnOnce(ClassId, &cratonvm_reader::method::ClassFileMethod) -> R,
) -> Option<R> {
    if method == 0 {
        return None;
    }
    // Cast: the id's halves, as `jni::decode_method_id` reads them.
    let class_id = ClassId::new((method >> 32) as u32);
    // Cast: the low 16 bits are the method index.
    let index = (method & 0xFFFF) as usize;
    let cm = shared.classes.class_manager.read();
    let m = cm.get_class(class_id)?.methods.get(index)?;
    Some(read(class_id, m))
}

/// Slot 64: `GetMethodName(env, method, name_ptr, signature_ptr,
/// generic_ptr)`. Any out-parameter may be NULL. The generic signature is
/// the method's `Signature` attribute, NULL when it has none (always NULL
/// until wave 41; see [`get_class_signature`]).
extern "C" fn get_method_name(
    env: *mut JvmtiNativeEnv,
    method: u64,
    name_ptr: *mut *mut u8,
    signature_ptr: *mut *mut u8,
    generic_ptr: *mut *mut u8,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let Some(shared) = env.start_or_live_vm() else {
        return ERR_WRONG_PHASE;
    };
    let Some((_, name, descriptor)) = method_or_proxy_method(&shared, method) else {
        return ERR_INVALID_METHODID;
    };
    let generic = if generic_ptr.is_null() {
        None
    } else {
        method_generic_signature(&shared, method)
    };
    let out = [
        (name_ptr, Some(&*name)),
        (signature_ptr, Some(&*descriptor)),
        (generic_ptr, generic.as_deref()),
    ];
    if write_modified_utf8_outs(&out) {
        ERR_NONE
    } else {
        ERR_OUT_OF_MEMORY
    }
}

/// Slot 65: `GetMethodDeclaringClass(env, method, declaring_class_ptr)`.
extern "C" fn get_method_declaring_class(
    env: *mut JvmtiNativeEnv,
    method: u64,
    declaring_class_ptr: *mut JObject,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    // Interpreter round i4 wave 95 (lane L1): phase, then the method id, then
    // the out-parameter, as HotSpot's `jvmtiEnter` wrapper checks them (the
    // NULL check came first until then).
    let Some(shared) = env.start_or_live_vm() else {
        return ERR_WRONG_PHASE;
    };
    let Some((class_id, _, _)) = method_or_proxy_method(&shared, method) else {
        return ERR_INVALID_METHODID;
    };
    if declaring_class_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *declaring_class_ptr = jni::class_id_to_jclass(class_id) };
    ERR_NONE
}

/// The `jvmtiClassStatus` of `class`, as HotSpot's `jvmti_class_status`
/// answers it: an array class is `ARRAY` only, a primitive class `PRIMITIVE`,
/// any other has JDWP's bits (VERIFIED | PREPARED once linked, INITIALIZED,
/// ERROR; `debug::jdwp_class_status`, whose values are JVMTI's).
fn class_status(class: &crate::classloading::Class) -> JInt {
    const PRIMITIVES: [&str; 9] = [
        "boolean", "byte", "char", "short", "int", "long", "float", "double", "void",
    ];
    if class.name.starts_with('[') {
        return CLASS_STATUS_ARRAY;
    }
    if PRIMITIVES.contains(&&*class.name) {
        return CLASS_STATUS_PRIMITIVE;
    }
    JInt::try_from(crate::debug::jdwp_class_status(class)).unwrap_or(0)
}

/// Slot 49: `GetClassStatus(env, klass, status_ptr)` (interpreter round i1
/// wave 17).
extern "C" fn get_class_status(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    status_ptr: *mut JInt,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let Some(shared) = env.start_or_live_vm() else {
        return ERR_WRONG_PHASE;
    };
    let class_id = match jvmti_class(&shared, klass) {
        Ok(JvmtiClass::Stored(class_id)) => class_id,
        // Wave 55: a lambda proxy class; wave 56: a primitive class's mirror.
        Ok(JvmtiClass::LambdaProxy(_)) => return write_jint(status_ptr, Ok(LAMBDA_PROXY_STATUS)),
        Ok(JvmtiClass::Primitive(_)) => return write_jint(status_ptr, Ok(CLASS_STATUS_PRIMITIVE)),
        Err(code) => return code,
    };
    if status_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let Some(status) = shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .map(class_status)
    else {
        return ERR_INVALID_CLASS;
    };
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *status_ptr = status };
    ERR_NONE
}

/// Slot 66: `GetMethodModifiers(env, method, modifiers_ptr)`: the method's
/// access flags, [`RECOGNIZED_METHOD_MODIFIERS`] only (wave 17).
extern "C" fn get_method_modifiers(
    env: *mut JvmtiNativeEnv,
    method: u64,
    modifiers_ptr: *mut JInt,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let Some(shared) = env.start_or_live_vm() else {
        return ERR_WRONG_PHASE;
    };
    let modifiers = with_method(&shared, method, |_, m| {
        JInt::from(m.access_flags.bits() & RECOGNIZED_METHOD_MODIFIERS)
    });
    // Wave 56: a lambda proxy's method.
    let Some(modifiers) = or_proxy_method(&shared, method, modifiers, |_, m| {
        JInt::from(m.access_flags & RECOGNIZED_METHOD_MODIFIERS)
    }) else {
        return ERR_INVALID_METHODID;
    };
    if modifiers_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *modifiers_ptr = modifiers };
    ERR_NONE
}

/// Slot 76: `IsMethodNative(env, method, is_native_ptr)` (wave 17).
extern "C" fn is_method_native(
    env: *mut JvmtiNativeEnv,
    method: u64,
    is_native_ptr: *mut u8,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let Some(shared) = env.start_or_live_vm() else {
        return ERR_WRONG_PHASE;
    };
    let native = with_method(&shared, method, |_, m| m.is_native());
    // Wave 56: a lambda proxy's method is never native.
    let Some(native) = or_proxy_method(&shared, method, native, |_, _| false) else {
        return ERR_INVALID_METHODID;
    };
    if is_native_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null `jboolean` out-parameter.
    unsafe { *is_native_ptr = u8::from(native) };
    ERR_NONE
}

/// Slot 71: `GetMethodLocation(env, method, start_location_ptr,
/// end_location_ptr)` (wave 17): `0` and the last bytecode index, as
/// HotSpot answers; `-1` and `-1` for a method with no code (abstract);
/// `JVMTI_ERROR_NATIVE_METHOD` for a native one.
extern "C" fn get_method_location(
    env: *mut JvmtiNativeEnv,
    method: u64,
    start_location_ptr: *mut i64,
    end_location_ptr: *mut i64,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let Some(shared) = env.start_or_live_vm() else {
        return ERR_WRONG_PHASE;
    };
    let size = with_method(&shared, method, |_, m| {
        (!m.is_native()).then(|| m.code().map_or(0, |code| code.code.len()))
    });
    let Some(size) = size else {
        // Wave 56: a lambda proxy's method; wave 57: the range of its spun
        // code ([`proxy_method_code`]), else, as before, "class information
        // does not include method sizes".
        return match proxy_method_code(&shared, method) {
            Some(Ok(code)) => {
                write_method_location(start_location_ptr, end_location_ptr, code.len())
            }
            Some(Err(code)) => code,
            None => ERR_INVALID_METHODID,
        };
    };
    let Some(size) = size else {
        return ERR_NATIVE_METHOD;
    };
    write_method_location(start_location_ptr, end_location_ptr, size)
}

/// `GetMethodLocation`'s answer for a code array of `size` bytes: `0` and
/// the last bytecode index, `-1` and `-1` for none.
fn write_method_location(
    start_location_ptr: *mut i64,
    end_location_ptr: *mut i64,
    size: usize,
) -> JInt {
    if start_location_ptr.is_null() || end_location_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // A code array is at most 65535 bytes (JVMS §4.7.3).
    let size = i64::try_from(size).unwrap_or(i64::MAX);
    // SAFETY: the agent's non-null out-parameters.
    unsafe {
        *start_location_ptr = if size == 0 { -1 } else { 0 };
        *end_location_ptr = size - 1;
    }
    ERR_NONE
}

/// The spun code of the lambda proxy method `method` names (interpreter
/// round i2 wave 57, lane L1; `runtime::lambda_proxy::lambda_proxy_code`):
/// `None` when it names no proxy method, `JVMTI_ERROR_ABSENT_INFORMATION`
/// while `DebuggerGates::c_lambda_code` is off or the model cannot tell.
#[cold]
#[inline(never)]
fn proxy_method_code(shared: &SharedVm, method: u64) -> Option<Result<Vec<u8>, JInt>> {
    let (class_id, _) = proxy_method(shared, method)?;
    if !shared.debug.debugger_gates.c_lambda_code() {
        return Some(Err(ERR_ABSENT_INFORMATION));
    }
    // Cast: the low 32 bits, the method's index (`proxy_method`).
    let index = (method & 0xFFFF_FFFF) as usize;
    let code = crate::runtime::lambda_proxy::lambda_proxy_code(shared, class_id, index);
    // Positive control: `CRATONVM_FRAME_TRACE=1`, as `GetClassMethods`'.
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[JVMTI_C_LAMBDA_CODE] class={:#x} method={index} bytes={}",
            class_id.as_u32(),
            code.as_ref()
                .map_or(-1, |c| i64::try_from(c.len()).unwrap_or(-1))
        );
    }
    Some(code.ok_or(ERR_ABSENT_INFORMATION))
}

/// Slot 70: `GetLineNumberTable(env, method, entry_count_ptr, table_ptr)`
/// (wave 17; `can_get_line_numbers`): the method's `LineNumberTable` rows,
/// by start location, in an `Allocate` block; `JVMTI_ERROR_ABSENT_INFORMATION`
/// for a method without one, `JVMTI_ERROR_NATIVE_METHOD` for a native one.
extern "C" fn get_line_number_table(
    env: *mut JvmtiNativeEnv,
    method: u64,
    entry_count_ptr: *mut JInt,
    table_ptr: *mut *mut LineNumberRow,
) -> JInt {
    use cratonvm_reader::attribute::Attribute;
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let Some(shared) = env.start_or_live_vm() else {
        return ERR_WRONG_PHASE;
    };
    if !env.possesses(CAN_GET_LINE_NUMBERS) {
        return ERR_MUST_POSSESS_CAPABILITY;
    }
    let Some(rows) = with_method(&shared, method, |_, m| {
        if m.is_native() {
            return Err(ERR_NATIVE_METHOD);
        }
        let mut rows: Option<Vec<LineNumberRow>> = None;
        for attribute in m
            .code()
            .map(|code| &code.attributes[..])
            .unwrap_or_default()
        {
            if let Attribute::LineNumberTable(entries) = attribute {
                rows.get_or_insert_with(Vec::new)
                    .extend(entries.iter().map(|e| LineNumberRow {
                        start_location: i64::from(e.start_pc),
                        line_number: JInt::from(e.line_number),
                    }));
            }
        }
        let mut rows = rows.ok_or(ERR_ABSENT_INFORMATION)?;
        rows.sort_by_key(|row| row.start_location);
        Ok(rows)
    }) else {
        // Wave 56: a lambda proxy's method has no `LineNumberTable`
        // (measured 101).
        if proxy_method(&shared, method).is_some() {
            return ERR_ABSENT_INFORMATION;
        }
        return ERR_INVALID_METHODID;
    };
    let rows = match rows {
        Ok(rows) => rows,
        Err(code) => return code,
    };
    if entry_count_ptr.is_null() || table_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let Ok(count) = JInt::try_from(rows.len()) else {
        return ERR_INTERNAL;
    };
    let Some(bytes) = rows.len().checked_mul(std::mem::size_of::<LineNumberRow>()) else {
        return ERR_OUT_OF_MEMORY;
    };
    let mem = allocate_block(bytes);
    if mem.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    // SAFETY: `mem` is a fresh block of `bytes` bytes, 16-byte aligned (as
    // `jvmtiLineNumberEntry` needs); the out-parameters are the agent's,
    // non-null.
    unsafe {
        std::ptr::copy_nonoverlapping(rows.as_ptr(), mem.cast::<LineNumberRow>(), rows.len());
        *entry_count_ptr = count;
        *table_ptr = mem.cast::<LineNumberRow>();
    }
    ERR_NONE
}

/// One `LocalVariableTable` row of a method, its strings still Rust's.
struct LocalVariableFacts {
    start: u16,
    length: u16,
    slot: u16,
    name: String,
    signature: String,
    generic: Option<String>,
}

/// The `LocalVariableTable` rows of the method `method` names, in class-file
/// order, each with the generic signature of the `LocalVariableTypeTable`
/// row of the same slot and start (JDWP's `Method.VariableTableWithGeneric`
/// pairs them the same way); `Err` with the code to answer: 111 for a native
/// method, 101 for one without a table (javac without `-g`, an abstract
/// method), 23 for an id that names no method.
fn local_variable_facts(shared: &SharedVm, method: u64) -> Result<Vec<LocalVariableFacts>, JInt> {
    use cratonvm_reader::attribute::Attribute;
    // Cast: the id's halves, as `jni::decode_method_id` reads them.
    let class_id = ClassId::new((method >> 32) as u32);
    // Cast: the low 16 bits are the method index.
    let index = (method & 0xFFFF) as usize;
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(class_id) else {
        drop(cm);
        // Wave 56: a lambda proxy's spun method has no table (as a method
        // of a class javac compiled without `-g`).
        return Err(if proxy_method(shared, method).is_some() {
            ERR_ABSENT_INFORMATION
        } else {
            ERR_INVALID_METHODID
        });
    };
    let m = class.methods.get(index).ok_or(ERR_INVALID_METHODID)?;
    if m.is_native() {
        return Err(ERR_NATIVE_METHOD);
    }
    let cp = &class.constant_pool;
    let attributes = m
        .code()
        .map(|code| &code.attributes[..])
        .unwrap_or_default();
    let mut rows: Option<Vec<LocalVariableFacts>> = None;
    let mut generics: Vec<(u16, u16, String)> = Vec::new();
    for attribute in attributes {
        match attribute {
            Attribute::LocalVariableTable(entries) => {
                rows.get_or_insert_with(Vec::new)
                    .extend(entries.iter().map(|e| {
                        LocalVariableFacts {
                            start: e.start_pc,
                            length: e.length,
                            slot: e.index,
                            name: cp.get_utf8(e.name_index).unwrap_or_default().to_string(),
                            signature: cp
                                .get_utf8(e.descriptor_index)
                                .unwrap_or_default()
                                .to_string(),
                            generic: None,
                        }
                    }));
            }
            Attribute::LocalVariableTypeTable(entries) => {
                generics.extend(entries.iter().filter_map(|e| {
                    let signature = cp.get_utf8(e.signature_index)?;
                    Some((e.index, e.start_pc, signature.to_string()))
                }));
            }
            _ => {}
        }
    }
    let mut rows = rows.ok_or(ERR_ABSENT_INFORMATION)?;
    for row in &mut rows {
        row.generic = generics
            .iter()
            .find(|(slot, start, _)| *slot == row.slot && *start == row.start)
            .map(|(_, _, signature)| signature.clone());
    }
    Ok(rows)
}

/// Slot 72: `GetLocalVariableTable(env, method, entry_count_ptr, table_ptr)`
/// (interpreter round i2 wave 57, lane L1; `can_access_local_variables`):
/// the method's `LocalVariableTable` rows in an `Allocate` block, each
/// string in its own (the agent deallocates each), the generic signature
/// NULL where the `LocalVariableTypeTable` has none. HotSpot's order, as
/// its generated entry checks it: the phase, the capability, the method,
/// the NULL out-parameters, then 111 for a native method and 101 for a
/// method without a table (MEASURED, `L1W57JvmtiLocalVariableTable`).
extern "C" fn get_local_variable_table(
    env: *mut JvmtiNativeEnv,
    method: u64,
    entry_count_ptr: *mut JInt,
    table_ptr: *mut *mut LocalVariableRow,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let Some(shared) = env.start_or_live_vm() else {
        return ERR_WRONG_PHASE;
    };
    if !env.possesses(CAN_ACCESS_LOCAL_VARIABLES) {
        return ERR_MUST_POSSESS_CAPABILITY;
    }
    let known = with_method(&shared, method, |_, _| ()).is_some()
        || proxy_method(&shared, method).is_some();
    if !known {
        return ERR_INVALID_METHODID;
    }
    if entry_count_ptr.is_null() || table_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let facts = match local_variable_facts(&shared, method) {
        Ok(facts) => facts,
        Err(code) => return code,
    };
    let Ok(count) = JInt::try_from(facts.len()) else {
        return ERR_INTERNAL;
    };
    let Some(bytes) = facts
        .len()
        .max(1)
        .checked_mul(std::mem::size_of::<LocalVariableRow>())
    else {
        return ERR_OUT_OF_MEMORY;
    };
    let mut rows: Vec<LocalVariableRow> = Vec::with_capacity(facts.len());
    let mut short = false;
    for fact in &facts {
        let name = allocate_modified_utf8(&fact.name);
        let signature = allocate_modified_utf8(&fact.signature);
        let generic_signature = fact
            .generic
            .as_deref()
            .map_or(std::ptr::null_mut(), allocate_modified_utf8);
        short |= name.is_null()
            || signature.is_null()
            || (fact.generic.is_some() && generic_signature.is_null());
        rows.push(LocalVariableRow {
            start_location: i64::from(fact.start),
            length: JInt::from(fact.length),
            name,
            signature,
            generic_signature,
            slot: JInt::from(fact.slot),
        });
    }
    let mem = if short {
        std::ptr::null_mut()
    } else {
        allocate_block(bytes)
    };
    if mem.is_null() {
        for row in &rows {
            for block in [row.name, row.signature, row.generic_signature] {
                if !block.is_null() {
                    // SAFETY: allocated just above, not handed out.
                    unsafe { free_block(block) };
                }
            }
        }
        return ERR_OUT_OF_MEMORY;
    }
    // SAFETY: `mem` is a fresh block of at least `rows.len()` rows, 16-byte
    // aligned; the out-parameters are the agent's, non-null.
    unsafe {
        std::ptr::copy_nonoverlapping(rows.as_ptr(), mem.cast::<LocalVariableRow>(), rows.len());
        *entry_count_ptr = count;
        *table_ptr = mem.cast::<LocalVariableRow>();
    }
    ERR_NONE
}

/// The Java frames of the calling thread, the current one of `shared`,
/// innermost first, as `(jmethodID, jlocation)` (interpreter round i1 wave
/// 22, lane L1;
/// `interpreter-L1-proposal-jvmti-c-table-stack-and-line-functions-RETIRED-20261015`, stage 2): its
/// interpreter frames at their `last_instr_pc` (the top one's is the bytecode
/// an event callback is posted for: the dispatch loop sets it before the
/// breakpoint hook, the throw and the method-entry / exit posts) and the
/// compiled activations between them, with the callees they inlined, spliced
/// as a stack trace splices them
/// (`stackwalker::capture_full_trace_without_store`), so a compiled caller of
/// the method an event stops in is listed as HotSpot lists it. A frame whose
/// method no longer resolves in its class is left out: it has no
/// `jmethodID`.
///
/// Wave 42 (lane L1;
/// `docs/feature-designs/jvmti-delivery-threading.md`):
/// and the native methods the thread is running, at location -1, as JVMTI
/// lists a native frame. A JNI native pushes no interpreter frame; its call
/// records a row ([`NativeFrameRow`], `JvmThread::jni_native_frames`), and
/// so does a native's `MethodEntry` / `MethodExit` post, around the agent's
/// callback. The row goes directly below the first frame the call put on
/// the stack: the first interpreter frame the call did not have yet, or,
/// since wave 44, a compiled activation entered after the call (its own
/// upcall's compiled target), whichever comes first -- above the compiled
/// activations entered before it -- or on top ([`splice_native_rows`],
/// `stackwalker::capture_trace_with_anchor_positions`).
///
/// Another thread (interpreter round i1 wave 45, lane L1; it was
/// `JVMTI_ERROR_NOT_AVAILABLE`) is read from where it stands still
/// ([`other_thread_frames`]): blocked in a native region, suspended, or,
/// running, after a short suspension of its own, as HotSpot reads it through
/// a handshake. A thread `shared` does not know is
/// `JVMTI_ERROR_UNATTACHED_THREAD`.
fn frames_of(shared: &SharedVm, thread: JObject) -> Result<Vec<FrameInfo>, JInt> {
    let current = jni::current_jvm_thread_of(shared).ok_or(ERR_UNATTACHED_THREAD)?;
    // SAFETY: the calling thread's own, live `JvmThread`
    // (`jni::current_jvm_thread_of`); `thread_id` is an immutable `Copy`
    // field.
    let current_tid = unsafe { (*current).thread_id.0 };
    if thread != 0 {
        let tid = thread_id_of(shared, thread)?;
        if tid != current_tid {
            return other_thread_frames(shared, tid);
        }
    }
    // SAFETY: as above; the thread is this one, which is inside this call and
    // so does not change its frames, or the natives it runs, while they are
    // read.
    let rows = listed_rows(shared, unsafe { &*current }, false);
    Ok(rows.into_iter().map(|row| row.info).collect())
}

/// One frame of the current thread's JVMTI listing ([`listed_rows`]): its
/// `(jmethodID, jlocation)`, and the index of the interpreter frame it is
/// (interpreter round i1 wave 44, lane L1), `None` for a native method's
/// row and a compiled activation's.
struct ListedRow {
    info: FrameInfo,
    frame: Option<usize>,
    /// A lambda proxy method's row: its locals record
    /// (`JvmThread::lambda_proxy_locals`; the i54-L1 retirement).
    proxy_locals: Option<usize>,
    /// JIT round 19 proposal wave 7, lane gc7 (SYNC619-3): a trace row's index
    /// in the trace the listing was built from (`FrameRow::Java`), `None` for
    /// every other row and for a stale interpreter frame's row. Read only by
    /// the calling thread's `GetOwnedMonitorStackDepthInfo`, which attributes
    /// monitors by trace index when compiled activations are on its stack.
    /// Interpreter round i6 wave 120 follow-up (lane L1): the shared listing
    /// finds it from the row's interpreter frame or compiled activation
    /// (`held_monitors::trace_position_owners`).
    at: Option<usize>,
    /// j27 wave 5 (lane JVMTIFRAME): a compiled activation's row, or an
    /// OSR'd interpreter frame's, of the shared listing asked with frames:
    /// what [`compiled_locals`] reads its locals by
    /// (`CRATONVM_JVMTI_C_COMPILED_FRAME_LOCALS`). `None` for every other row.
    home: Option<crate::runtime::interpreter::compiled_frame_locals::HomeRow>,
}

/// The listing of [`frames_of`], top first, of `current`, the calling
/// thread's own `JvmThread` (which is inside this call and does not change
/// its frames while they are read). With `with_frames` each row also names
/// its interpreter frame (wave 44, for the local-variable and monitor
/// functions).
///
/// Interpreter round i1 wave 46, lane L1:
///
/// * The interpreter frames are found by their own positions in the trace:
///   one anchor per frame at an entry-chain length no compiled activation
///   reaches (`stackwalker::capture_trace_with_anchor_positions` then
///   answers the frame's own slot). Since wave 44's merge the frames were
///   looked up among the JNI natives' anchor positions, which named no frame
///   (or another one), so every local-variable function of the current
///   thread answered `OPAQUE_FRAME` and `NotifyFramePop` found no frame.
/// * A reflective call's JDK frames (`Method.invoke` and the accessor's
///   `invoke`; `Constructor.newInstance`, `newInstanceWithCaller` and the
///   accessor's `newInstance`) are listed where a stack trace lists them,
///   right above the first frame the call pushed, as HotSpot lists them
///   (item 4 of
///   `docs/feature-designs/jdk-only-mode.md`;
///   `stackwalker::reflective_entries`, anchored at `JvmThread::reflective_calls`).
///   HotSpot also lists the accessor's `@Hidden` `invokeImpl` and the
///   method-handle frames under it, which this VM does not run.
fn listed_rows(shared: &SharedVm, current: &JvmThread, with_frames: bool) -> Vec<ListedRow> {
    // Interpreter round i6 wave 119, lane L1 (`CRATONVM_JVMTI_C_LISTING_SHARED`,
    // default ON since the i6 wave-126 flip; i102-L1 item 1 / i46-L1 step 2): the listing from the
    // builder JDWP and the other-thread reads use, through the conversion
    // those reads use (`other_rows`). Wave 120: also for the local-variable
    // and monitor functions (`with_frames`): each row's interpreter frame
    // (none for a frame whose body runs compiled, as below) and a lambda
    // proxy row's locals record (matched to the thread's proxy rows,
    // innermost first). Wave 120 follow-up: also with compiled activations
    // on the thread and frames asked -- each row's trace position (`at`, what
    // the owned-monitor depths attribute by) is found from its interpreter
    // frame or its compiled activation's chain index
    // (`held_monitors::trace_position_owners`) -- and while a proxy's calls
    // go unrecorded (the inferred proxy rows, [`with_inferred_proxy_other_rows`]).
    let gates = &shared.debug.debugger_gates;
    if gates.c_listing_shared() {
        let rows = crate::runtime::interpreter::listing_rows_of_current_thread(shared, current);
        // The proxy rows' keys, innermost first, as the listing's rows run.
        let mut proxy_keys: Vec<(u32, u16, u64)> = if with_frames {
            current
                .jni_native_frames
                .iter()
                .rev()
                .filter_map(|&(_, class_id, key, _)| {
                    Some((class_id.as_u32(), proxy_row_index(key)?, key))
                })
                .collect()
        } else {
            Vec::new()
        };
        let rows = other_rows(shared, rows);
        let rows: Vec<(OtherRow, bool)> = if gates.proxy_rows_incomplete() {
            with_inferred_proxy_other_rows(shared, rows)
        } else {
            rows.into_iter().map(|row| (row, false)).collect()
        };
        // Each trace position's owner, with compiled code active and frames
        // asked (empty otherwise: no row carries `at`, as before).
        let owners = if with_frames {
            crate::runtime::interpreter::held_monitors::trace_position_owners(&current.frames)
        } else {
            Vec::new()
        };
        return rows
            .into_iter()
            .map(|(row, inferred)| {
                let frame = if with_frames {
                    row.raw.frame.filter(|_| !row.raw.runs_compiled)
                } else {
                    None
                };
                // Truncation: the class id and the proxy index halves of
                // wave 56's `(proxy id << 32) | index`.
                let (class_raw, index) = ((row.info.method >> 32) as u32, row.info.method as u16);
                let proxy_locals = if frame.is_none() && !inferred && !proxy_keys.is_empty() {
                    proxy_keys
                        .iter()
                        .position(|&(c, i, _)| c == class_raw && i == index)
                        .and_then(|p| proxy_row_locals(proxy_keys.remove(p).2))
                } else {
                    None
                };
                use crate::runtime::interpreter::held_monitors::TracePositionOwner as Owner;
                let owner = match (row.raw.frame, row.raw.chain) {
                    (Some(index), _) => Some(Owner::Interp(index)),
                    (None, Some(chain)) => Some(Owner::Compiled(chain)),
                    (None, None) => None,
                };
                let at = owner.and_then(|owner| owners.iter().position(|&o| o == owner));
                // j27 wave 5 (lane JVMTIFRAME): a compiled or OSR'd row, for
                // the local-variable functions only.
                let home = (with_frames && frame.is_none() && proxy_locals.is_none())
                    .then(|| {
                        crate::runtime::interpreter::compiled_frame_locals::HomeRow::of_listing_row(
                            &row.raw,
                        )
                    })
                    .flatten();
                ListedRow {
                    info: row.info,
                    frame,
                    proxy_locals,
                    at,
                    home,
                }
            })
            .collect();
    }
    let frames = &current.frames;
    let natives = &current.jni_native_frames;
    let calls = &current.reflective_calls;
    // The i54-L1 retirement: the VM-served `Thread.run` calls, whose frames
    // (`Thread.run`, its `@Hidden` `runWith`) HotSpot lists.
    let served = current.served_thread_run.call_depths().unwrap_or_default();
    let (trace, positions) = if natives.is_empty()
        && calls.is_empty()
        && served.is_empty()
        && !with_frames
    {
        (
            crate::runtime::stackwalker::capture_full_trace_without_store(frames),
            Vec::new(),
        )
    } else {
        let frame_anchors = if with_frames { frames.len() } else { 0 };
        let mut anchors: Vec<(u32, u32)> =
            Vec::with_capacity(natives.len() + calls.len() + frame_anchors);
        anchors.extend(natives.iter().map(|&(depth, _, _, jit_depth)| {
            (u32::try_from(depth).unwrap_or(u32::MAX), jit_depth)
        }));
        anchors.extend(calls.iter().map(|call| (call.interp_depth, call.jit_depth)));
        anchors.extend(served.iter().copied());
        // A frame's own slot: no compiled activation's chain index reaches
        // `u32::MAX`, so the first slot at or past frame `d` that the anchor
        // finds is frame `d`'s.
        anchors
            .extend((0..frame_anchors).map(|d| (u32::try_from(d).unwrap_or(u32::MAX), u32::MAX)));
        crate::runtime::stackwalker::capture_trace_with_anchor_positions(shared, frames, &anchors)
    };
    let native_end = natives.len().min(positions.len());
    let call_end = (native_end + calls.len()).min(positions.len());
    let served_end = (call_end + served.len()).min(positions.len());
    let native_positions = &positions[..native_end];
    let call_positions = &positions[native_end..call_end];
    let served_positions = &positions[call_end..served_end];
    let frame_positions = &positions[served_end..];
    // An interpreter frame whose body runs compiled (an OSR'd loop) holds
    // the values of its compiled entry, not its current ones: no
    // local-variable function reads it (wave 44; JDWP withholds them the
    // same way, `interpreter::capture_blocked_compiled_view`).
    let stale: Vec<usize> =
        if with_frames && crate::jit::conservative_roots::current_thread_jit_depth() != 0 {
            crate::runtime::interpreter::capture_blocked_compiled_view(frames)
                .stale
                .iter()
                .filter_map(|&(index, _)| usize::try_from(index).ok())
                .collect()
        } else {
            Vec::new()
        };
    // Interpreter round i1 wave 47, lane L1: a thread parked at the return of
    // a registered native standing in for a JDK method (`Thread.sleep`,
    // `Object.wait`, `LockSupport.parkNanos`) has that method's native leaf
    // on top (the park's row, `NativeFrameRow::enter_hashed`); the JDK Java
    // levels between the leaf and the caller, which the stand-in did not
    // run, go right below it, as the JDWP listing lists them
    // (`interpreter::NativeHead`). Named before the class-manager lock below.
    let standin_levels: Vec<crate::runtime::interpreter::StandinLevel> =
        match (natives.last(), frames.last()) {
            (Some(&(depth, class_id, key, _)), Some(top))
                if depth == frames.len() && key & ROW_KEY_IS_HASH != 0 =>
            {
                crate::runtime::interpreter::standin_head_of_top(shared, top)
                    .filter(|head| {
                        head.class_id == u64::from(class_id.as_u32())
                            && head.method_id | ROW_KEY_IS_HASH == key
                    })
                    .map(|head| head.levels().to_vec())
                    .unwrap_or_default()
            }
            _ => Vec::new(),
        };
    // Interpreter round i2 wave 60, lane L1: each lambda proxy method's row
    // (`NativeFrameRow::enter_lambda_proxy`) is listed at the location of its
    // spun code's invoking instruction. Named before the class-manager lock
    // below (the model reads the class store). Empty without proxy rows.
    let proxy_locations: Vec<(ClassId, u16, i64)> = natives
        .iter()
        .filter_map(|&(_, class_id, key, _)| {
            let index = proxy_row_index(key)?;
            let at = crate::runtime::lambda_proxy::lambda_proxy_invoke_location(
                shared,
                class_id,
                usize::from(index),
            )
            .and_then(|at| i64::try_from(at).ok());
            if crate::runtime::env_cache::frame_trace() {
                eprintln!(
                    "[JVMTI_C_LAMBDA_FRAMES] class={:#x} method={index} location={at:?}",
                    class_id.as_u32()
                );
            }
            Some((class_id, index, at.unwrap_or(0)))
        })
        .collect();
    // The i54-L1 retirement: the proxies a frame of the trace implements,
    // for the calls no row recorded. Named before the class-manager lock.
    let candidates = if shared.debug.debugger_gates.proxy_rows_incomplete() {
        inferred_proxy_candidates(shared, &trace)
    } else {
        Vec::new()
    };
    let cm = shared.classes.class_manager.read();
    // Each reflective call's JDK frames, OUTERMOST first, at its position; a
    // call whose frames cannot be named (a JDK without these accessors) is
    // left out, as a stack trace leaves it out.
    let mut reflective: Vec<(usize, Vec<crate::native::registry::StackTraceEntry>)> = calls
        .iter()
        .zip(call_positions)
        .filter_map(|(call, &position)| {
            crate::runtime::stackwalker::reflective_entries(&cm.class_store, call.kind)
                .map(|entries| (position, entries))
        })
        .collect();
    // The i54-L1 retirement: each served `Thread.run` call's frames, at
    // its position, after a reflective call's at the same one.
    if !served_positions.is_empty() {
        if let Some(entries) =
            crate::runtime::stackwalker::served_thread_run_debugger_entries(&cm.class_store)
        {
            reflective.extend(
                served_positions
                    .iter()
                    .map(|&position| (position, entries.clone())),
            );
        }
    }
    let rows = splice_rows(&trace, native_positions, natives, &reflective);
    let rows = if candidates.is_empty() {
        rows
    } else {
        with_inferred_proxy_rows(&cm.class_store, rows, &candidates)
    };
    if crate::runtime::env_cache::frame_trace() && !(natives.is_empty() && reflective.is_empty()) {
        eprintln!(
            "[JVMTI_NATIVE_ROWS] natives={} reflective={} trace={} rows={}",
            natives.len(),
            reflective.len(),
            trace.len(),
            rows.len()
        );
    }
    let names_it = |m: &cratonvm_reader::method::ClassFileMethod,
                    entry: &crate::native::registry::StackTraceEntry| {
        *m.name == *entry.method_name
            && entry
                .method_descriptor
                .as_deref()
                .is_none_or(|d| *m.descriptor == *d)
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows.iter().rev() {
        let (entry, frame, at) = match *row {
            FrameRow::Java(at, entry) => {
                // The interpreter frame whose row this is, if any: the
                // innermost frame anchored there (a frame the trace does not
                // list anchors at the next one's slot).
                let anchored = frame_positions.iter().rposition(|&p| p == at);
                let frame = anchored.filter(|index| !stale.contains(index));
                // SYNC619-3 (round 19 wave 7, lane gc7): the row's trace index,
                // for a monitor attributed by trace position; withheld from a
                // stale interpreter frame's row, as its frame is.
                let at = (anchored.is_none() || frame.is_some()).then_some(at);
                (entry, frame, at)
            }
            FrameRow::Spliced(entry) => (entry, None, None),
            FrameRow::Inferred(class_id, location) => {
                out.push(ListedRow {
                    info: FrameInfo {
                        // Widening: the functional method, index 1 (wave 56's scheme).
                        method: (u64::from(class_id.as_u32()) << 32) | 1,
                        location,
                    },
                    frame: None,
                    proxy_locals: None,
                    at: None,
                    home: None,
                });
                continue;
            }
            FrameRow::Native(class_id, key) => {
                if let Some(index) = proxy_row_index(key) {
                    let location = proxy_locations
                        .iter()
                        .find(|&&(c, i, _)| c == class_id && i == index)
                        .map_or(0, |&(_, _, at)| at);
                    out.push(ListedRow {
                        info: FrameInfo {
                            // Widening: the proxy's u32 id and the u16 index
                            // into their halves of the id (wave 56's scheme).
                            method: (u64::from(class_id.as_u32()) << 32) | u64::from(index),
                            location,
                        },
                        frame: None,
                        proxy_locals: proxy_row_locals(key),
                        at: None,
                        home: None,
                    });
                    continue;
                }
                let Some(class) = cm.get_class(class_id) else {
                    continue;
                };
                // A native method's frame has no bytecode: location -1.
                let Some(index) =
                    row_method_index(shared, class, key).and_then(|i| u16::try_from(i).ok())
                else {
                    continue;
                };
                out.push(ListedRow {
                    info: FrameInfo {
                        // Widening: u32 class id and u16 index into their halves of the id.
                        method: (u64::from(class_id.as_u32()) << 32) | u64::from(index),
                        location: -1,
                    },
                    frame: None,
                    proxy_locals: None,
                    at: None,
                    home: None,
                });
                continue;
            }
        };
        let Some(class_id) = entry.class_id else {
            continue;
        };
        let Some(class) = cm.get_class(class_id) else {
            continue;
        };
        let index = entry
            .method_index
            .and_then(|i| usize::try_from(i).ok())
            .filter(|&i| class.methods.get(i).is_some_and(|m| names_it(m, entry)))
            .or_else(|| class.methods.iter().position(|m| names_it(m, entry)));
        // `jni::decode_method_id` reads the index from the low 16 bits.
        let Some(index) = index.and_then(|i| u16::try_from(i).ok()) else {
            continue;
        };
        out.push(ListedRow {
            info: FrameInfo {
                // Widening: u32 class id and u16 index into their halves of the id.
                method: (u64::from(class_id.as_u32()) << 32) | u64::from(index),
                // -1 for an unknown index, as the capture marks it.
                location: i64::from(entry.byte_code_index),
            },
            frame,
            proxy_locals: None,
            at,
            home: None,
        });
    }
    if !standin_levels.is_empty()
        && out
            .first()
            .is_some_and(|row| row.info.location == -1 && row.frame.is_none())
    {
        out.splice(
            1..1,
            standin_levels.iter().map(|level| ListedRow {
                info: FrameInfo {
                    // Widening: u32 class id and u16 index into their halves of the id.
                    method: (level.class_id << 32) | u64::from(level.method_index),
                    location: i64::try_from(level.bci).unwrap_or(-1),
                },
                frame: None,
                proxy_locals: None,
                at: None,
                home: None,
            }),
        );
    }
    out
}

/// One row of [`frames_of`]'s listing: a row of the stack trace, a JDK frame
/// of a reflective call (wave 46), or a native method the thread runs
/// (declaring class, the row's key: see [`NativeFrameRow`]).
#[derive(Clone, Copy)]
enum FrameRow<'a> {
    /// The row's index in the trace, and the row (wave 44: the index names
    /// the interpreter frame a local-variable function reads).
    Java(usize, &'a crate::native::registry::StackTraceEntry),
    /// A reflective call's JDK frame (wave 46), which has no interpreter frame.
    Spliced(&'a crate::native::registry::StackTraceEntry),
    Native(ClassId, u64),
    /// A lambda proxy's functional method between a call site and the
    /// implementation it ran, for a call no row recorded (in progress when
    /// the first C env bound late; the i54-L1 retirement): the proxy and the
    /// location [`with_inferred_proxy_rows`] inferred.
    Inferred(ClassId, i64),
}

/// `trace` (outermost first) with the rows of `natives` (outermost first,
/// `(interpreter depth at the call, class, key, JIT entry-chain length at the
/// call)`, `JvmThread::jni_native_frames`) spliced in (wave 42): row `k` goes
/// right before `trace[positions[k]]`, or after every row for a position at
/// or past the end (`stackwalker::capture_trace_with_anchor_positions`: the
/// first frame the native's call put on the stack, an interpreter frame or,
/// since wave 44, a compiled activation its own upcall entered). Positions
/// never decrease; natives at one position keep their order, the outer one
/// first.
#[cfg(test)]
fn splice_native_rows<'a>(
    trace: &'a [crate::native::registry::StackTraceEntry],
    positions: &[usize],
    natives: &[(usize, ClassId, u64, u32)],
) -> Vec<FrameRow<'a>> {
    splice_rows(trace, positions, natives, &[])
}

/// What [`splice_rows`] puts between two rows of the trace.
enum Inserted<'a> {
    Native(ClassId, u64),
    /// A reflective call's JDK frames, outermost first.
    Reflective(&'a [crate::native::registry::StackTraceEntry]),
}

/// `trace` with the rows of `natives` at `positions`, as `splice_native_rows`
/// places them, and each of `reflective` (`(position, JDK frames outermost
/// first)`, wave 46) right before `trace[position]` (after every row for a
/// position at or past the end). At one position a reflective call's frames
/// go below (outside) a native's row: a native a reflective call runs is its
/// target, entered after the call's own frames.
fn splice_rows<'a>(
    trace: &'a [crate::native::registry::StackTraceEntry],
    positions: &[usize],
    natives: &[(usize, ClassId, u64, u32)],
    reflective: &'a [(usize, Vec<crate::native::registry::StackTraceEntry>)],
) -> Vec<FrameRow<'a>> {
    let spliced: usize = reflective.iter().map(|(_, entries)| entries.len()).sum();
    let mut inserts: Vec<(usize, u8, usize, Inserted<'a>)> =
        Vec::with_capacity(natives.len() + reflective.len());
    inserts.extend(
        reflective
            .iter()
            .enumerate()
            .map(|(k, (position, entries))| {
                (*position, 0, k, Inserted::Reflective(entries.as_slice()))
            }),
    );
    inserts.extend(
        natives
            .iter()
            .enumerate()
            .map(|(k, &(_, class_id, key, _))| {
                let position = positions.get(k).copied().unwrap_or(usize::MAX);
                (position, 1, k, Inserted::Native(class_id, key))
            }),
    );
    inserts.sort_by_key(|&(position, kind, k, _)| (position, kind, k));
    let mut rows = Vec::with_capacity(trace.len() + natives.len() + spliced);
    let put = |rows: &mut Vec<FrameRow<'a>>, inserted: Inserted<'a>| match inserted {
        Inserted::Native(class_id, key) => rows.push(FrameRow::Native(class_id, key)),
        Inserted::Reflective(entries) => rows.extend(entries.iter().map(FrameRow::Spliced)),
    };
    let mut pending = inserts.into_iter().peekable();
    for (at, entry) in trace.iter().enumerate() {
        while let Some((_, _, _, inserted)) = pending.next_if(|&(position, ..)| position <= at) {
            put(&mut rows, inserted);
        }
        rows.push(FrameRow::Java(at, entry));
    }
    for (_, _, _, inserted) in pending {
        put(&mut rows, inserted);
    }
    rows
}

/// The lambda proxies whose implementation method a frame of `trace` runs,
/// for [`with_inferred_proxy_rows`] (the i54-L1 retirement); none while no
/// such proxy exists (one read of the proxy map). Reads the class manager
/// (the proxies' host names): call it before taking that lock.
fn inferred_proxy_candidates(
    shared: &SharedVm,
    trace: &[crate::native::registry::StackTraceEntry],
) -> Vec<crate::runtime::stackwalker::LambdaProxyCandidate> {
    let sites: Vec<Arc<crate::classloading::resolution::LambdaCallSite>> = shared
        .classes
        .lambda_proxies
        .read()
        .values()
        .filter(|site| {
            trace
                .iter()
                .any(|entry| crate::runtime::stackwalker::lambda_site_stands_below(site, entry))
        })
        .cloned()
        .collect();
    sites
        .into_iter()
        .filter_map(|site| {
            let id = site.proxy_class_id;
            let (_, host) = crate::runtime::interpreter::lambda_proxy_host(shared, id)?;
            let name: Arc<str> = Arc::from(format!("{host}$$Lambda/0x{:016x}", id.as_u32()));
            Some(crate::runtime::stackwalker::LambdaProxyCandidate { site, name })
        })
        .collect()
}

/// `rows` (outermost first) with a [`FrameRow::Inferred`] row between each
/// call site and the lambda implementation it reached through a proxy no row
/// recorded (the i54-L1 retirement): a call that was in progress when the
/// first C env bound late, before which no lambda dispatch recorded its proxy
/// frame. Inferred as a `SHOW_HIDDEN_FRAMES` walk infers it
/// (`stackwalker::lambda_proxy_standin_frames`, fail-closed: the caller
/// stands at an `invokeinterface` of the SAM, the callee is the
/// implementation of exactly one proxy of it), only between two adjacent
/// Java rows: a recorded call's row stands between them already.
fn with_inferred_proxy_rows<'a>(
    store: &crate::classloading::ClassStore,
    rows: Vec<FrameRow<'a>>,
    candidates: &[crate::runtime::stackwalker::LambdaProxyCandidate],
) -> Vec<FrameRow<'a>> {
    let mut out = Vec::with_capacity(rows.len() + 2);
    for (i, row) in rows.iter().enumerate() {
        out.push(*row);
        let caller = match row {
            FrameRow::Java(_, caller) | FrameRow::Spliced(caller) => *caller,
            _ => continue,
        };
        let Some(FrameRow::Java(_, callee)) = rows.get(i + 1) else {
            continue;
        };
        let pair = [(*callee).clone(), (*caller).clone()];
        let inferred =
            crate::runtime::stackwalker::lambda_proxy_standin_frames(store, &pair, candidates);
        if let Some(entry) = inferred.first().and_then(|(_, entries)| entries.first()) {
            if let Some(proxy) = entry.class_id {
                if crate::runtime::env_cache::frame_trace() {
                    eprintln!(
                        "[JVMTI_C_LAMBDA_FRAMES] inferred class={:#x} location={}",
                        proxy.as_u32(),
                        entry.byte_code_index
                    );
                }
                out.push(FrameRow::Inferred(proxy, i64::from(entry.byte_code_index)));
            }
        }
    }
    out
}

/// The key a native method's row carries (wave 42): a 64-bit FNV-1a hash of
/// `name`, a `0` separator byte and `descriptor`, the function the JDWP
/// method id is (`debug::jdwp_method_id` answers this one), so a row and a
/// JDWP-side report name a method alike. [`frames_of`] finds the method in
/// its declaring class by it.
pub(crate) fn native_row_method_hash(name: &str, descriptor: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in name
        .bytes()
        .chain(std::iter::once(0u8))
        .chain(descriptor.bytes())
    {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// The bit of a native row's key ([`NativeFrameRow`]) that marks it a
/// [`native_row_method_hash`] (a method-event post's row); clear, the key is
/// the JNI function pointer the native was called through (a JNI arm's row).
/// A user-space function address never has bit 63 set.
const ROW_KEY_IS_HASH: u64 = 1 << 63;

/// With [`ROW_KEY_IS_HASH`] clear, the bit of a row's key that marks it a
/// lambda proxy method's frame (interpreter round i2 wave 60, lane L1;
/// [`NativeFrameRow::enter_lambda_proxy`]): the row's class is the proxy,
/// the key's low 16 bits the method's index among
/// `runtime::lambda_proxy::lambda_proxy_methods`. A user-space function
/// address never has bit 62 set either.
const ROW_KEY_IS_PROXY: u64 = 1 << 62;

/// The method index a row's `key` names when it is a lambda proxy method's
/// row ([`ROW_KEY_IS_PROXY`]), `None` for a native method's row.
#[inline]
pub(crate) fn proxy_row_index(key: u64) -> Option<u16> {
    // Truncation: the index is the key's low 16 bits.
    (key & (ROW_KEY_IS_HASH | ROW_KEY_IS_PROXY) == ROW_KEY_IS_PROXY).then_some(key as u16)
}

/// How many of a thread's rows `rows` (`JvmThread::jni_native_frames`) are
/// lambda proxy method frames entered below interpreter frame count `depth`
/// (each stands between frames `d - 1` and `d`, `d < depth`) whose listing
/// facts `listed` admits (`DebugRealm::lambda_proxy_rows`): the proxy rows a
/// debugger's listing places under the thread's frame number `depth`
/// (interpreter round i7 wave 138, lane L1; `interpreter::debugger_step_depth`).
pub(crate) fn proxy_rows_below(
    rows: &[(usize, ClassId, u64, u32)],
    depth: usize,
    listed: impl Fn(ClassId, u16) -> bool,
) -> usize {
    rows.iter()
        .filter(|&&(d, class_id, key, _)| {
            d < depth && proxy_row_index(key).is_some_and(|index| listed(class_id, index))
        })
        .count()
}

/// Interpreter round i7 wave 138, lane L1: the proxy rows a step counts.
#[cfg(test)]
mod i138_l1_proxy_rows_below_tests {
    use super::{proxy_rows_below, ClassId, ROW_KEY_IS_HASH, ROW_KEY_IS_PROXY};

    #[test]
    fn only_proxy_rows_entered_below_the_depth_and_listed_count() {
        let proxy = ClassId::new(7);
        let rows = vec![
            // A proxy row entered at depth 2 (between frames 1 and 2).
            (2usize, proxy, ROW_KEY_IS_PROXY | (1 << 16) | 3, 0u32),
            // A JNI native's row (a function pointer key).
            (1, ClassId::new(9), 0x7ff0_1234, 0),
            // A method-event post's row (a hashed key).
            (1, ClassId::new(9), ROW_KEY_IS_HASH | 5, 0),
            // A proxy row on top of three frames (the proxy's own events).
            (3, proxy, ROW_KEY_IS_PROXY | 3, 0),
        ];
        assert_eq!(proxy_rows_below(&rows, 3, |_, _| true), 1);
        assert_eq!(proxy_rows_below(&rows, 4, |_, _| true), 2);
        assert_eq!(proxy_rows_below(&rows, 2, |_, _| true), 0);
        // A row whose listing facts were never noted is not listed.
        assert_eq!(proxy_rows_below(&rows, 4, |_, index| index != 3), 0);
    }
}

/// The index, in `class`, of the native method a row's `key` names
/// ([`NativeFrameRow`]); `None` when none does. Read-time work only: a
/// hashed key is compared with each method's [`native_row_method_hash`]; a
/// function pointer with what this VM's JNI table binds each native method
/// of the class to (`jni::find_jni_native`: `RegisterNatives` and the
/// naming-convention lookup, which caches its answer there before the call).
/// Two methods bound to one function are told apart by neither; the first is
/// named.
fn row_method_index(
    shared: &SharedVm,
    class: &crate::classloading::Class,
    key: u64,
) -> Option<usize> {
    if key & ROW_KEY_IS_HASH != 0 {
        return class
            .methods
            .iter()
            .position(|m| native_row_method_hash(&m.name, &m.descriptor) | ROW_KEY_IS_HASH == key);
    }
    class.methods.iter().position(|m| {
        m.is_native()
            && jni::find_jni_native(&shared.natives, &class.name, &m.name, &m.descriptor)
                // Widening: a function address into the key.
                .is_some_and(|bound| bound as u64 == key)
    })
}

/// `(JDWP method id, method slot)` of the native method a row of
/// `JvmThread::jni_native_frames` names in `class` ([`row_method_index`]):
/// how a debugger's listing names a JNI native in the middle of a stack
/// (interpreter round i1 wave 47, lane L1; `interpreter::debugger_listing_splices`).
pub(crate) fn jni_row_method(
    shared: &SharedVm,
    class: &crate::classloading::Class,
    key: u64,
) -> Option<(u64, u16)> {
    let index = row_method_index(shared, class, key)?;
    let method = class.methods.get(index)?;
    Some((
        crate::debug::jdwp_method_id(&method.name, &method.descriptor),
        u16::try_from(index).ok()?,
    ))
}

/// A native method's row in its thread's `JvmThread::jni_native_frames`
/// while it runs (interpreter round i1 wave 42, lane L1): the JVMTI stack
/// functions list it at location -1 ([`frames_of`]). A row is `(interpreter
/// depth at the call, declaring class, key, JIT entry-chain length at the
/// call)`, plain `Copy` data (the chain length since wave 44, lane L3: it
/// places the row above the compiled callers and below a compiled method
/// the native's own upcall entered).
///
/// * The two JNI arms of `vm_exec::invoke_on_class_shared_inner` enter one
///   around the whole call, its method-event reports included, keyed by the
///   JNI function pointer they call ([`Self::enter_jni`]). That is the whole
///   per-call cost, in every mode and without an agent: one `Vec` push of
///   24 bytes (no allocation once the thread's list has grown to its deepest
///   JNI nesting: `truncate` keeps the capacity) and one truncate. The method
///   is named only when a stack function reads the row
///   ([`row_method_index`]). Wave 42's first version hashed the name and
///   descriptor on every call.
/// * A native's `MethodEntry` / `MethodExit` post enters one around the
///   agent's callback, keyed by the method's hash ([`Self::enter_hashed`]),
///   unless the innermost row already names that native at that depth.
///
/// `Drop` truncates the list back to where it was, on every exit edge, an
/// unwind included.
pub(crate) struct NativeFrameRow {
    thread: *mut JvmThread,
    base: usize,
}

impl NativeFrameRow {
    /// Record the JNI native declared by `declaring` that is about to be
    /// called through `fn_ptr`, at `thread`'s current interpreter depth.
    ///
    /// # Safety
    ///
    /// `thread` is the calling thread's live `JvmThread`, and outlives the
    /// row.
    #[inline]
    pub(crate) unsafe fn enter_jni(
        thread: *mut JvmThread,
        declaring: ClassId,
        fn_ptr: usize,
    ) -> Self {
        // SAFETY: the caller's contract: this thread's own live `JvmThread`.
        let thread_ref = unsafe { &mut *thread };
        let depth = thread_ref.frames.len();
        let jit_depth = jit_chain_len();
        let rows = &mut thread_ref.jni_native_frames;
        let base = rows.len();
        // Widening: a function address into the key (bit 63 clear).
        rows.push((depth, declaring, fn_ptr as u64, jit_depth));
        Self { thread, base }
    }

    /// Record the native whose [`native_row_method_hash`] is `hash`, declared
    /// by `declaring`, at `thread`'s current interpreter depth, around a
    /// method-event post; nothing when the innermost row already names it at
    /// this depth (a JNI arm's own row, found through `shared`'s JNI table).
    ///
    /// # Safety
    ///
    /// As [`Self::enter_jni`].
    pub(crate) unsafe fn enter_hashed(
        shared: &SharedVm,
        thread: *mut JvmThread,
        declaring: ClassId,
        hash: u64,
    ) -> Self {
        // SAFETY: the caller's contract: this thread's own live `JvmThread`.
        let thread_ref = unsafe { &mut *thread };
        let depth = thread_ref.frames.len();
        let key = hash | ROW_KEY_IS_HASH;
        let rows = &mut thread_ref.jni_native_frames;
        let base = rows.len();
        let recorded = match rows.last() {
            Some(&(at, class_id, top, _)) if at == depth && class_id == declaring => {
                top == key
                    || (top & ROW_KEY_IS_HASH == 0
                        && shared
                            .classes
                            .class_manager
                            .read_recursive()
                            .get_class(declaring)
                            .is_some_and(|class| {
                                row_method_index(shared, class, top).is_some_and(|i| {
                                    class.methods.get(i).is_some_and(|m| {
                                        native_row_method_hash(&m.name, &m.descriptor) == hash
                                    })
                                })
                            }))
            }
            _ => false,
        };
        if !recorded {
            rows.push((depth, declaring, key, jit_chain_len()));
        }
        Self { thread, base }
    }

    /// Record that a call on lambda proxy `proxy` (a class the VM mints
    /// outside the class store) enters its method `index` -- the functional
    /// method or a bridge (`runtime::lambda_proxy::lambda_proxy_forwarding_index`)
    /// -- at `thread`'s current interpreter depth (interpreter round i2 wave
    /// 60, lane L1; `DebuggerGates::c_lambda_frames_armed`). The VM runs the
    /// spun method without a frame of its own (`interpreter::try_lambda_dispatch`
    /// calls the implementation directly); the row places one where HotSpot
    /// has it, right below the first frame the call puts on the stack, as a
    /// JNI native's row is placed ([`listed_rows`]). Only while a C JVMTI env
    /// is bound.
    ///
    /// # Safety
    ///
    /// As [`Self::enter_jni`].
    pub(crate) unsafe fn enter_lambda_proxy(
        thread: *mut JvmThread,
        proxy: ClassId,
        index: u16,
    ) -> Self {
        // SAFETY: the caller's contract: this thread's own live `JvmThread`.
        let thread_ref = unsafe { &mut *thread };
        let depth = thread_ref.frames.len();
        let rows = &mut thread_ref.jni_native_frames;
        let base = rows.len();
        rows.push((
            depth,
            proxy,
            ROW_KEY_IS_PROXY | u64::from(index),
            jit_chain_len(),
        ));
        Self { thread, base }
    }
}

/// The calling thread's JIT entry-chain length, as a row records it (wave 44,
/// lane L3). Saturating: a chain that long cannot exist.
#[inline]
fn jit_chain_len() -> u32 {
    u32::try_from(crate::jit::conservative_roots::current_thread_jit_depth()).unwrap_or(u32::MAX)
}

impl Drop for NativeFrameRow {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: `thread` outlives the row (`enter_jni`'s contract).
        unsafe { (*self.thread).jni_native_frames.truncate(self.base) };
    }
}

/// The bit position, in a lambda proxy row's key, of the number of its
/// locals record (`JvmThread::lambda_proxy_locals` index plus one; 0: none),
/// above the method index ([`proxy_row_index`]) and below
/// [`ROW_KEY_IS_PROXY`] (the i54-L1 retirement, [`LambdaProxyFrame`]).
const ROW_KEY_PROXY_LOCALS_SHIFT: u32 = 16;

/// The `JvmThread::lambda_proxy_locals` index a lambda proxy row's `key`
/// names, `None` for a row with no record and for any other row.
#[inline]
fn proxy_row_locals(key: u64) -> Option<usize> {
    proxy_row_index(key)?;
    let number = (key >> ROW_KEY_PROXY_LOCALS_SHIFT) & 0xFFFF_FFFF;
    usize::try_from(number).ok()?.checked_sub(1)
}

/// A lambda proxy method's frame while a call runs through it under a C
/// JVMTI agent (the i54-L1 retirement): wave 60's row
/// ([`NativeFrameRow::enter_lambda_proxy`]) plus the frame's locals, which
/// HotSpot's spun frame holds -- the proxy in slot 0, then the functional
/// method's arguments as the caller passed them (MEASURED,
/// `tools/probes/interp/L1/L1K54JvmtiLambdaProxyLocalsAndFields.java`).
/// Each reference slot is pinned in `JvmThread::native_pin_roots` for the
/// call (a null one by the proxy itself, marked null, so a write replaces
/// the pin in place), as the spun frame keeps them reachable; the
/// local-variable functions read and write them ([`proxy_local_read`],
/// [`proxy_local_write`]). `Drop` takes the record and the pins back, on
/// every exit edge.
pub(crate) struct LambdaProxyFrame {
    _row: NativeFrameRow,
    thread: *mut JvmThread,
    pin_base: usize,
    locals_base: usize,
}

impl LambdaProxyFrame {
    /// Enter lambda proxy `proxy`'s method `index` on `receiver` with the
    /// call's `args` (the functional method's, or a bridge's, as passed).
    ///
    /// # Safety
    ///
    /// As [`NativeFrameRow::enter_jni`].
    pub(crate) unsafe fn enter(
        thread: *mut JvmThread,
        proxy: ClassId,
        index: u16,
        receiver: ObjectRef,
        args: &[Value],
    ) -> Self {
        use crate::threading::jvm_thread::{LambdaProxyLocals, ProxyLocalSlot};
        // SAFETY: the caller's contract: this thread's own live `JvmThread`.
        let thread_ref = unsafe { &mut *thread };
        let pins = &mut thread_ref.native_pin_roots;
        let pin_base = pins.len();
        let mut pin = |obj: ObjectRef| {
            pins.push(obj);
            pins.len() - 1
        };
        let mut slots = Vec::with_capacity(args.len() + 1);
        slots.push(ProxyLocalSlot::Ref {
            pin: pin(receiver),
            null: false,
        });
        for arg in args {
            match *arg {
                Value::Long(v) => slots.extend([ProxyLocalSlot::Long(v), ProxyLocalSlot::Second]),
                Value::Double(v) => {
                    slots.extend([ProxyLocalSlot::Double(v), ProxyLocalSlot::Second])
                }
                Value::Float(v) => slots.push(ProxyLocalSlot::Float(v)),
                Value::Object(Some(obj)) => slots.push(ProxyLocalSlot::Ref {
                    pin: pin(obj),
                    null: false,
                }),
                Value::Object(None) => slots.push(ProxyLocalSlot::Ref {
                    pin: pin(receiver),
                    null: true,
                }),
                Value::Int(v) => slots.push(ProxyLocalSlot::Int(v)),
                // Never a call argument.
                Value::ReturnAddress(_) | Value::Uninitialized => {
                    slots.push(ProxyLocalSlot::Second)
                }
            }
        }
        let locals_base = thread_ref.lambda_proxy_locals.len();
        thread_ref
            .lambda_proxy_locals
            .push(LambdaProxyLocals { slots });
        let depth = thread_ref.frames.len();
        let rows = &mut thread_ref.jni_native_frames;
        let base = rows.len();
        // Widening: the record's number into the key's middle bits.
        let number = (locals_base as u64 + 1) << ROW_KEY_PROXY_LOCALS_SHIFT;
        rows.push((
            depth,
            proxy,
            ROW_KEY_IS_PROXY | number | u64::from(index),
            jit_chain_len(),
        ));
        Self {
            _row: NativeFrameRow { thread, base },
            thread,
            pin_base,
            locals_base,
        }
    }
}

impl LambdaProxyFrame {
    /// The frame's receiver and arguments at their current addresses (a
    /// collection while an event's callback or a debugger park ran may have
    /// moved them), from the pins [`Self::enter`] made. `None` when the record
    /// is gone (never, while the frame lives).
    pub(crate) fn current(&self) -> Option<(ObjectRef, Vec<Value>)> {
        // SAFETY: `thread` outlives the frame (`enter`'s contract); only read.
        let thread = unsafe { &*self.thread };
        let mut values = proxy_frame_values(
            &thread.lambda_proxy_locals,
            &thread.native_pin_roots,
            ROW_KEY_IS_PROXY | ((self.locals_base as u64 + 1) << ROW_KEY_PROXY_LOCALS_SHIFT),
        )?;
        let Value::Object(Some(receiver)) = values.first().copied()? else {
            return None;
        };
        // The arguments as passed: one value each, a `long`'s or a
        // `double`'s second slot dropped.
        values.remove(0);
        values.retain(|v| !matches!(v, Value::Uninitialized));
        Some((receiver, values))
    }
}

impl Drop for LambdaProxyFrame {
    fn drop(&mut self) {
        // SAFETY: `thread` outlives the frame (`enter`'s contract).
        let thread = unsafe { &mut *self.thread };
        thread.lambda_proxy_locals.truncate(self.locals_base);
        thread.native_pin_roots.truncate(self.pin_base);
    }
}

/// The locals of the lambda proxy frame a row's `key` names, each
/// reference at its current (pinned) address and a `long`'s or a
/// `double`'s second slot `Uninitialized`, from a thread's own records and
/// pins: what a debugger's listing made on that thread publishes
/// (`interpreter::ListingSplice::values`; the i54-L1 retirement). `None`
/// for a row with no record.
pub(crate) fn proxy_frame_values(
    records: &[crate::threading::jvm_thread::LambdaProxyLocals],
    pins: &[ObjectRef],
    key: u64,
) -> Option<Vec<Value>> {
    use crate::threading::jvm_thread::ProxyLocalSlot as S;
    let record = records.get(proxy_row_locals(key)?)?;
    record
        .slots
        .iter()
        .map(|slot| {
            Some(match *slot {
                S::Int(v) => Value::Int(v),
                S::Long(v) => Value::Long(v),
                S::Float(v) => Value::Float(v),
                S::Double(v) => Value::Double(v),
                S::Ref { pin, null } => {
                    Value::Object(if null { None } else { Some(*pins.get(pin)?) })
                }
                S::Second => Value::Uninitialized,
            })
        })
        .collect()
}

/// Note the listing facts of method `index` of lambda proxy `proxy`
/// (`DebugRealm::lambda_proxy_rows`) the first time a recorded call enters
/// it, on the calling thread, which may read the class store (the i54-L1
/// retirement). One lock and lookup afterwards; only while proxy frames are
/// recorded (`DebuggerGates::c_lambda_frames_armed`).
pub(crate) fn note_lambda_proxy_row(shared: &SharedVm, proxy: ClassId, index: u16) {
    if shared
        .debug
        .lambda_proxy_rows
        .lock()
        .contains_key(&(proxy, index))
    {
        return;
    }
    if let Some(row) =
        crate::runtime::lambda_proxy::lambda_proxy_row(shared, proxy, usize::from(index))
    {
        shared
            .debug
            .lambda_proxy_rows
            .lock()
            .insert((proxy, index), row);
    }
}

/// The method index of lambda proxy `proxy` whose JDWP method id is
/// `method_id`, among the methods recorded calls entered
/// (`DebugRealm::lambda_proxy_rows`): how a JDWP listing's proxy row
/// (`interpreter::ListingSplice`) becomes a `jmethodID` again (the i54-L1
/// retirement).
fn proxy_method_of_jdwp(shared: &SharedVm, proxy: ClassId, method_id: u64) -> Option<u16> {
    shared
        .debug
        .lambda_proxy_rows
        .lock()
        .iter()
        .find(|((id, _), row)| {
            *id == proxy && crate::debug::jdwp_method_id(&row.name, &row.descriptor) == method_id
        })
        .map(|((_, index), _)| *index)
}

/// Local `slot` of `kind` of lambda proxy frame `record` of `thread`
/// ([`LambdaProxyFrame`]): a reference is its current (pinned) address.
/// `JVMTI_ERROR_INVALID_SLOT` past the frame's slots, `TYPE_MISMATCH` for
/// another kind, as HotSpot answers for the spun frame (MEASURED,
/// `L1K54JvmtiLambdaProxyLocalsAndFields`).
fn proxy_local_read(
    thread: &JvmThread,
    record: usize,
    slot: JInt,
    kind: LocalKind,
) -> Result<Value, JInt> {
    use crate::threading::jvm_thread::ProxyLocalSlot as S;
    let slots = &thread
        .lambda_proxy_locals
        .get(record)
        .ok_or(ERR_OPAQUE_FRAME)?
        .slots;
    let at = usize::try_from(slot)
        .ok()
        .filter(|&s| s < slots.len())
        .ok_or(ERR_INVALID_SLOT)?;
    match (kind, slots[at]) {
        (LocalKind::Int, S::Int(v)) => Ok(Value::Int(v)),
        (LocalKind::Long, S::Long(v)) => Ok(Value::Long(v)),
        (LocalKind::Float, S::Float(v)) => Ok(Value::Float(v)),
        (LocalKind::Double, S::Double(v)) => Ok(Value::Double(v)),
        (LocalKind::Object, S::Ref { pin, null }) => {
            let obj = thread
                .native_pin_roots
                .get(pin)
                .copied()
                .ok_or(ERR_OPAQUE_FRAME)?;
            Ok(Value::Object((!null).then_some(obj)))
        }
        _ => Err(ERR_TYPE_MISMATCH),
    }
}

/// Write `value` (of `kind`) into local `slot` of lambda proxy frame
/// `record` of `thread` ([`proxy_local_read`]'s errors); a reference
/// replaces the slot's pin (a null one pins the proxy, marked null).
fn proxy_local_write(
    thread: &mut JvmThread,
    record: usize,
    slot: JInt,
    kind: LocalKind,
    value: Value,
) -> Result<(), JInt> {
    use crate::threading::jvm_thread::ProxyLocalSlot as S;
    let receiver_pin = match thread
        .lambda_proxy_locals
        .get(record)
        .and_then(|r| r.slots.first())
    {
        Some(&S::Ref { pin, .. }) => pin,
        _ => return Err(ERR_OPAQUE_FRAME),
    };
    let receiver = thread
        .native_pin_roots
        .get(receiver_pin)
        .copied()
        .ok_or(ERR_OPAQUE_FRAME)?;
    let slots = &mut thread.lambda_proxy_locals[record].slots;
    let at = usize::try_from(slot)
        .ok()
        .filter(|&s| s < slots.len())
        .ok_or(ERR_INVALID_SLOT)?;
    let written = match (kind, slots[at], value) {
        (LocalKind::Int, S::Int(_), Value::Int(v)) => S::Int(v),
        (LocalKind::Long, S::Long(_), Value::Long(v)) => S::Long(v),
        (LocalKind::Float, S::Float(_), Value::Float(v)) => S::Float(v),
        (LocalKind::Double, S::Double(_), Value::Double(v)) => S::Double(v),
        (LocalKind::Object, S::Ref { pin, .. }, Value::Object(obj)) => {
            let target = thread
                .native_pin_roots
                .get_mut(pin)
                .ok_or(ERR_OPAQUE_FRAME)?;
            *target = obj.unwrap_or(receiver);
            S::Ref {
                pin,
                null: obj.is_none(),
            }
        }
        _ => return Err(ERR_TYPE_MISMATCH),
    };
    slots[at] = written;
    Ok(())
}

/// The `JvmThread::running_native` a [`CInNativeMark`] records when none is
/// recorded, so that `interpreter::NestedJavaAck` lowers the mark for Java
/// the native calls. Never a native's callback address.
pub(crate) const C_IN_NATIVE_RUNNING: usize = crate::runtime::interpreter::C_NATIVE_CALLBACK;

/// A JNI native's C code runs on this thread (interpreter round i2 wave 58,
/// lane L1; `DebuggerGates::c_in_native_armed`,
/// `docs/feature-designs/jvmti-delivery-threading.md`):
/// raises `GcBlockState::jvmti_in_native` around the JNI door's call, so
/// that [`get_thread_state`] answers `JVMTI_THREAD_STATE_IN_NATIVE` for the
/// thread, as HotSpot answers for a thread in `_thread_in_native` (MEASURED,
/// `tools/probes/interp/L1/L1W58JvmtiInNative.java`: a native spinning in C
/// `0x400005`, its Java upcall `0x5`). Without a debugger session it also
/// records [`C_IN_NATIVE_RUNNING`] as the running native when none is, so
/// that Java the native calls lowers the mark while it runs
/// (`interpreter::NestedJavaAck`); with one, the record is the debugger's
/// and is left alone. Inert, one test and one load, unless a C JVMTI env is
/// bound and the switch is on. `Drop` restores what the caller had.
pub(crate) struct CInNativeMark {
    thread: *mut JvmThread,
    engaged: bool,
    prior: bool,
    recorded: bool,
}

impl CInNativeMark {
    /// Raise the mark for `thread`'s JNI native call.
    ///
    /// # Safety
    ///
    /// `thread` is the calling thread's live `JvmThread`, and outlives the
    /// mark.
    #[inline]
    pub(crate) unsafe fn enter(shared: &SharedVm, thread: *mut JvmThread) -> Self {
        if !shared.debug.debugger_gates.c_in_native_armed() {
            return Self {
                thread,
                engaged: false,
                prior: false,
                recorded: false,
            };
        }
        // SAFETY: the caller's contract.
        unsafe { Self::enter_armed(shared, thread) }
    }

    #[cold]
    #[inline(never)]
    unsafe fn enter_armed(shared: &SharedVm, thread: *mut JvmThread) -> Self {
        // SAFETY: the caller's contract: this thread's own live `JvmThread`.
        let thread_ref = unsafe { &mut *thread };
        let prior = thread_ref
            .gc_block_state
            .jvmti_in_native
            .swap(true, std::sync::atomic::Ordering::AcqRel);
        let recorded =
            thread_ref.running_native == 0 && !shared.debug.debugger_gates.session_attached();
        if recorded {
            thread_ref.running_native = C_IN_NATIVE_RUNNING;
        }
        if crate::runtime::env_cache::frame_trace() {
            eprintln!("[JVMTI_C_IN_NATIVE] tid={} enter", thread_ref.thread_id.0);
        }
        Self {
            thread,
            engaged: true,
            prior,
            recorded,
        }
    }
}

impl Drop for CInNativeMark {
    #[inline]
    fn drop(&mut self) {
        if !self.engaged {
            return;
        }
        // SAFETY: `thread` outlives the mark (`enter`'s contract).
        let thread = unsafe { &mut *self.thread };
        thread
            .gc_block_state
            .jvmti_in_native
            .store(self.prior, std::sync::atomic::Ordering::Release);
        if self.recorded && thread.running_native == C_IN_NATIVE_RUNNING {
            thread.running_native = 0;
        }
    }
}

/// Does live thread `tid` run native code with no Java above it, as HotSpot's
/// `_thread_in_native` (wave 58; [`CInNativeMark`])? A JNI native's C code
/// under the mark, a JNI native or a GC-safe FFM downcall in the in-native
/// region (`GcBlockState::foreign_native` with `in_blocked_region`), or a
/// lazy FFM downcall. Never the calling thread: it is in the VM, in this
/// function (HotSpot MEASURED: `GetThreadState` of itself from a native
/// answers `0x5`).
#[cold]
#[inline(never)]
fn thread_in_native(shared: &SharedVm, tid: u64) -> bool {
    let current = jni::current_jvm_thread_of(shared)
        // SAFETY: the calling thread's own, live `JvmThread`; `thread_id` is
        // an immutable `Copy` field.
        .map(|current| unsafe { (*current).thread_id.0 });
    if current == Some(tid) {
        return false;
    }
    // Wave 59 (lane L6): the registry's one in-native answer, which also
    // counts a registered native blocked in C (`BlockKind::InNative`,
    // HotSpot MEASURED `0x400005` for a thread in `ServerSocket.accept`).
    let registry = &shared.threads.thread_registry;
    if registry.thread_in_native(ThreadId(tid)) {
        return true;
    }
    // Interpreter round i2 wave 61, lane L6 (`CRATONVM_JVMTI_C_RUNNING_NATIVE`):
    // a thread RUNNING a funnel native outside a blocking region, with no
    // Java above it (its native-excuse word raised), whose callback stands in
    // for a JDK `native` method (HotSpot MEASURED `0x400005` inside
    // `Deflater.deflateBytesBytes`, `L6W61JvmtiRunningNativeState`); not one
    // shadowing a Java method (`0x5`, HotSpot runs the Java).
    if !shared.debug.debugger_gates.c_running_native_armed()
        || !registry.running_funnel_native(ThreadId(tid))
    {
        if crate::runtime::env_cache::frame_trace() {
            eprintln!(
                "[JVMTI_C_RUNNING_NATIVE] tid={tid} armed={} running_funnel_native=false word=0x{:x} blocked={}",
                shared.debug.debugger_gates.c_running_native_armed(),
                registry.native_excuse_word(ThreadId(tid)),
                registry.is_blocked(ThreadId(tid)),
            );
        }
        return false;
    }
    // Wave 62 (lane L6, `CRATONVM_HANDSHAKE_NATIVE_CALLBACK`): the callback
    // the raised excuse word itself records, when it does; the debugger
    // gate's record otherwise.
    let callback = registry
        .running_funnel_callback(ThreadId(tid))
        .or_else(|| registry.funnel_native_callback(ThreadId(tid)));
    let native = callback.is_some_and(|cb| {
        crate::runtime::interpreter::funnel_callback_runs_a_native_method(shared, cb)
    });
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[JVMTI_C_RUNNING_NATIVE] tid={tid} callback=0x{:x} native={native}",
            callback.unwrap_or(0)
        );
    }
    native
}

/// The VM a stack function acts on, when it is in the live phase (the stack
/// functions are live-phase only).
fn live_phase_vm(env: *mut JvmtiNativeEnv) -> Result<Arc<SharedVm>, JInt> {
    // gci i16/sn (`CRATONVM_DEBUG_SUSPEND_AT_NATIVE_TRANSITION`): the C
    // functions behind this prologue -- the thread, stack, suspension and
    // agent-thread functions -- are VM entries in HotSpot (`JVMTI_ENTRY`,
    // `ThreadInVMfromNative`), so a suspended caller whose C code runs in
    // native stops here as at a JNIEnv function, before it reads anything.
    // Inert unless this thread's innermost JNI native call is in native.
    jni::hold_in_native_while_suspended();
    let env = live_env(env)?;
    if env.outside_live_phase() {
        return Err(ERR_WRONG_PHASE);
    }
    env.vm().ok_or(ERR_WRONG_PHASE)
}

/// Slot 18: `GetCurrentThread(env, thread_ptr)` (wave 22): the calling
/// thread's `java.lang.Thread` as a local reference, NULL while it has none;
/// `JVMTI_ERROR_UNATTACHED_THREAD` for a thread the VM does not know.
extern "C" fn get_current_thread(env: *mut JvmtiNativeEnv, thread_ptr: *mut JObject) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let Some(shared) = env.start_or_live_vm() else {
        return ERR_WRONG_PHASE;
    };
    if thread_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let Some(current) = jni::current_jvm_thread_of(&shared) else {
        return ERR_UNATTACHED_THREAD;
    };
    // SAFETY: the calling thread's own `JvmThread`; `thread_id` is `Copy`.
    let tid = unsafe { (*current).thread_id };
    // The mirror is read and made a handle inside the foreign-thread entry,
    // as `thread_id_of` reads one, and in `shared`'s JNI context.
    let _fx = jni::ForeignJniEntry::enter();
    let prev = jni::replace_jni_context(&shared);
    let handle = shared
        .threads
        .thread_registry
        .java_thread_obj(tid)
        .map_or(0, jni::new_local_handle);
    jni::restore_jni_context(prev);
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *thread_ptr = handle };
    ERR_NONE
}

/// Slot 16: `GetFrameCount(env, thread, count_ptr)` (wave 22; the current
/// thread, [`frames_of`]).
extern "C" fn get_frame_count(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    count_ptr: *mut JInt,
) -> JInt {
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    if count_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let frames = match frames_of(&shared, thread) {
        Ok(frames) => frames,
        Err(code) => return code,
    };
    let Ok(count) = JInt::try_from(frames.len()) else {
        return ERR_INTERNAL;
    };
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *count_ptr = count };
    ERR_NONE
}

/// Slot 19: `GetFrameLocation(env, thread, depth, method_ptr, location_ptr)`
/// (wave 22; the current thread, [`frames_of`]): depth 0 is the top frame;
/// `JVMTI_ERROR_NO_MORE_FRAMES` past the bottom.
extern "C" fn get_frame_location(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    method_ptr: *mut u64,
    location_ptr: *mut i64,
) -> JInt {
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let Ok(depth) = usize::try_from(depth) else {
        return ERR_ILLEGAL_ARGUMENT;
    };
    if method_ptr.is_null() || location_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let frames = match frames_of(&shared, thread) {
        Ok(frames) => frames,
        Err(code) => return code,
    };
    let Some(frame) = frames.get(depth) else {
        return ERR_NO_MORE_FRAMES;
    };
    // SAFETY: the agent's non-null out-parameters.
    unsafe {
        *method_ptr = frame.method;
        *location_ptr = frame.location;
    }
    ERR_NONE
}

/// Slot 104: `GetStackTrace(env, thread, start_depth, max_frame_count,
/// frame_buffer, count_ptr)` (wave 22; the current thread, [`frames_of`]).
/// A non-negative `start_depth` counts from the top, a negative one from the
/// bottom (`-n`: the lowest `n` frames); frames are written from there toward
/// the bottom, at most `max_frame_count`. `JVMTI_ERROR_ILLEGAL_ARGUMENT` for
/// a positive `start_depth` at or past the depth, a negative one below
/// `-depth`, or a negative `max_frame_count`, as the specification lists.
extern "C" fn get_stack_trace(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    start_depth: JInt,
    max_frame_count: JInt,
    frame_buffer: *mut FrameInfo,
    count_ptr: *mut JInt,
) -> JInt {
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let Ok(max) = usize::try_from(max_frame_count) else {
        return ERR_ILLEGAL_ARGUMENT;
    };
    if frame_buffer.is_null() || count_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let frames = match frames_of(&shared, thread) {
        Ok(frames) => frames,
        Err(code) => return code,
    };
    let Some(start) = stack_trace_start(frames.len(), start_depth) else {
        return ERR_ILLEGAL_ARGUMENT;
    };
    let taken = &frames[start..];
    let taken = &taken[..taken.len().min(max)];
    let Ok(count) = JInt::try_from(taken.len()) else {
        return ERR_INTERNAL;
    };
    // SAFETY: the agent's buffer holds at least `max_frame_count` entries
    // (the function's contract), and `taken.len()` is at most that; the
    // out-parameters are non-null.
    unsafe {
        std::ptr::copy_nonoverlapping(taken.as_ptr(), frame_buffer, taken.len());
        *count_ptr = count;
    }
    ERR_NONE
}

/// The index (into the frames, top first) `GetStackTrace` starts at for
/// `start_depth` over a stack of `depth` frames, or `None` for the
/// specification's `ILLEGAL_ARGUMENT` cases.
fn stack_trace_start(depth: usize, start_depth: JInt) -> Option<usize> {
    if start_depth >= 0 {
        let start = usize::try_from(start_depth).ok()?;
        // "positive and greater than or equal to stackDepth": a zero start
        // over an empty stack answers no frames.
        (start == 0 || start < depth).then_some(start)
    } else {
        let from_bottom = usize::try_from(start_depth.unsigned_abs()).ok()?;
        depth.checked_sub(from_bottom)
    }
}

// ---------------------------------------------------------------------------
// Every thread's stack (interpreter round i1 wave 46, lane L1)
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/jvmti-delivery-threading.md`:
// sampling profilers and thread-dump agents enumerate threads with
// `GetAllThreads` and read every stack with one `GetAllStackTraces` or
// `GetThreadListStackTraces`, which answered `JVMTI_ERROR_NOT_AVAILABLE`, so
// such an agent saw no stack at all on this VM. Each stack is read as
// `GetStackTrace` reads it ([`frames_of`]): the calling thread's own, another
// through its window, its park or a handshake of its own. HotSpot reads
// every thread at one safepoint, so its stacks are consistent with each
// other; these are read one after another. Codes as HotSpot 25.0.3's,
// measured (`tools/probes/interp/L1/L1W46JvmtiAllStackTraces.java`).

/// `jvmtiStackInfo`: `{ jthread thread; jint state; jvmtiFrameInfo
/// *frame_buffer; jint frame_count; }`, 32 bytes with its padding.
#[repr(C)]
#[derive(Clone, Copy)]
struct StackInfoRow {
    thread: JObject,
    state: JInt,
    frame_buffer: *mut FrameInfo,
    frame_count: JInt,
}

/// The live PLATFORM threads' `java.lang.Thread` objects, in the order they
/// registered (interpreter round i5 wave 102, lane L1). JVMTI's
/// `GetAllThreads`, `GetAllStackTraces` and `GetThreadGroupChildren` return
/// "live platform threads"; virtual threads are not returned by them (JDK 21+
/// JVMTI specification; HotSpot walks its threads list, where a virtual
/// thread is never an entry). This VM registers each virtual thread as an
/// entry of its own whose mirror is the `VirtualThread`, so every one of them
/// was listed, in the registry map's hash order, which also changed from run
/// to run where HotSpot's list keeps attach order.
fn live_platform_thread_objects(shared: &SharedVm) -> Vec<ObjectRef> {
    let mut threads = shared
        .threads
        .thread_registry
        .alive_thread_objects_in_id_order();
    threads.retain(|&t| !extensions::is_virtual_thread_object(shared, t));
    threads
}

/// Every live thread of `shared` that has a `java.lang.Thread`, as local
/// references of the calling thread (made inside its foreign-thread entry,
/// in `shared`'s JNI context).
fn all_thread_handles(shared: &SharedVm) -> Vec<JObject> {
    let _fx = jni::ForeignJniEntry::enter();
    // Interpreter round i5 wave 102, lane L1: platform threads only, in
    // attach order (see [`live_platform_thread_objects`]).
    let threads = live_platform_thread_objects(shared);
    let prev = jni::replace_jni_context(shared);
    let handles = threads.into_iter().map(jni::new_local_handle).collect();
    jni::restore_jni_context(prev);
    handles
}

/// Slot 4: `GetAllThreads(env, threads_count_ptr, threads_ptr)` (wave 46;
/// no capability): the VM's live threads, in no particular order, as local
/// references in an `Allocate`d array.
extern "C" fn get_all_threads(
    env: *mut JvmtiNativeEnv,
    threads_count_ptr: *mut JInt,
    threads_ptr: *mut *mut JObject,
) -> JInt {
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    if threads_count_ptr.is_null() || threads_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let handles = all_thread_handles(&shared);
    let Ok(count) = JInt::try_from(handles.len()) else {
        return ERR_INTERNAL;
    };
    let Some(size) = handles.len().checked_mul(std::mem::size_of::<JObject>()) else {
        return ERR_OUT_OF_MEMORY;
    };
    let block = allocate_block(size).cast::<JObject>();
    if block.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    // SAFETY: `block` holds `handles.len()` handles (`allocate_block` of
    // their size, 16-aligned); the out-parameters are the agent's, non-null.
    unsafe {
        std::ptr::copy_nonoverlapping(handles.as_ptr(), block, handles.len());
        *threads_count_ptr = count;
        *threads_ptr = block;
    }
    ERR_NONE
}

/// One thread's `jvmtiStackInfo` before it is written out: the handle, the
/// `GetThreadState` answer and at most `max` frames, top first.
struct StackOf {
    thread: JObject,
    state: JInt,
    frames: Vec<FrameInfo>,
}

/// [`StackOf`] for thread `handle`: a thread never started is state 0 with
/// no frames, a terminated one (or one that ends while it is read)
/// `TERMINATED` with none, as HotSpot 25.0.3 answers them;
/// `JVMTI_ERROR_INVALID_THREAD` for a handle that names no thread.
fn stack_of(shared: &SharedVm, handle: JObject, max: usize) -> Result<StackOf, JInt> {
    let (state, mut frames) = match lookup_thread(shared, handle)? {
        ThreadLookup::Unstarted => (0, Vec::new()),
        ThreadLookup::Dead => (THREAD_STATE_TERMINATED, Vec::new()),
        ThreadLookup::Live(tid) => {
            let frames = match frames_of(shared, handle) {
                Ok(frames) => frames,
                Err(ERR_THREAD_NOT_ALIVE) => Vec::new(),
                // A thread that did not stand still in time (a JNI native
                // running outside a blocking region, a compiled loop with
                // no exit-capable poll): listed with no frames rather than
                // failing every other thread's stack. HotSpot's handshake
                // reaches such a thread.
                Err(ERR_NOT_AVAILABLE) => {
                    if crate::runtime::env_cache::frame_trace() {
                        eprintln!("[JVMTI_ALL_STACKS] tid={tid} not stopped: no frames");
                    }
                    Vec::new()
                }
                Err(code) => return Err(code),
            };
            let state = if shared.threads.thread_registry.is_alive(ThreadId(tid)) {
                live_thread_state(shared, tid)
            } else {
                THREAD_STATE_TERMINATED
            };
            (state, frames)
        }
    };
    frames.truncate(max);
    Ok(StackOf {
        thread: handle,
        state,
        frames,
    })
}

/// `stacks` as one `Allocate`d block: the `jvmtiStackInfo` array, then every
/// frame buffer after it ("the frame buffers are allocated in the same
/// block"); null when out of memory.
fn allocate_stack_infos(stacks: &[StackOf]) -> *mut StackInfoRow {
    let frames: usize = stacks.iter().map(|s| s.frames.len()).sum();
    let size = stacks
        .len()
        .checked_mul(std::mem::size_of::<StackInfoRow>())
        .and_then(|head| {
            frames
                .checked_mul(std::mem::size_of::<FrameInfo>())
                .and_then(|tail| head.checked_add(tail))
        });
    let Some(size) = size else {
        return std::ptr::null_mut();
    };
    let block = allocate_block(size);
    if block.is_null() {
        return std::ptr::null_mut();
    }
    let rows = block.cast::<StackInfoRow>();
    // SAFETY: the frame buffers start right after the rows, inside the
    // block, 8-aligned (a row is 32 bytes and the block 16-aligned).
    let mut buffer = unsafe { block.add(stacks.len() * std::mem::size_of::<StackInfoRow>()) }
        .cast::<FrameInfo>();
    for (i, stack) in stacks.iter().enumerate() {
        let count = JInt::try_from(stack.frames.len()).unwrap_or(JInt::MAX);
        // SAFETY: `block` holds `stacks.len()` rows and every stack's frames
        // after them (`size`); `buffer` stays inside it.
        unsafe {
            std::ptr::copy_nonoverlapping(stack.frames.as_ptr(), buffer, stack.frames.len());
            rows.add(i).write(StackInfoRow {
                thread: stack.thread,
                state: stack.state,
                frame_buffer: buffer,
                frame_count: count,
            });
            buffer = buffer.add(stack.frames.len());
        }
    }
    rows
}

/// Slot 101: `GetThreadListStackTraces(env, thread_count, thread_list,
/// max_frame_count, stack_info_ptr)` (wave 46; no capability): each listed
/// thread's state and at most `max_frame_count` frames, in the list's
/// order. The checks in HotSpot 25.0.3's order: a negative count
/// (`ILLEGAL_ARGUMENT`), a NULL list (`NULL_POINTER`), a negative
/// `max_frame_count` (`ILLEGAL_ARGUMENT`), a NULL `stack_info_ptr`
/// (`NULL_POINTER`), then a handle naming no thread (`INVALID_THREAD`).
extern "C" fn get_thread_list_stack_traces(
    env: *mut JvmtiNativeEnv,
    thread_count: JInt,
    thread_list: *const JObject,
    max_frame_count: JInt,
    stack_info_ptr: *mut *mut StackInfoRow,
) -> JInt {
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let Ok(count) = usize::try_from(thread_count) else {
        return ERR_ILLEGAL_ARGUMENT;
    };
    if thread_list.is_null() {
        return ERR_NULL_POINTER;
    }
    let Ok(max) = usize::try_from(max_frame_count) else {
        return ERR_ILLEGAL_ARGUMENT;
    };
    if stack_info_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's list holds `thread_count` handles (the function's
    // contract), and it is non-null.
    let handles = unsafe { std::slice::from_raw_parts(thread_list, count) }.to_vec();
    let mut stacks = Vec::with_capacity(handles.len());
    for handle in handles {
        match stack_of(&shared, handle, max) {
            Ok(stack) => stacks.push(stack),
            Err(code) => return code,
        }
    }
    let block = allocate_stack_infos(&stacks);
    if block.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *stack_info_ptr = block };
    ERR_NONE
}

/// Slot 100: `GetAllStackTraces(env, max_frame_count, stack_info_ptr,
/// thread_count_ptr)` (wave 46; no capability): [`get_thread_list_stack_traces`]
/// over [`get_all_threads`]' threads; a thread that ends while the others
/// are read is left out.
extern "C" fn get_all_stack_traces(
    env: *mut JvmtiNativeEnv,
    max_frame_count: JInt,
    stack_info_ptr: *mut *mut StackInfoRow,
    thread_count_ptr: *mut JInt,
) -> JInt {
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let Ok(max) = usize::try_from(max_frame_count) else {
        return ERR_ILLEGAL_ARGUMENT;
    };
    if stack_info_ptr.is_null() || thread_count_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let mut stacks = Vec::new();
    for handle in all_thread_handles(&shared) {
        match stack_of(&shared, handle, max) {
            Ok(stack) if stack.state & THREAD_STATE_ALIVE != 0 => stacks.push(stack),
            // Ended meanwhile (or not a thread any more): not listed.
            Ok(_) | Err(ERR_THREAD_NOT_ALIVE | ERR_INVALID_THREAD) => {}
            Err(code) => return code,
        }
    }
    let Ok(count) = JInt::try_from(stacks.len()) else {
        return ERR_INTERNAL;
    };
    let block = allocate_stack_infos(&stacks);
    if block.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[JVMTI_ALL_STACKS] threads={count}");
    }
    // SAFETY: the agent's non-null out-parameters.
    unsafe {
        *stack_info_ptr = block;
        *thread_count_ptr = count;
    }
    ERR_NONE
}

// ---------------------------------------------------------------------------
// Other threads: suspension, thread state and stacks (interpreter round i1
// wave 45, lane L1)
// ---------------------------------------------------------------------------
//
// Stage 1 of
// `docs/feature-designs/jvmti-delivery-threading.md`.
// A C agent's `SuspendThread` is the JDWP suspension's park
// (`DebugState::jvmti_suspend`: the thread stops at its next interpreter
// suspend point, or at the return of the native it runs, and a thread blocked
// in a native region stays there), with JVMTI's flag semantics. The stack
// functions read another thread where it stands still: blocked, through its
// inspection window (`interpreter::blocked_frame_rows`); parked, from the
// listing its park published (`DebugState::thread_frames`); running, after a
// handshake, a suspension of its own for the read (`DebugState::begin_handshake`).
// Each code is HotSpot 25.0.3's, measured
// (`tools/probes/interp/L1/L1W45JvmtiSuspendAndOtherStacks.java`).

/// How long a `SuspendThread` or a stack read waits for another thread to
/// stop: a thread in a compiled loop stops only at an exit-capable poll
/// (`interpreter::request_compiled_loop_exits`). A suspension that is not
/// reached by then still stands; a stack read answers
/// `JVMTI_ERROR_NOT_AVAILABLE`.
const STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Wait, polling every millisecond, until `done` answers `true` (then
/// `true`) or `deadline` passes (`false`). The calling thread waits GC-safe:
/// a registered mutator (a JNI native, an event callback) waits inside a
/// blocking region, as a native's `Thread.sleep` does, so a collection that
/// another thread (the one it waits for, perhaps) starts meanwhile does not
/// wait for it. A thread already GC-blocked (an attached agent thread idle
/// between calls) or unknown to the VM just sleeps.
fn wait_gc_safe(
    shared: &SharedVm,
    deadline: Option<std::time::Instant>,
    mut done: impl FnMut() -> bool,
) -> bool {
    if done() {
        return true;
    }
    gc_safe(shared, || loop {
        if done() {
            break true;
        }
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    })
}

/// Run `work`, which waits for another thread, with the calling thread
/// GC-safe (see [`wait_gc_safe`]).
fn gc_safe<R>(shared: &SharedVm, work: impl FnOnce() -> R) -> R {
    use cratonvm_native_api::NativeThreadAccess as _;
    let current = jni::current_jvm_thread_of(shared).filter(|&thread| {
        // SAFETY: the calling thread's own, live `JvmThread`.
        let blocked = unsafe { &*thread }
            .gc_block_state
            .in_blocked_region
            .load(std::sync::atomic::Ordering::Acquire);
        !blocked
    });
    let mut region = current.map(|thread| crate::vm::vm_exec::NativeContextImpl {
        shared,
        // SAFETY: the calling thread's own, live `JvmThread`, which this call
        // runs on and does not otherwise touch until the region ends.
        thread: unsafe { &mut *thread },
    });
    if let Some(ctx) = region.as_mut() {
        // Interpreter round i2 wave 66, lane L1
        // (`CRATONVM_VM_INTERNAL_WAITS_RUNNABLE`): HotSpot waits for the
        // target inside the VM (a handshake, `_thread_blocked` in a JVM
        // entry), so the caller reads RUNNABLE, not a WAITING region with no
        // wait kind.
        if shared.threads.thread_registry.vm_internal_waits_runnable() {
            ctx.begin_vm_wait_region();
        } else {
            ctx.begin_blocking_region();
        }
    }
    let answer = work();
    if let Some(ctx) = region.as_mut() {
        ctx.end_blocking_region();
    }
    answer
}

/// Has thread `tid` stopped for a suspension: parked at a suspend point,
/// blocked in a native region, or gone?
fn thread_stopped(shared: &SharedVm, tid: u64) -> bool {
    let registry = &shared.threads.thread_registry;
    let id = ThreadId(tid);
    !registry.is_alive(id)
        || registry.is_blocked(id)
        // An unmounted virtual thread runs nothing until it is mounted again,
        // which honours the suspension (interpreter round i1 wave 50, lane L1).
        || shared.threads.virtual_thread_manager.is_deposited(tid)
        // Interpreter round i5 wave 101, lane L1: in a native outside a
        // region with no Java above it, the acknowledgement a JDWP
        // suspension's settle wait already counts stopped (raised only while
        // a JDWP session is attached, `CRATONVM_DEBUG_SUSPEND_NATIVE_ACK`);
        // the native's return parks it, named or not. HotSpot's handshake
        // counts a thread in native safe at once; this wait ran to its 2 s
        // bound for one.
        || (shared.debug.debugger_gates.suspend_native_ack()
            && crate::debug::in_native_acknowledged(registry, id))
        || shared.debug.debug_state.lock().is_parked(tid)
}

/// The listing of thread `tid` while it stands still, top first: parked,
/// the listing its park published in this suspension; blocked, read through
/// its window.
fn stopped_rows(shared: &SharedVm, tid: u64) -> Option<Vec<BlockedRow>> {
    {
        let ds = shared.debug.debug_state.lock();
        if ds.is_parked(tid) {
            if let Some(rows) = published_rows(&ds, tid, None) {
                return Some(rows);
            }
        }
    }
    crate::runtime::interpreter::with_blocked_frames(shared, tid, |held| held.rows.clone())
}

/// The listing parked thread `tid` published for its current suspension
/// (`DebugState::thread_frames`), top first, as [`BlockedRow`]s (interpreter
/// round i1 wave 46, lane L1): with `depth`, the thread's interpreter frame
/// count (known on the thread itself), each interpreter row names its frame,
/// counted from the bottom as `debug::inspect`'s `set_frame_values` counts
/// it; the native method heading the listing and a compiled activation name
/// none. `None` when no listing of this suspension stands. Also JDWP's
/// OSR'd-frame home read (`compiled_frame_locals::publish_osr_home_locals`,
/// j27 wave 8, lane W8INTR).
pub(crate) fn published_rows(
    ds: &crate::debug::DebugState,
    tid: u64,
    depth: Option<usize>,
) -> Option<Vec<BlockedRow>> {
    if !ds.has_current_frames(tid) {
        return None;
    }
    let entries = ds.thread_frames.get(&tid)?;
    let mut from_top = 0usize;
    let rows = entries
        .iter()
        .map(|e| {
            let native = e.offset == crate::debug::NATIVE_FRAME_LOCATION;
            // `true`: a compiled activation; `false`: an interpreter frame
            // whose body runs compiled.
            let opaque = ds.opaque_frames.get(&(tid, e.frame_id)).copied();
            // Interpreter round i7 wave 138, lane L1: a lambda proxy
            // method's row names no interpreter frame
            // (`DebugState::proxy_frames`); counting it shifted every row
            // below it onto the frame under its own.
            let interpreted = !native
                && opaque != Some(true)
                && !ds.proxy_frames.contains(&(tid, e.frame_id));
            let frame = if interpreted {
                let index = depth.and_then(|d| d.checked_sub(from_top + 1));
                from_top += 1;
                index
            } else {
                None
            };
            BlockedRow {
                class_id: e.class_id,
                method_id: e.method_id,
                location: if native {
                    -1
                } else {
                    i64::try_from(e.offset).unwrap_or(-1)
                },
                frame,
                runs_compiled: opaque == Some(false),
                // A published listing keeps no chain index (another thread's:
                // its owned-monitor depths attribute by frame).
                chain: None,
            }
        })
        .collect();
    Some(rows)
}

/// A row of another thread's listing as the C table lists it (interpreter
/// round i1 wave 46, lane L1): its `jvmtiFrameInfo`, and the raw row it came
/// from (the interpreter frame it is, to read its locals and monitors).
#[derive(Clone, Copy)]
struct OtherRow {
    info: FrameInfo,
    raw: BlockedRow,
}

/// `rows` (top first) as the C table lists them: a row whose method no
/// longer resolves in its class is left out (an obsolete method's null JDWP
/// id names none; it has no `jmethodID`), so a depth counts the rows
/// [`frames_of`] answers for the thread.
fn other_rows(shared: &SharedVm, rows: Vec<BlockedRow>) -> Vec<OtherRow> {
    let cm = shared.classes.class_manager.read();
    let mut out = Vec::with_capacity(rows.len());
    for raw in rows {
        let Ok(class_raw) = u32::try_from(raw.class_id) else {
            continue;
        };
        let Some(class) = cm.get_class(ClassId::new(class_raw)) else {
            // The i54-L1 retirement: a lambda proxy method's row.
            if let Some(index) =
                proxy_method_of_jdwp(shared, ClassId::new(class_raw), raw.method_id)
            {
                out.push(OtherRow {
                    info: FrameInfo {
                        // Widening: wave 56's `(proxy id << 32) | index`.
                        method: (u64::from(class_raw) << 32) | u64::from(index),
                        location: raw.location,
                    },
                    raw,
                });
            }
            continue;
        };
        // The JDWP method id is the row hash (`native_row_method_hash`).
        let Some(index) = row_method_index(shared, class, raw.method_id | ROW_KEY_IS_HASH)
            .and_then(|i| u16::try_from(i).ok())
        else {
            continue;
        };
        out.push(OtherRow {
            info: FrameInfo {
                // Widening: u32 class id and u16 index into their halves of the id.
                method: (u64::from(class_raw) << 32) | u64::from(index),
                location: raw.location,
            },
            raw,
        });
    }
    out
}

/// `rows` (top first, [`other_rows`] of the calling thread's shared listing)
/// with a lambda proxy row between each call site and the lambda
/// implementation it reached through a proxy no row recorded, each answered
/// with whether it was inferred (interpreter round i6 wave 120 follow-up,
/// lane L1): the i54-L1 retirement's [`with_inferred_proxy_rows`] on the
/// rows of the builder JDWP uses, for `CRATONVM_JVMTI_C_LISTING_SHARED`
/// while `DebuggerGates::proxy_rows_incomplete` holds. The same inference
/// (`stackwalker::lambda_proxy_standin_frames`, fail-closed) over each pair
/// of adjacent rows that name a Java method at a code index (an interpreter
/// frame, a compiled or inlined activation, a spliced JDK frame); a native
/// row (-1) or a recorded proxy row stands between a call and its target
/// already. The inferred row is the proxy's functional method (index 1, wave
/// 56's scheme) at its forwarding call, with no locals record, as
/// [`listed_rows`]'s own path lists it.
fn with_inferred_proxy_other_rows(shared: &SharedVm, rows: Vec<OtherRow>) -> Vec<(OtherRow, bool)> {
    use crate::native::registry::StackTraceEntry;
    // Each row as a stack-trace entry, when it names a Java method at an
    // index: what the inference reads (the caller's invoke, the callee's
    // method).
    let entries: Vec<Option<StackTraceEntry>> = {
        let cm = shared.classes.class_manager.read();
        rows.iter()
            .map(|row| {
                let raw = &row.raw;
                let class_id = ClassId::new(u32::try_from(raw.class_id).ok()?);
                let bci = i32::try_from(raw.location).ok().filter(|&bci| bci >= 0)?;
                let class = cm.get_class(class_id)?;
                let (index, method) = class.methods.iter().enumerate().find(|(_, m)| {
                    crate::debug::jdwp_method_id(&m.name, &m.descriptor) == raw.method_id
                })?;
                Some(StackTraceEntry {
                    class_name: Arc::clone(&class.name),
                    method_name: Arc::clone(&method.name),
                    method_descriptor: Some(Arc::clone(&method.descriptor)),
                    source_file: None,
                    line_number: -1,
                    byte_code_index: bci,
                    class_id: Some(class_id),
                    method_index: u32::try_from(index).ok(),
                })
            })
            .collect()
    };
    // Named before the class-manager lock below (the proxies' host names).
    let trace: Vec<StackTraceEntry> = entries.iter().flatten().cloned().collect();
    let candidates = inferred_proxy_candidates(shared, &trace);
    if candidates.is_empty() {
        return rows.into_iter().map(|row| (row, false)).collect();
    }
    let cm = shared.classes.class_manager.read();
    let mut out = Vec::with_capacity(rows.len() + 2);
    for (i, row) in rows.into_iter().enumerate() {
        // Top first: row `i - 1` is the callee of row `i`.
        let callee = i.checked_sub(1).and_then(|c| entries[c].as_ref());
        if let (Some(callee), Some(caller)) = (callee, entries[i].as_ref()) {
            let pair = [callee.clone(), caller.clone()];
            let inferred = crate::runtime::stackwalker::lambda_proxy_standin_frames(
                &cm.class_store,
                &pair,
                &candidates,
            );
            if let Some(entry) = inferred.first().and_then(|(_, entries)| entries.first()) {
                if let Some(proxy) = entry.class_id {
                    if crate::runtime::env_cache::frame_trace() {
                        eprintln!(
                            "[JVMTI_C_LAMBDA_FRAMES] shared inferred class={:#x} location={}",
                            proxy.as_u32(),
                            entry.byte_code_index
                        );
                    }
                    let location = i64::from(entry.byte_code_index);
                    out.push((
                        OtherRow {
                            info: FrameInfo {
                                // Widening: the functional method, index 1
                                // (wave 56's scheme).
                                method: (u64::from(proxy.as_u32()) << 32) | 1,
                                location,
                            },
                            raw: BlockedRow {
                                class_id: u64::from(proxy.as_u32()),
                                method_id: crate::debug::jdwp_method_id(
                                    &entry.method_name,
                                    entry.method_descriptor.as_deref().unwrap_or(""),
                                ),
                                location,
                                frame: None,
                                runs_compiled: false,
                                chain: None,
                            },
                        },
                        true,
                    ));
                }
            }
        }
        out.push((row, false));
    }
    out
}

/// [`frames_of`] for thread `tid`, another than the caller: blocked, read
/// through its window at once; suspended, once it has stopped; running,
/// after a handshake. `JVMTI_ERROR_THREAD_NOT_ALIVE` when it ended meanwhile,
/// `JVMTI_ERROR_NOT_AVAILABLE` when it did not stop in [`STOP_WAIT`].
///
/// Wave 46 (lane L1): a thread parked at a suspend point lists itself, on
/// its own thread, as the current thread's frames are listed
/// ([`listed_rows`]), so a JNI native in the middle of its stack (one that
/// called back into Java) is listed as HotSpot lists it; the listing its
/// park published for JDWP names only the native on top. That listing is
/// still the answer when the thread cannot take the work (a debugger
/// invocation runs on it).
fn other_thread_frames(shared: &SharedVm, tid: u64) -> Result<Vec<FrameInfo>, JInt> {
    let window = |held: &crate::runtime::interpreter::BlockedFrames<'_>| {
        other_rows(shared, held.rows.clone())
            .into_iter()
            .map(|row| row.info)
            .collect::<Vec<FrameInfo>>()
    };
    // An unmounted virtual thread (a continuation that yielded in a sleep or a
    // park) is in no blocking region and no carrier runs it, so a handshake
    // would wait for it in vain: its deposited frames are read where they lie
    // (interpreter round i1 wave 50, lane L1;
    // `interpreter-L1-the-c-jvmti-table-lists-no-frames-for-an-unmounted-virtual-thread-FIXED`).
    let unmounted = || {
        crate::runtime::interpreter::unmounted_virtual_thread_rows(shared, tid).map(|rows| {
            other_rows(shared, rows)
                .into_iter()
                .map(|row| row.info)
                .collect()
        })
    };
    let blocked = crate::runtime::interpreter::with_blocked_frames(shared, tid, &window);
    let (answer, via) = match blocked
        .map(|rows| (rows, "window"))
        .or_else(|| unmounted().map(|rows: Vec<FrameInfo>| (rows, "unmounted")))
    {
        Some((rows, via)) => (Ok(rows), via),
        None => {
            let handshake = !shared.debug.debug_state.lock().is_thread_suspended(tid);
            if handshake {
                let mut ds = shared.debug.debug_state.lock();
                ds.begin_handshake(tid);
                crate::debug::publish_debugger_gates(shared, &ds);
            }
            let answer = on_stopped_thread(
                shared,
                tid,
                |shared: &SharedVm, thread: &mut JvmThread| {
                    listed_rows(shared, thread, false)
                        .into_iter()
                        .map(|row| row.info)
                        .collect::<Vec<FrameInfo>>()
                },
                window,
            );
            if handshake {
                let mut ds = shared.debug.debug_state.lock();
                ds.end_handshake(tid);
                crate::debug::publish_debugger_gates(shared, &ds);
                if crate::runtime::env_cache::frame_trace() {
                    eprintln!(
                        "[JVMTI_SUSPEND] handshake tid={tid} stopped={}",
                        answer.is_ok()
                    );
                }
            }
            match answer {
                Ok((rows, via)) => {
                    let via = match (handshake, via) {
                        (true, _) => "handshake",
                        (false, "parked") => "suspended",
                        (false, via) => via,
                    };
                    (Ok(rows), via)
                }
                Err(ERR_NOT_AVAILABLE) => match stopped_rows(shared, tid) {
                    Some(rows) => (
                        Ok(other_rows(shared, rows)
                            .into_iter()
                            .map(|row| row.info)
                            .collect()),
                        "published",
                    ),
                    None => (Err(ERR_NOT_AVAILABLE), "none"),
                },
                Err(code) => (Err(code), "none"),
            }
        }
    };
    let out = answer?;
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[JVMTI_OTHER_STACK] tid={tid} rows={} via={via}", out.len());
    }
    Ok(out)
}

/// The thread a suspension function names: `(thread id, is it the calling
/// thread)`; a NULL `thread` is the calling thread.
fn suspension_target(shared: &SharedVm, thread: JObject) -> Result<(u64, bool), JInt> {
    let current_tid = jni::current_jvm_thread_of(shared)
        // SAFETY: the calling thread's own, live `JvmThread`; `thread_id` is
        // an immutable `Copy` field.
        .map(|current| unsafe { (*current).thread_id.0 });
    if thread == 0 {
        return current_tid
            .map(|tid| (tid, true))
            .ok_or(ERR_UNATTACHED_THREAD);
    }
    let tid = thread_id_of(shared, thread)?;
    Ok((tid, current_tid == Some(tid)))
}

/// Mark thread `tid` suspended by the agent and republish the debugger
/// gates, so it stops at its next suspend point.
/// `JVMTI_ERROR_THREAD_SUSPENDED` when it already is.
fn mark_suspended(shared: &SharedVm, tid: u64) -> JInt {
    let mut ds = shared.debug.debug_state.lock();
    if !ds.jvmti_suspend(tid) {
        return ERR_THREAD_SUSPENDED;
    }
    crate::debug::publish_debugger_gates(shared, &ds);
    ERR_NONE
}

/// Wait for thread `tid`, which [`mark_suspended`] suspended, to stop; the
/// calling thread itself waits until it is resumed, as JVMTI's
/// `SuspendThread` of the current thread does not return before that.
fn await_suspended(shared: &SharedVm, tid: u64, is_current: bool) {
    if is_current {
        wait_gc_safe(shared, None, || {
            !shared.debug.debug_state.lock().is_jvmti_suspended(tid)
        });
        return;
    }
    let deadline = std::time::Instant::now() + STOP_WAIT;
    let stopped = wait_gc_safe(shared, Some(deadline), || thread_stopped(shared, tid));
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[JVMTI_SUSPEND] suspend tid={tid} stopped={stopped}");
    }
}

/// End the agent's suspension of thread `tid`.
/// `JVMTI_ERROR_THREAD_NOT_SUSPENDED` when it has none.
fn mark_resumed(shared: &SharedVm, tid: u64) -> JInt {
    let mut ds = shared.debug.debug_state.lock();
    if !ds.jvmti_resume(tid) {
        return ERR_THREAD_NOT_SUSPENDED;
    }
    crate::debug::publish_debugger_gates(shared, &ds);
    ERR_NONE
}

/// Slot 5: `SuspendThread(env, thread)` (wave 45; `can_suspend`). Returns
/// once the thread has stopped (or after [`STOP_WAIT`], still suspended); the
/// calling thread returns only once resumed.
extern "C" fn suspend_thread(env: *mut JvmtiNativeEnv, thread: JObject) -> JInt {
    let shared = match live_phase_vm_with(env, 0, CAN_SUSPEND) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let (tid, is_current) = match suspension_target(&shared, thread) {
        Ok(target) => target,
        Err(code) => return code,
    };
    let code = mark_suspended(&shared, tid);
    if code == ERR_NONE {
        await_suspended(&shared, tid, is_current);
    }
    code
}

/// Slot 6: `ResumeThread(env, thread)` (wave 45; `can_suspend`).
extern "C" fn resume_thread(env: *mut JvmtiNativeEnv, thread: JObject) -> JInt {
    let shared = match live_phase_vm_with(env, 0, CAN_SUSPEND) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    match suspension_target(&shared, thread) {
        Ok((tid, _)) => mark_resumed(&shared, tid),
        Err(code) => code,
    }
}

/// The checks `SuspendThreadList` and `ResumeThreadList` make before any
/// thread (wave 45): the capability, a negative count
/// (`ILLEGAL_ARGUMENT`), a NULL list or results array (`NULL_POINTER`).
/// Answers the VM and the list's handles.
fn thread_list(
    env: *mut JvmtiNativeEnv,
    request_count: JInt,
    request_list: *const JObject,
    results: *mut JInt,
) -> Result<(Arc<SharedVm>, Vec<JObject>), JInt> {
    let shared = live_phase_vm_with(env, 0, CAN_SUSPEND)?;
    let count = usize::try_from(request_count).map_err(|_| ERR_ILLEGAL_ARGUMENT)?;
    if request_list.is_null() || results.is_null() {
        return Err(ERR_NULL_POINTER);
    }
    // SAFETY: the agent's list holds `request_count` handles (the function's
    // contract), and it is non-null.
    let handles = unsafe { std::slice::from_raw_parts(request_list, count) }.to_vec();
    Ok((shared, handles))
}

/// Slot 92: `SuspendThreadList(env, request_count, request_list, results)`
/// (wave 45; `can_suspend`): each thread's own answer in `results` (a thread
/// listed twice is `THREAD_SUSPENDED` the second time), `JVMTI_ERROR_NONE`
/// overall. Every other thread is marked first, then waited for; the calling
/// thread, if listed, waits last, until it is resumed.
extern "C" fn suspend_thread_list(
    env: *mut JvmtiNativeEnv,
    request_count: JInt,
    request_list: *const JObject,
    results: *mut JInt,
) -> JInt {
    let (shared, handles) = match thread_list(env, request_count, request_list, results) {
        Ok(list) => list,
        Err(code) => return code,
    };
    let mut marked = Vec::new();
    let mut current_marked = None;
    for (at, &handle) in handles.iter().enumerate() {
        let code = match suspension_target(&shared, handle) {
            Ok((tid, is_current)) => {
                let code = mark_suspended(&shared, tid);
                if code == ERR_NONE {
                    if is_current {
                        current_marked = Some(tid);
                    } else {
                        marked.push(tid);
                    }
                }
                code
            }
            Err(code) => code,
        };
        // SAFETY: the agent's results array has `request_count` entries.
        unsafe { *results.add(at) = code };
    }
    for tid in marked {
        await_suspended(&shared, tid, false);
    }
    if let Some(tid) = current_marked {
        await_suspended(&shared, tid, true);
    }
    ERR_NONE
}

/// Slot 93: `ResumeThreadList(env, request_count, request_list, results)`
/// (wave 45; `can_suspend`): each thread's own answer in `results`.
extern "C" fn resume_thread_list(
    env: *mut JvmtiNativeEnv,
    request_count: JInt,
    request_list: *const JObject,
    results: *mut JInt,
) -> JInt {
    let (shared, handles) = match thread_list(env, request_count, request_list, results) {
        Ok(list) => list,
        Err(code) => return code,
    };
    for (at, &handle) in handles.iter().enumerate() {
        let code = match suspension_target(&shared, handle) {
            Ok((tid, _)) => mark_resumed(&shared, tid),
            Err(code) => code,
        };
        // SAFETY: the agent's results array has `request_count` entries.
        unsafe { *results.add(at) = code };
    }
    ERR_NONE
}

/// Slot 20: `NotifyFramePop(env, thread, depth)` (wave 45;
/// `can_generate_frame_pop_events`): a `FramePop` event when the frame at
/// `depth` of the listing ([`listed_rows`]) is popped, by a return or an
/// exception, posted by the interpreter as it pops it
/// (`jvmti_events::fire_jvmti_frame_pop_if_requested`, from
/// `JvmThread::frame_pop_requests`). Codes as HotSpot 25.0.3's (measured,
/// `tools/probes/interp/L1/L1W45JvmtiNotifyFramePop.java`): another thread
/// not suspended is `THREAD_NOT_SUSPENDED`, a negative depth
/// `ILLEGAL_ARGUMENT`, a depth past the bottom `NO_MORE_FRAMES`, a native
/// method's frame `OPAQUE_FRAME`, a second request for one frame `DUPLICATE`.
/// A compiled activation, and an interpreter frame whose body runs compiled,
/// are `OPAQUE_FRAME` here (HotSpot deoptimizes them). A suspended thread
/// takes the request itself while parked at a suspend point
/// (`debug::run_on_parked_thread`); one suspended while blocked in a native
/// region is `JVMTI_ERROR_NOT_AVAILABLE`, or, with
/// `DebuggerGates::c_frame_control` (wave 61), takes it at its wake
/// ([`frame_pop_through_window`]).
extern "C" fn notify_frame_pop(env: *mut JvmtiNativeEnv, thread: JObject, depth: JInt) -> JInt {
    let shared = match live_phase_vm_with(env, 0, CAN_GENERATE_FRAME_POP_EVENTS) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let (tid, is_current) = match suspension_target(&shared, thread) {
        Ok(target) => target,
        Err(code) => return code,
    };
    if !is_current && !shared.debug.debug_state.lock().is_thread_suspended(tid) {
        return ERR_THREAD_NOT_SUSPENDED;
    }
    if depth < 0 {
        return ERR_ILLEGAL_ARGUMENT;
    }
    if is_current {
        let Some(current) = jni::current_jvm_thread_of(&shared) else {
            return ERR_UNATTACHED_THREAD;
        };
        // SAFETY: the calling thread's own, live `JvmThread`, which is inside
        // this call and does not change its frames meanwhile.
        return request_frame_pop(&shared, unsafe { &mut *current }, depth);
    }
    // Interpreter round i2 wave 61 (lane L1): a thread suspended while blocked
    // in a native takes the request at its wake ([`frame_pop_through_window`]).
    if shared.debug.debugger_gates.c_frame_control() {
        let window = |held: &crate::runtime::interpreter::BlockedFrames<'_>| {
            frame_pop_through_window(&shared, held, depth)
        };
        let asked = on_stopped_thread(
            &shared,
            tid,
            move |shared: &SharedVm, thread: &mut JvmThread| {
                request_frame_pop(shared, thread, depth)
            },
            window,
        );
        return match asked {
            Ok((code, _)) => code,
            Err(code) => code,
        };
    }
    let asked = gc_safe(&shared, || {
        crate::debug::run_on_parked_thread(&shared, Some(tid), move |shared, thread| {
            request_frame_pop(shared, thread, depth)
        })
    });
    asked.unwrap_or(ERR_NOT_AVAILABLE)
}

/// [`notify_frame_pop`] on `thread`, the thread that runs this.
fn request_frame_pop(shared: &SharedVm, thread: &mut JvmThread, depth: JInt) -> JInt {
    let Ok(depth) = usize::try_from(depth) else {
        return ERR_ILLEGAL_ARGUMENT;
    };
    let rows = listed_rows(shared, thread, true);
    let Some(row) = rows.get(depth) else {
        return ERR_NO_MORE_FRAMES;
    };
    let Some(index) = row.frame.and_then(|frame| u32::try_from(frame).ok()) else {
        return ERR_OPAQUE_FRAME;
    };
    if thread.frame_pop_requests.contains(&index) {
        return ERR_DUPLICATE;
    }
    thread.frame_pop_requests.push(index);
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[JVMTI_FRAME_POP] requested tid={} depth={depth} frame={index}",
            thread.thread_id.0
        );
    }
    ERR_NONE
}

// ---------------------------------------------------------------------------
// Frame control (interpreter round i2 wave 61, lane L1)
// ---------------------------------------------------------------------------
//
// `PopFrame` (80), `ForceEarlyReturn*` (81-86), `ClearAllFramePops` (67) and
// `NotifyFramePop` of a thread suspended while blocked in a native, behind
// `DebuggerGates::c_frame_control` (`CRATONVM_JVMTI_C_FRAME_CONTROL`; off,
// every slot answers `JVMTI_ERROR_NOT_AVAILABLE` as before and the two
// capabilities are not potential). Proposal:
// `docs/feature-designs/jvmti-delivery-threading.md`.
// MEASURED on HotSpot 25.0.3 (`tools/probes/interp/L1/L1W61JvmtiFrameControl.java`).
//
// A suspended thread parked at an interpreter suspend point takes the
// request itself (`debug::run_on_parked_thread`): the forced return or the
// pop is recorded there and taken when the thread resumes, as HotSpot takes
// both (the JDWP machinery of waves 43-44, `debug::early_return`,
// `debug::pop_frames`). The calling thread's own top frame is the native
// that calls the function, `OPAQUE_FRAME` on HotSpot too. A request from a
// `Breakpoint` or `SingleStep` callback on the calling thread (whose top
// frame is the event's Java frame on HotSpot) is served since wave 62 with
// `DebuggerGates::c_callback_frame_control` ([`FrameTarget::Current`]); from
// a `MethodEntry` callback since the wave-129 flip of
// `CRATONVM_JVMTI_METHOD_ENTRY_FRAME_CONTROL` (default ON; `=0` answers
// `OPAQUE_FRAME` there again; stage 2 of
// `docs/feature-designs/jdk-only-mode.md`).
// A thread suspended while blocked on a monitor entry is served since
// interpreter round i6 wave 134, lane L1, through its inspection window
// (`CRATONVM_JVMTI_BLOCKED_ENTRY_FRAME_CONTROL`, default ON, `=0` answers
// `OPAQUE_FRAME` again; [`frame_op_on_parked`], [`blocked_entry_frame`];
// `docs/feature-designs/jvmti-delivery-threading.md`).

/// The VM of a frame-control call: `JVMTI_ERROR_NOT_AVAILABLE` while its
/// `DebuggerGates::c_frame_control` is off, then the live phase and the
/// capability `bit` of word `word`.
fn frame_control_vm(
    env: *mut JvmtiNativeEnv,
    word: usize,
    bit: u32,
) -> Result<Arc<SharedVm>, JInt> {
    let shared = live_phase_vm(env)?;
    if !shared.debug.debugger_gates.c_frame_control() {
        return Err(ERR_NOT_AVAILABLE);
    }
    live_phase_vm_with(env, word, bit)
}

/// Run `work` on suspended thread `tid` while it is parked
/// (`debug::run_on_parked_thread`); its refusal is a JDWP error number, the
/// JVMTI one for these codes. A thread suspended but not parked (blocked in
/// a native, or not yet at a suspend point) is `OPAQUE_FRAME`: HotSpot
/// answers it for a native top frame and for a thread in `Object.wait`.
///
/// Interpreter round i6 wave 134, lane L1
/// (`CRATONVM_JVMTI_BLOCKED_ENTRY_FRAME_CONTROL`, default ON, `=0` off;
/// `docs/feature-designs/jvmti-delivery-threading.md`):
/// a suspended thread blocked entering a monitor, whose top frame is the
/// Java frame at the entry on HotSpot, gets `window` through its inspection
/// window instead ([`blocked_entry_frame`]); a window that is not open (or
/// another reader's) keeps the `OPAQUE_FRAME` answer.
fn frame_op_on_parked<F, W>(shared: &SharedVm, tid: u64, work: F, window: W) -> JInt
where
    F: FnOnce(&SharedVm, &mut JvmThread) -> Result<(), u16> + Send + 'static,
    W: FnOnce(&crate::runtime::interpreter::BlockedFrames<'_>) -> JInt,
{
    let ran = gc_safe(shared, || {
        crate::debug::run_on_parked_thread(shared, Some(tid), work)
    });
    match ran {
        Ok(Ok(())) => ERR_NONE,
        Ok(Err(code)) => JInt::from(code),
        Err(crate::debug::ParkedError::NotParked) => {
            if !shared.debug.debug_state.lock().is_thread_suspended(tid) {
                return ERR_THREAD_NOT_SUSPENDED;
            }
            if blocked_entry_frame_control_on(shared) {
                if let Some(code) =
                    crate::runtime::interpreter::with_blocked_frames(shared, tid, window)
                {
                    return code;
                }
            }
            ERR_OPAQUE_FRAME
        }
        Err(crate::debug::ParkedError::Busy) => ERR_THREAD_NOT_SUSPENDED,
        Err(crate::debug::ParkedError::Lost) => ERR_INTERNAL,
    }
}

/// The thread a frame-control function acts on ([`frame_control_target`]).
#[derive(Clone, Copy)]
enum FrameTarget {
    /// Another thread, suspended: the request runs on it while it is parked
    /// ([`frame_op_on_parked`]).
    Suspended(u64),
    /// The calling thread, inside a `Breakpoint` or `SingleStep` callback
    /// whose frame it may act on (interpreter round i2 wave 62, lane L1;
    /// `DebuggerGates::c_callback_frame_control`): the request is checked
    /// against the callback point the interpreter entered
    /// ([`frame_op_on_current`]).
    Current(u64),
}

/// The thread a frame-control function names, after HotSpot's checks
/// (`check_non_suspended_or_opaque_frame`): another thread not suspended,
/// `THREAD_NOT_SUSPENDED`; the calling thread, whose top frame is the native
/// calling this, `OPAQUE_FRAME` -- unless (wave 62, with
/// `DebuggerGates::c_callback_frame_control`) it calls from an event
/// callback, where its top frame is the event's Java frame, as on HotSpot;
/// that is decided against the callback point ([`frame_op_on_current`]).
fn frame_control_target(shared: &SharedVm, thread: JObject) -> Result<FrameTarget, JInt> {
    let (tid, is_current) = suspension_target(shared, thread)?;
    if is_current {
        if shared.debug.debugger_gates.c_callback_frame_control() {
            return Ok(FrameTarget::Current(tid));
        }
        return Err(ERR_OPAQUE_FRAME);
    }
    if !shared.debug.debug_state.lock().is_thread_suspended(tid) {
        return Err(ERR_THREAD_NOT_SUSPENDED);
    }
    Ok(FrameTarget::Suspended(tid))
}

/// Run `work` on the calling thread's own `JvmThread`, inside an event
/// callback (wave 62, [`FrameTarget::Current`]); its refusal is a JDWP error
/// number, the JVMTI one for these codes. Outside a callback point the
/// checks in `work` answer `OPAQUE_FRAME`, wave 61's answer for the calling
/// thread.
fn frame_op_on_current<F>(shared: &SharedVm, work: F) -> JInt
where
    F: FnOnce(&SharedVm, &JvmThread) -> Result<(), u16>,
{
    let Some(current) = jni::current_jvm_thread_of(shared) else {
        return ERR_UNATTACHED_THREAD;
    };
    // SAFETY: the calling thread's own, live `JvmThread`, read inside this
    // call (as `clear_all_frame_pops` reaches it); the interpreter that runs
    // the callback does not touch it until the callback returns.
    match work(shared, unsafe { &*current }) {
        Ok(()) => ERR_NONE,
        Err(code) => JInt::from(code),
    }
}

/// Slot 80: `PopFrame(env, thread)` (wave 61; `can_pop_frame`): the top
/// frame of a suspended thread is popped when it resumes and the invoke that
/// called it runs again (`debug::pop_frames::request_jvmti_pop`). HotSpot's
/// answers (MEASURED): a NULL `thread` is `INVALID_THREAD` (not the calling
/// thread, unlike the other thread functions), a second `PopFrame` before
/// the resume `INTERNAL`, a thread whose only frame is the top one
/// `NO_MORE_FRAMES`, a native top frame `OPAQUE_FRAME`; the thread lists the
/// popped frame until it resumes; a `NotifyFramePop` of the popped frame
/// posts nothing.
extern "C" fn pop_frame(env: *mut JvmtiNativeEnv, thread: JObject) -> JInt {
    let shared = match frame_control_vm(env, 0, CAN_POP_FRAME) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    if thread == 0 {
        return ERR_INVALID_THREAD;
    }
    match frame_control_target(&shared, thread) {
        Ok(FrameTarget::Suspended(tid)) => frame_op_on_parked(
            &shared,
            tid,
            move |shared, thread| crate::debug::pop_frames::request_jvmti_pop(shared, thread, tid),
            // Wave 134 (lane L1): blocked entering a monitor.
            |held| pop_through_window(&shared, held),
        ),
        // Wave 62: from the calling thread's own event callback.
        Ok(FrameTarget::Current(tid)) => frame_op_on_current(&shared, |shared, thread| {
            crate::debug::pop_frames::request_jvmti_pop_in_callback(shared, thread, tid)
        }),
        Err(code) => code,
    }
}

/// A `ForceEarlyReturn*` value as the agent passed it.
#[derive(Clone, Copy)]
enum EarlyValue {
    Void,
    Plain(crate::types::Value),
    Object(JObject),
}

/// Slots 81-86, `ForceEarlyReturn{Object,Int,Long,Float,Double,Void}` (wave
/// 61; `can_force_early_return`): the top method of a suspended thread
/// returns `value` when the thread resumes, without running on
/// (`debug::early_return::record_jvmti`, taken where a JDWP
/// `ThreadReference.ForceEarlyReturn` is). `kind` is the family (`I`, `J`,
/// `F`, `D`, `L`, `V`). HotSpot's answers (MEASURED): a value of another kind
/// than the return type, or an object of another class, `TYPE_MISMATCH`
/// (`Int` serves `boolean`, `byte`, `char`, `short` and `int`); a second
/// request before the resume `INTERNAL`; a native top frame `OPAQUE_FRAME`.
fn force_early_return(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    kind: u8,
    value: EarlyValue,
) -> JInt {
    use crate::debug::early_return::JvmtiForced;
    let shared = match frame_control_vm(env, 1, CAN_FORCE_EARLY_RETURN) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let target = match frame_control_target(&shared, thread) {
        Ok(target) => target,
        Err(code) => return code,
    };
    let forced = match value {
        EarlyValue::Void => JvmtiForced::Void,
        EarlyValue::Plain(v) => JvmtiForced::Plain(v),
        EarlyValue::Object(0) => JvmtiForced::Plain(crate::types::Value::Object(None)),
        EarlyValue::Object(handle) => match local_to_global(&shared, handle) {
            Ok(global) => JvmtiForced::Global(global),
            Err(code) => return code,
        },
    };
    let code = match target {
        FrameTarget::Suspended(tid) => frame_op_on_parked(
            &shared,
            tid,
            move |shared, thread| {
                crate::debug::early_return::record_jvmti(shared, thread, tid, kind, forced)
            },
            // Wave 134 (lane L1): blocked entering a monitor.
            |held| forced_return_through_window(&shared, held, kind, forced),
        ),
        // Wave 62: from the calling thread's own event callback, recorded
        // at its callback point and taken as the callback returns.
        FrameTarget::Current(tid) => frame_op_on_current(&shared, |shared, thread| {
            crate::debug::early_return::record_jvmti(shared, thread, tid, kind, forced)
        }),
    };
    if code != ERR_NONE {
        if let JvmtiForced::Global(global) = forced {
            release_global(&shared, global);
        }
    }
    code
}

extern "C" fn force_early_return_object(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    value: JObject,
) -> JInt {
    force_early_return(env, thread, b'L', EarlyValue::Object(value))
}

extern "C" fn force_early_return_int(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    value: JInt,
) -> JInt {
    force_early_return(
        env,
        thread,
        b'I',
        EarlyValue::Plain(crate::types::Value::Int(value)),
    )
}

extern "C" fn force_early_return_long(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    value: i64,
) -> JInt {
    force_early_return(
        env,
        thread,
        b'J',
        EarlyValue::Plain(crate::types::Value::Long(value)),
    )
}

extern "C" fn force_early_return_float(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    value: f32,
) -> JInt {
    force_early_return(
        env,
        thread,
        b'F',
        EarlyValue::Plain(crate::types::Value::Float(value)),
    )
}

extern "C" fn force_early_return_double(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    value: f64,
) -> JInt {
    force_early_return(
        env,
        thread,
        b'D',
        EarlyValue::Plain(crate::types::Value::Double(value)),
    )
}

extern "C" fn force_early_return_void(env: *mut JvmtiNativeEnv, thread: JObject) -> JInt {
    force_early_return(env, thread, b'V', EarlyValue::Void)
}

/// Slot 67: `ClearAllFramePops(env, thread)` (JDK 25's slot 67; wave 61,
/// `can_generate_frame_pop_events`): drop every `NotifyFramePop` request of
/// the calling thread or of a suspended one, which posts none of their
/// `FramePop`s. MEASURED: another thread not suspended is
/// `THREAD_NOT_SUSPENDED`; the calling thread, and a second call, `NONE`. A
/// thread suspended while blocked in a native drops them at its wake.
extern "C" fn clear_all_frame_pops(env: *mut JvmtiNativeEnv, thread: JObject) -> JInt {
    let shared = match frame_control_vm(env, 0, CAN_GENERATE_FRAME_POP_EVENTS) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let (tid, is_current) = match suspension_target(&shared, thread) {
        Ok(target) => target,
        Err(code) => return code,
    };
    if is_current {
        let Some(current) = jni::current_jvm_thread_of(&shared) else {
            return ERR_UNATTACHED_THREAD;
        };
        // SAFETY: the calling thread's own, live `JvmThread`, inside this call.
        unsafe { &mut *current }.frame_pop_requests.clear();
        return ERR_NONE;
    }
    if !shared.debug.debug_state.lock().is_thread_suspended(tid) {
        return ERR_THREAD_NOT_SUSPENDED;
    }
    let window = |held: &crate::runtime::interpreter::BlockedFrames<'_>| {
        crate::debug::inspect::defer_jvmti_clear_frame_pops(&shared, held.tid());
        ERR_NONE
    };
    let cleared = on_stopped_thread(
        &shared,
        tid,
        |_: &SharedVm, thread: &mut JvmThread| {
            thread.frame_pop_requests.clear();
            ERR_NONE
        },
        window,
    );
    match cleared {
        Ok((code, _)) => code,
        Err(code) => code,
    }
}

/// [`notify_frame_pop`] of a thread suspended while blocked in a native
/// (wave 61), through its inspection window: the frame at `depth` of the
/// listing the window publishes, recorded for the thread's wake
/// (`debug::inspect::defer_jvmti_frame_pop`). The listing's native top row
/// and a compiled frame are `OPAQUE_FRAME`, as for the parked thread; so is
/// an unmounted virtual thread's frame (its continuation is not the thread's
/// stack).
fn frame_pop_through_window(
    shared: &SharedVm,
    held: &crate::runtime::interpreter::BlockedFrames<'_>,
    depth: JInt,
) -> JInt {
    let Ok(depth) = usize::try_from(depth) else {
        return ERR_ILLEGAL_ARGUMENT;
    };
    let rows = other_rows(shared, held.rows.clone());
    let Some(row) = rows.get(depth) else {
        return ERR_NO_MORE_FRAMES;
    };
    if held.unmounted() {
        return ERR_OPAQUE_FRAME;
    }
    let index = match local_frame_index(row) {
        Ok(index) => index,
        Err(code) => return code,
    };
    let Some(frame) = held.frames().get(index) else {
        return ERR_OPAQUE_FRAME;
    };
    if !row_names_frame(&row.raw, frame) {
        return ERR_NOT_AVAILABLE;
    }
    let recorded = crate::debug::inspect::defer_jvmti_frame_pop(
        shared,
        held.tid(),
        index,
        held.frames().len(),
        row.raw.class_id,
        row.raw.method_id,
    );
    if recorded {
        ERR_NONE
    } else {
        ERR_DUPLICATE
    }
}

/// Is `CRATONVM_JVMTI_BLOCKED_ENTRY_FRAME_CONTROL` in force for this VM
/// (interpreter round i6 wave 134, lane L1)? It needs the dispatch loop's
/// reader of the agent-request word, `CRATONVM_JVMTI_METHOD_ENTRY_FRAME_CONTROL`.
fn blocked_entry_frame_control_on(shared: &SharedVm) -> bool {
    shared.config.jvmti_blocked_entry_frame_control
        && shared.config.jvmti_method_entry_frame_control
}

/// The JVMTI error number `code` as the JDWP-numbered refusals of
/// `debug::early_return` / `debug::pop_frames` carry it (the same numbers
/// for these codes).
fn jdwp_numbered(code: JInt) -> u16 {
    u16::try_from(code).unwrap_or(113) // 113: JVMTI_ERROR_INTERNAL
}

/// The two ways a thread blocked entering a monitor stands at its top frame
/// (interpreter round i6 wave 134, lane L1): at a `monitorenter` (its
/// `last_instr_pc`), or at bytecode 0 of a `synchronized` method whose
/// frame was pushed before its contended monitor (`frame_first_entry`: the
/// method is synchronized and the frame owns no monitor yet,
/// `vm_exec::SyncEntry::Deferred`).
fn blocked_at_monitor_entry(code_at_last: Option<u8>, frame_first_entry: bool) -> bool {
    const MONITORENTER: u8 = 0xc2;
    code_at_last == Some(MONITORENTER) || frame_first_entry
}

/// Can a `PopFrame` / `ForceEarlyReturn*` of the top frame of the thread
/// whose frames `held` holds be recorded for its wake (interpreter round i6
/// wave 134, lane L1)? The thread is BLOCKED entering a monitor (block state
/// 2), mounted, and its listing's top row is that interpreter frame, run
/// interpreted, standing at the monitor entry ([`blocked_at_monitor_entry`]):
/// HotSpot's top frame then is that Java frame. Anything else --
/// `Object.wait`, a sleep or a native whose row heads the listing, a
/// compiled activation, a block of another kind -- is `OPAQUE_FRAME`, the
/// answer before this wave. `with_caller`: the `PopFrame` checks of the
/// caller's row too (none `NO_MORE_FRAMES`; a native, compiled or spliced row
/// `OPAQUE_FRAME`, as HotSpot's two-Java-frames test answers it).
fn blocked_entry_frame(
    shared: &SharedVm,
    held: &crate::runtime::interpreter::BlockedFrames<'_>,
    with_caller: bool,
) -> Result<(), JInt> {
    if held.unmounted()
        || shared
            .threads
            .thread_registry
            .java_block_state(ThreadId(held.tid()))
            != 2
    {
        return Err(ERR_OPAQUE_FRAME);
    }
    let rows = other_rows(shared, held.rows.clone());
    let top_row = rows.first().ok_or(ERR_OPAQUE_FRAME)?;
    let index = local_frame_index(top_row)?;
    let frames = held.frames();
    if index + 1 != frames.len() {
        return Err(ERR_OPAQUE_FRAME);
    }
    let frame = &frames[index];
    if !row_names_frame(&top_row.raw, frame) || frame.runs_obsolete_method() {
        return Err(ERR_OPAQUE_FRAME);
    }
    let frame_first_entry = frame.monitor_on_exit.is_none()
        && frame.pc == 0
        && with_method(shared, top_row.info.method, |_, m| m.is_synchronized()).unwrap_or(false);
    if !blocked_at_monitor_entry(frame.code.get(frame.last_instr_pc).copied(), frame_first_entry) {
        return Err(ERR_OPAQUE_FRAME);
    }
    if with_caller {
        let caller_row = rows.get(1).ok_or(ERR_NO_MORE_FRAMES)?;
        let caller = local_frame_index(caller_row)?;
        if caller + 1 != index {
            return Err(ERR_OPAQUE_FRAME);
        }
    }
    Ok(())
}

/// [`pop_frame`] of a suspended thread blocked entering a monitor, through
/// its inspection window (interpreter round i6 wave 134, lane L1): checked
/// here ([`blocked_entry_frame`], `debug::pop_frames::checked_blocked_pop`)
/// and recorded for its wake (`debug::early_return::defer_blocked_pop`),
/// where it becomes the record the thread's next bytecode takes. A second
/// `PopFrame` before the resume is `INTERNAL`, as for a parked thread.
fn pop_through_window(
    shared: &SharedVm,
    held: &crate::runtime::interpreter::BlockedFrames<'_>,
) -> JInt {
    let frames = held.frames();
    let checked = match blocked_entry_frame(shared, held, true) {
        Ok(()) => crate::debug::pop_frames::checked_blocked_pop(
            shared,
            frames,
            held.no_continuation_adapters(),
        ),
        Err(code) => Err(jdwp_numbered(code)),
    };
    match crate::debug::early_return::defer_blocked_pop(shared, held.tid(), frames, checked) {
        Ok(()) => ERR_NONE,
        Err(code) => JInt::from(code),
    }
}

/// [`force_early_return`] of a suspended thread blocked entering a monitor,
/// through its inspection window (interpreter round i6 wave 134, lane L1):
/// the top frame judged here ([`blocked_entry_frame`]), the value and the
/// pending check in `debug::early_return::defer_blocked_forced_return`,
/// recorded for the thread's wake. An object value is resolved inside the
/// foreign-thread entry, so no collection moves it while its class is read.
fn forced_return_through_window(
    shared: &SharedVm,
    held: &crate::runtime::interpreter::BlockedFrames<'_>,
    kind: u8,
    forced: crate::debug::early_return::JvmtiForced,
) -> JInt {
    let top_servable = blocked_entry_frame(shared, held, false).is_ok();
    let _fx = jni::ForeignJniEntry::enter();
    match crate::debug::early_return::defer_blocked_forced_return(
        shared,
        held.tid(),
        held.frames(),
        top_servable,
        kind,
        forced,
    ) {
        Ok(()) => ERR_NONE,
        Err(code) => JInt::from(code),
    }
}

/// Slot 17: `GetThreadState(env, thread, thread_state_ptr)` (wave 45; no
/// capability). A NULL `thread` is the calling thread. The answer carries
/// `java.lang.Thread.State`'s bits (from the thread's blocking kind,
/// `ThreadRegistry::java_block_state`: `RUNNABLE`, `BLOCKED_ON_MONITOR_ENTER`,
/// `WAITING` with `INDEFINITELY` or `WITH_TIMEOUT`), `SUSPENDED` (a JDWP or a
/// JVMTI suspension, `DebugState::is_thread_suspended`) and `INTERRUPTED`;
/// a waiting thread's kind, `SLEEPING`, `IN_OBJECT_WAIT` or `PARKED`
/// (wave 57, [`wait_kind`]); not yet `IN_NATIVE`.
/// A terminated thread is `TERMINATED`, one never started 0.
extern "C" fn get_thread_state(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    state_ptr: *mut JInt,
) -> JInt {
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    if state_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let lookup = if thread == 0 {
        match suspension_target(&shared, 0) {
            Ok((tid, _)) => ThreadLookup::Live(tid),
            Err(code) => return code,
        }
    } else {
        match lookup_thread(&shared, thread) {
            Ok(lookup) => lookup,
            Err(code) => return code,
        }
    };
    let state = match lookup {
        ThreadLookup::Unstarted => 0,
        ThreadLookup::Dead => THREAD_STATE_TERMINATED,
        ThreadLookup::Live(tid) => live_thread_state(&shared, tid),
    };
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *state_ptr = state };
    ERR_NONE
}

/// [`get_thread_state`]'s answer for live thread `tid`.
fn live_thread_state(shared: &SharedVm, tid: u64) -> JInt {
    let registry = &shared.threads.thread_registry;
    let id = ThreadId(tid);
    // Interpreter round i6 wave 132, lane L1 (i130-L1b's first next step,
    // `CRATONVM_JVMTI_THREAD_STATE_ONE_READ`, performance, default ON since i7, `=0` off): the
    // block state, the published wait kind and the interrupted flag from one
    // registry read, each the answer of its own read below.
    // Interpreter round i7 wave 135, lane L1 (`CRATONVM_JVMTI_STATE_SUSPEND_WORD`,
    // performance, default off): the thread's own suspended word, read in the
    // same registry read as the facts when there is one.
    let word_on = shared.debug.debugger_gates.jvmti_state_suspend_word();
    let mut suspend_word = None;
    let facts = shared
        .debug
        .debugger_gates
        .jvmti_thread_state_one_read()
        .then(|| {
            if word_on {
                let (facts, word) = registry.jvmti_state_facts_and_suspend_word(id);
                suspend_word = Some(word);
                facts
            } else {
                registry.jvmti_state_facts(id)
            }
        });
    let block = facts.map_or_else(|| registry.java_block_state(id), |(block, _, _)| block);
    let kind = || match facts {
        Some((_, Some(kind), _)) => published_wait_kind_bit(kind),
        _ => wait_kind(shared, id),
    };
    let mut state = THREAD_STATE_ALIVE
        | match block {
            1 => THREAD_STATE_WAITING | THREAD_STATE_WAITING_INDEFINITELY | kind(),
            2 => THREAD_STATE_BLOCKED_ON_MONITOR_ENTER,
            3 => THREAD_STATE_WAITING | THREAD_STATE_WAITING_WITH_TIMEOUT | kind(),
            _ => THREAD_STATE_RUNNABLE,
        };
    // Wave 58 (lane L1): a RUNNABLE thread in native code
    // ([`thread_in_native`]; only with `CRATONVM_JVMTI_C_IN_NATIVE`).
    if state & THREAD_STATE_RUNNABLE != 0
        && shared.debug.debugger_gates.c_in_native_armed()
        && thread_in_native(shared, tid)
    {
        state |= THREAD_STATE_IN_NATIVE;
    }
    let interrupted = facts.map_or_else(
        || {
            registry
                .get_interrupted_flag(id)
                .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire))
        },
        |(_, _, interrupted)| interrupted,
    );
    if interrupted {
        state |= THREAD_STATE_INTERRUPTED;
    }
    // Interpreter round i3 wave 72, lane L6
    // (`CRATONVM_JVMTI_STATE_SUSPENSION_HINT`, default off): no thread is
    // suspended -- the hint every suspension mutator refreshes under the
    // debug-state lock -- so no `SUSPENDED` bit, without that lock (a sampler
    // asking many threads no longer serialises on the debugger's lock).
    let surely_not_suspended = shared.debug.debugger_gates.state_suspension_hint()
        && !shared
            .debug
            .suspension_hint
            .load(std::sync::atomic::Ordering::Acquire);
    // Wave 135 (lane L1): the per-thread word answers without the lock once
    // published; an unpublished one subscribes the thread under the lock.
    let suspended = if surely_not_suspended {
        false
    } else if word_on {
        let word = suspend_word.unwrap_or_else(|| registry.jvmti_state_suspend_word(id));
        state_suspend_word_answer(word)
            .unwrap_or_else(|| subscribe_state_suspend_word(shared, id))
    } else {
        shared.debug.debug_state.lock().is_thread_suspended(tid)
    };
    if suspended {
        state |= THREAD_STATE_SUSPENDED;
    }
    state
}

/// What a published C JVMTI state suspend word answers (interpreter round i7
/// wave 135, lane L1; `GcBlockState::jvmti_state_suspended`): `None` while it
/// is unpublished, and the reader asks the lock.
fn state_suspend_word_answer(word: u8) -> Option<bool> {
    use crate::threading::jvm_thread::{JVMTI_STATE_SUSPEND_NO, JVMTI_STATE_SUSPEND_YES};
    match word {
        JVMTI_STATE_SUSPEND_YES => Some(true),
        JVMTI_STATE_SUSPEND_NO => Some(false),
        _ => None,
    }
}

/// The first `GetThreadState` of thread `id` under
/// `CRATONVM_JVMTI_STATE_SUSPEND_WORD` (wave 135, lane L1): its block state is
/// fetched from the registry (no lock held), then, under the debug-state lock,
/// the thread is subscribed (its word written now and by every later
/// suspension mutator) and the lock's answer returned. A thread gone from the
/// registry is answered from the lock alone.
#[cold]
#[inline(never)]
fn subscribe_state_suspend_word(shared: &SharedVm, id: ThreadId) -> bool {
    let block_state = shared.threads.thread_registry.gc_block_state_of(id);
    let mut ds = shared.debug.debug_state.lock();
    match block_state {
        Some(state) => ds.subscribe_state_suspend_word(id.0, state),
        None => ds.is_thread_suspended(id.0),
    }
}

/// The [`live_thread_state`] kind bit of a published wait kind
/// ([`wait_kind`]'s first answer; shared with the one-read facts of wave 132).
fn published_wait_kind_bit(kind: crate::threading::thread_registry::PublishedWaitKind) -> JInt {
    use crate::threading::thread_registry::PublishedWaitKind;
    match kind {
        PublishedWaitKind::Sleeping => THREAD_STATE_SLEEPING,
        PublishedWaitKind::ObjectWait => THREAD_STATE_IN_OBJECT_WAIT,
        PublishedWaitKind::Parked | PublishedWaitKind::Unrecorded => THREAD_STATE_PARKED,
    }
}

/// The kind bit of a WAITING / TIMED_WAITING thread's [`live_thread_state`]
/// (interpreter round i2 wave 57, lane L1; HotSpot 25.0.3 MEASURED,
/// `tools/probes/interp/L1/L1W57JvmtiWaitStates.java`): `IN_OBJECT_WAIT` in
/// `Object.wait` (also `Thread.join`, which waits on the thread), the
/// thread's published waiting monitor; `SLEEPING` in `Thread.sleep`, read
/// from the deposited top frame as JDWP's `ThreadReference.Status` reads it
/// (`debug::is_thread_sleep_frame`, `debug::frame_invokes_thread_sleep`: the
/// registered `Thread.sleep` native deposits no `Thread` frame); `PARKED`
/// otherwise (`LockSupport.park*`, and every other blocking native that
/// stands in for JDK code that parks).
#[cold]
#[inline(never)]
fn wait_kind(shared: &SharedVm, id: ThreadId) -> JInt {
    let registry = &shared.threads.thread_registry;
    // Interpreter round i2 wave 64, lane L1 (`CRATONVM_PUBLISHED_ALL_WAIT_KINDS`):
    // every wait published its kind (a park, `Object.wait`, the served
    // `Thread.join`, a sleep), so one atomic load answers, with no per-thread
    // mutex; a timed served join reads `IN_OBJECT_WAIT`, as HotSpot's `join`
    // (a `wait` loop) does, where the waiting-monitor test read `PARKED`.
    if let Some(kind) = registry.published_wait_kind_of(id) {
        return published_wait_kind_bit(kind);
    }
    // Interpreter round i2 wave 65, lane L1 (`CRATONVM_SERVED_WAIT_KINDS`): a
    // served stand-in's monitor wait for JDK code that parks (the
    // `--compatible` `CountDownLatch.await`) publishes a waiting monitor too,
    // which would read `IN_OBJECT_WAIT`; HotSpot's AQS acquire is `PARKED`.
    if registry.published_monitor_parked(id) {
        return THREAD_STATE_PARKED;
    }
    if registry.peek_jmx_waiting_monitor(id).is_some() {
        return THREAD_STATE_IN_OBJECT_WAIT;
    }
    // Interpreter round i2 wave 63, lane L1 (`CRATONVM_PUBLISHED_WAIT_KIND`):
    // the sleep's own region says it is a sleep, whatever frame it was
    // entered from (a reflective call, a method handle, `TimeUnit.sleep`);
    // the frame test below read those PARKED.
    match registry.published_sleeping(id) {
        Some(true) => return THREAD_STATE_SLEEPING,
        Some(false) => return THREAD_STATE_PARKED,
        None => {}
    }
    let trace = registry.frame_trace_of(id);
    let Some(top) = trace.last() else {
        return THREAD_STATE_PARKED;
    };
    if crate::debug::is_thread_sleep_frame(top) {
        return THREAD_STATE_SLEEPING;
    }
    let cm = shared.classes.class_manager.read();
    if crate::debug::frame_invokes_thread_sleep(&cm, top) {
        THREAD_STATE_SLEEPING
    } else {
        THREAD_STATE_PARKED
    }
}

// ---------------------------------------------------------------------------
// Locals and monitors of the current thread (interpreter round i1 wave 44,
// lane L1)
// ---------------------------------------------------------------------------
//
// The local-variable functions (slots 21-30 and 155), `GetCurrentContendedMonitor`
// (11) and `GetOwnedMonitorStackDepthInfo` (153) answered
// `JVMTI_ERROR_NOT_AVAILABLE` whatever they were asked, where HotSpot answers
// `JVMTI_ERROR_MUST_POSSESS_CAPABILITY` without the capability and serves
// them with it. Each code below is HotSpot 25.0.3's, measured
// (`tools/probes/interp/L1/L1W44JvmtiLocalsAndStack.java`, run as an
// `-agentpath` agent from a JNI native): the phase, then the capability,
// then the thread, a negative depth (`ILLEGAL_ARGUMENT`), a NULL
// out-parameter (`NULL_POINTER`), another running thread
// (`THREAD_NOT_SUSPENDED`), a depth past the bottom (`NO_MORE_FRAMES`), a
// native method's frame or a compiled activation (`OPAQUE_FRAME`), then the
// slot ([`checked_local`]).

/// `jvmtiMonitorStackDepthInfo`: `{ jobject monitor; jint stack_depth; }`,
/// 16 bytes with its padding.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MonitorDepthRow {
    monitor: JObject,
    stack_depth: JInt,
}

/// The kind a local-variable function reads or writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalKind {
    Int,
    Long,
    Float,
    Double,
    Object,
}

impl LocalKind {
    /// Two slots: `long` and `double`.
    fn wide(self) -> bool {
        matches!(self, LocalKind::Long | LocalKind::Double)
    }

    /// The kind of a field descriptor's first byte (`Z`, `B`, `C`, `S` and
    /// `I` are all `Int`, as `GetLocalInt` takes all five).
    fn of_signature(first: u8) -> Self {
        match first {
            b'J' => LocalKind::Long,
            b'F' => LocalKind::Float,
            b'D' => LocalKind::Double,
            b'L' | b'[' => LocalKind::Object,
            _ => LocalKind::Int,
        }
    }
}

/// What a method's `LocalVariableTable` says of a slot at a bytecode index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Declared {
    /// The method has no table.
    NoTable,
    /// The table has no entry for the slot that covers the index.
    NotLive,
    /// The covering entry's kind.
    Kind(LocalKind),
}

/// [`Declared`] for `slot` of `frame`'s method at the frame's current
/// bytecode (`last_instr_pc`: its invoke for a caller, the bytecode an event
/// is posted for on top). A frame running an obsolete method is read as one
/// without a table: its class's current table may describe other bytecode.
fn declared_local(shared: &SharedVm, frame: &crate::runtime::frame::Frame, slot: u16) -> Declared {
    if frame.runs_obsolete_method() {
        return Declared::NoTable;
    }
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(frame.class_id) else {
        return Declared::NoTable;
    };
    let Some(code) = class
        .find_method(frame.method_name(), frame.method_descriptor())
        .and_then(|m| m.code())
    else {
        return Declared::NoTable;
    };
    declared_in_code(class, code, frame.last_instr_pc, slot)
}

/// [`Declared`] for `slot` at `bci` of the method whose `code` `class`
/// declares: [`declared_local`]'s table read, split out in j27 wave 5 (lane
/// JVMTIFRAME) for a compiled row, which has no `Frame`
/// ([`compiled_locals`]).
fn declared_in_code(
    class: &crate::classloading::Class,
    code: &cratonvm_reader::attribute::CodeAttribute,
    bci: usize,
    slot: u16,
) -> Declared {
    use cratonvm_reader::attribute::Attribute;
    let mut has_table = false;
    for attribute in &code.attributes {
        let Attribute::LocalVariableTable(entries) = attribute else {
            continue;
        };
        has_table = true;
        // Interpreter round i5 wave 100, lane L1 (page
        // `interpreter-i100-L1-jvmti-local-scope-end-is-inclusive-FIXED-20261016.md`): the END is
        // inclusive and the first entry in table order wins, as HotSpot's
        // `VM_BaseGetOrSetLocal::check_slot_type_lvt` reads the table
        // (`start_bci <= bci && bci <= start_bci + length`). The JDWP side
        // already accepts the end (`commands::local_read_refusal`'s `pc == end`).
        for e in entries {
            let start = usize::from(e.start_pc);
            if e.index == slot && start <= bci && bci <= start + usize::from(e.length) {
                let first = class
                    .constant_pool
                    .get_utf8(e.descriptor_index)
                    .and_then(|d| d.bytes().next())
                    .unwrap_or(b'I');
                return Declared::Kind(LocalKind::of_signature(first));
            }
        }
    }
    if has_table {
        Declared::NotLive
    } else {
        Declared::NoTable
    }
}

/// The `LocalVariableTable` signature of the reference variable covering
/// `slot` of `frame`'s method at the frame's current bytecode, as
/// [`declared_local`] finds it; `None` without a table, for an obsolete
/// method, a slot no entry covers, or a primitive variable.
fn declared_reference_signature(
    shared: &SharedVm,
    frame: &crate::runtime::frame::Frame,
    slot: JInt,
) -> Option<String> {
    use cratonvm_reader::attribute::Attribute;
    let slot = u16::try_from(slot).ok()?;
    if frame.runs_obsolete_method() {
        return None;
    }
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(frame.class_id)?;
    let code = class
        .find_method(frame.method_name(), frame.method_descriptor())
        .and_then(|m| m.code())?;
    let bci = frame.last_instr_pc;
    for attribute in &code.attributes {
        let Attribute::LocalVariableTable(entries) = attribute else {
            continue;
        };
        for e in entries {
            let start = usize::from(e.start_pc);
            if e.index == slot && start <= bci && bci <= start + usize::from(e.length) {
                let signature = class.constant_pool.get_utf8(e.descriptor_index)?;
                return matches!(signature.bytes().next(), Some(b'L' | b'['))
                    .then(|| signature.to_string());
            }
        }
    }
    None
}

/// Does `obj`, a non-null value of `SetLocalObject`, fit the variable whose
/// signature is `signature`? HotSpot's `check_slot_type_lvt` refuses one
/// that does not with `TYPE_MISMATCH` (`is_assignable`, by name;
/// `debug::inspect::object_fits_local_signature`). Interpreter round i4 wave
/// 84, lane L1: unchecked before, so an agent could store a `String` into an
/// `Integer` local and the method's next `getfield` or `invokevirtual` ran
/// on an object of the wrong class. The callers hold no class-manager guard
/// but may be inside a foreign JNI entry, so the class manager is only tried
/// (it cannot block); a write that finds it write-locked is let through, as
/// every write was before.
fn object_fits_declared(shared: &SharedVm, signature: Option<&str>, obj: ObjectRef) -> bool {
    let Some(signature) = signature else {
        return true;
    };
    let Some(cm) = shared.classes.class_manager.try_read_untracked() else {
        return true;
    };
    crate::debug::inspect::object_fits_local_signature(
        &cm,
        shared.mem.heap.class_id_of(obj),
        signature,
    )
}

/// May a `kind` local be read or written at `slot` (a `jint` from the agent)
/// of `frame`? The slot as a `u16`, or the error, in HotSpot 25.0.3's order
/// (measured, `L1W44JvmtiLocalsAndStack`):
///
/// 1. `INVALID_SLOT` for a slot outside the frame's locals (the second slot
///    of a `long` / `double` included);
/// 2. `TYPE_MISMATCH` when the slot holds a reference and the kind is not
///    `Object`, or the kind is `Object` and the slot holds none (an
///    `Object` read of a variable not yet assigned is `TYPE_MISMATCH`);
/// 3. with a `LocalVariableTable`: `INVALID_SLOT` when no entry for the slot
///    covers the frame's bytecode (javac's hidden copy of a `synchronized`
///    block's lock, the second slot of a `long`, a variable not yet
///    assigned read as an `int`), `TYPE_MISMATCH` when the covering entry
///    declares another kind.
fn checked_local(
    declared: Declared,
    frame: &crate::runtime::frame::Frame,
    slot: JInt,
    kind: LocalKind,
) -> Result<u16, JInt> {
    let slot = u16::try_from(slot).map_err(|_| ERR_INVALID_SLOT)?;
    let last = usize::from(slot) + usize::from(kind.wide());
    if last >= frame.locals_len() {
        return Err(ERR_INVALID_SLOT);
    }
    let tag = frame.get_local_tag(usize::from(slot));
    let holds_reference = tag == crate::types::VTAG_OBJECT || tag == crate::types::VTAG_NULL;
    if holds_reference != (kind == LocalKind::Object) {
        return Err(ERR_TYPE_MISMATCH);
    }
    match declared {
        Declared::NoTable => Ok(slot),
        Declared::NotLive => Err(ERR_INVALID_SLOT),
        Declared::Kind(k) if k == kind => Ok(slot),
        Declared::Kind(_) => Err(ERR_TYPE_MISMATCH),
    }
}

/// The VM, when `env` is in the live phase and possesses the capability
/// `bit` of word `word`.
fn live_phase_vm_with(
    env: *mut JvmtiNativeEnv,
    word: usize,
    bit: u32,
) -> Result<Arc<SharedVm>, JInt> {
    let shared = live_phase_vm(env)?;
    let possessed = live_env(env)?
        .state
        .lock()
        .capabilities
        .get(word)
        .is_some_and(|w| w & bit != 0);
    if !possessed {
        return Err(ERR_MUST_POSSESS_CAPABILITY);
    }
    Ok(shared)
}

/// The frame a local-variable function names, after the checks every one of
/// them makes (see the section note).
enum LocalTarget {
    /// The calling thread's own `JvmThread` and its listing row at the depth.
    Current(*mut JvmThread, ListedRow),
    /// Another thread, suspended, and the depth in its listing (interpreter
    /// round i1 wave 46, lane L1; [`other_local`]).
    Other(u64, usize),
}

/// The frame a local-variable function names: `(vm, the frame)`, after the
/// checks every one of them makes (see the section note). `out_is_null`: the
/// function's out-parameter (or, for a write, nothing: `false`) is NULL.
fn local_row(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    out_is_null: bool,
) -> Result<(Arc<SharedVm>, LocalTarget), JInt> {
    let shared = live_phase_vm_with(env, 0, CAN_ACCESS_LOCAL_VARIABLES)?;
    let current = jni::current_jvm_thread_of(&shared).ok_or(ERR_UNATTACHED_THREAD)?;
    // SAFETY: the calling thread's own, live `JvmThread`; `thread_id` is an
    // immutable `Copy` field.
    let current_tid = unsafe { (*current).thread_id.0 };
    let other = if thread == 0 {
        None
    } else {
        Some(thread_id_of(&shared, thread)?).filter(|&tid| tid != current_tid)
    };
    if depth < 0 {
        return Err(ERR_ILLEGAL_ARGUMENT);
    }
    if out_is_null {
        return Err(ERR_NULL_POINTER);
    }
    let depth = usize::try_from(depth).map_err(|_| ERR_ILLEGAL_ARGUMENT)?;
    // Another thread's frames may be read only while it is suspended:
    // HotSpot's answer for a running thread. Wave 46 (lane L1): a suspended
    // one's are read and written where they hold still ([`other_local`]),
    // as HotSpot serves them (`L1W46JvmtiOtherThreadLocalsAndMonitors`).
    if let Some(tid) = other {
        if !shared.debug.debug_state.lock().is_thread_suspended(tid) {
            return Err(ERR_THREAD_NOT_SUSPENDED);
        }
        return Ok((shared, LocalTarget::Other(tid, depth)));
    }
    // SAFETY: as above; the thread is this one, inside this call.
    let rows = listed_rows(&shared, unsafe { &*current }, true);
    let row = rows.into_iter().nth(depth).ok_or(ERR_NO_MORE_FRAMES)?;
    Ok((shared, LocalTarget::Current(current, row)))
}

/// Read local `slot` of `kind` of the current thread's frame at `depth`:
/// the value, or the error. An object is answered as a local reference.
fn read_local(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    kind: LocalKind,
    out_is_null: bool,
) -> Result<LocalRead, JInt> {
    let (shared, current, row) = match local_row(env, thread, depth, out_is_null)? {
        (shared, LocalTarget::Current(current, row)) => (shared, current, row),
        (shared, LocalTarget::Other(tid, depth)) => {
            let read = other_local(&shared, tid, depth, LocalOp::Read(kind, slot))?;
            return read.map(|v| caller_value(&shared, v)).ok_or(ERR_INTERNAL);
        }
    };
    if let (None, Some(record)) = (row.frame, row.proxy_locals) {
        // The i54-L1 retirement: a lambda proxy method's frame, read inside
        // the foreign-thread entry (no collection moves its pins meanwhile).
        let _fx = jni::ForeignJniEntry::enter();
        // SAFETY: the calling thread's own `JvmThread`, inside this call.
        let value = proxy_local_read(unsafe { &*current }, record, slot, kind)?;
        return local_read_of(kind, value, |obj| {
            let prev = jni::replace_jni_context(&shared);
            let handle = obj.map_or(0, jni::new_local_handle);
            jni::restore_jni_context(prev);
            handle
        });
    }
    if let (None, Some(home)) = (row.frame, row.home) {
        // j27 wave 5 (lane JVMTIFRAME): a compiled or OSR'd frame.
        let op = LocalOp::Read(kind, slot);
        return compiled_locals::current_home_read(&shared, current, &row.info, &home, op);
    }
    let index = row.frame.ok_or(ERR_OPAQUE_FRAME)?;
    // SAFETY: the calling thread's own `JvmThread`, inside this call: its
    // frames do not change while they are read.
    let frame = unsafe { (*current).frames.get(index) }.ok_or(ERR_OPAQUE_FRAME)?;
    let declared =
        u16::try_from(slot).map_or(Declared::NoTable, |s| declared_local(&shared, frame, s));
    // The frame's references are read, and a handle made, inside the
    // foreign-thread entry, so no collection moves them meanwhile.
    let _fx = jni::ForeignJniEntry::enter();
    let slot = checked_local(declared, frame, slot, kind)?;
    // gcf f2/f: a liveness-dead reference slot may hold a stale address.
    let live = frame.live_locals_mask_here();
    let value = crate::debug::debugger_local_for_read(&shared, frame, slot, &live);
    let read = match (kind, value) {
        (LocalKind::Int, crate::types::Value::Int(v)) => LocalRead::Int(v),
        (LocalKind::Long, crate::types::Value::Long(v)) => LocalRead::Long(v),
        (LocalKind::Float, crate::types::Value::Float(v)) => LocalRead::Float(v),
        (LocalKind::Double, crate::types::Value::Double(v)) => LocalRead::Double(v),
        (LocalKind::Object, crate::types::Value::Object(obj)) => {
            let prev = jni::replace_jni_context(&shared);
            let handle = obj.map_or(0, jni::new_local_handle);
            jni::restore_jni_context(prev);
            LocalRead::Object(handle)
        }
        _ => return Err(ERR_TYPE_MISMATCH),
    };
    Ok(read)
}

/// A value [`read_local`] read (or a local-variable function writes).
#[derive(Clone, Copy)]
enum LocalRead {
    Int(JInt),
    Long(i64),
    Float(f32),
    Double(f64),
    Object(JObject),
}

/// Write `value` (of `kind`) into local `slot` of the current thread's frame
/// at `depth`; an object `value` is a JNI handle (0: null).
fn write_local(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    kind: LocalKind,
    value: LocalRead,
) -> JInt {
    let (shared, current, row) = match local_row(env, thread, depth, false) {
        Ok((shared, LocalTarget::Current(current, row))) => (shared, current, row),
        Ok((shared, LocalTarget::Other(tid, depth))) => {
            return write_other_local(&shared, tid, depth, slot, kind, value);
        }
        Err(code) => return code,
    };
    if let (None, Some(record)) = (row.frame, row.proxy_locals) {
        // The i54-L1 retirement: a lambda proxy method's frame.
        let _fx = jni::ForeignJniEntry::enter();
        let value = match value {
            LocalRead::Int(v) => Value::Int(v),
            LocalRead::Long(v) => Value::Long(v),
            LocalRead::Float(v) => Value::Float(v),
            LocalRead::Double(v) => Value::Double(v),
            LocalRead::Object(handle) => {
                let prev = jni::replace_jni_context(&shared);
                let obj = if handle == 0 {
                    None
                } else {
                    jni::jobject_to_obj(handle)
                };
                jni::restore_jni_context(prev);
                if handle != 0 && obj.is_none() {
                    return ERR_INVALID_OBJECT;
                }
                Value::Object(obj)
            }
        };
        // SAFETY: the calling thread's own `JvmThread`, inside this call.
        return match proxy_local_write(unsafe { &mut *current }, record, slot, kind, value) {
            Ok(()) => ERR_NONE,
            Err(code) => code,
        };
    }
    let Some(index) = row.frame else {
        return ERR_OPAQUE_FRAME;
    };
    let (declared, signature) = {
        // SAFETY: as in `read_local`.
        let Some(frame) = (unsafe { (*current).frames.get(index) }) else {
            return ERR_OPAQUE_FRAME;
        };
        (
            u16::try_from(slot).map_or(Declared::NoTable, |s| declared_local(&shared, frame, s)),
            (kind == LocalKind::Object)
                .then(|| declared_reference_signature(&shared, frame, slot))
                .flatten(),
        )
    };
    let _fx = jni::ForeignJniEntry::enter();
    // SAFETY: the calling thread's own `JvmThread`, inside this call: the
    // frame is below the native method running, and runs again only when
    // that returns.
    let Some(frame) = (unsafe { (*current).frames.get_mut(index) }) else {
        return ERR_OPAQUE_FRAME;
    };
    let slot = match checked_local(declared, frame, slot, kind) {
        Ok(slot) => slot,
        Err(code) => return code,
    };
    let value = match value {
        LocalRead::Int(v) => crate::types::Value::Int(v),
        LocalRead::Long(v) => crate::types::Value::Long(v),
        LocalRead::Float(v) => crate::types::Value::Float(v),
        LocalRead::Double(v) => crate::types::Value::Double(v),
        LocalRead::Object(handle) => {
            let prev = jni::replace_jni_context(&shared);
            let obj = if handle == 0 {
                None
            } else {
                jni::jobject_to_obj(handle)
            };
            jni::restore_jni_context(prev);
            if handle != 0 && obj.is_none() {
                return ERR_INVALID_OBJECT;
            }
            if obj.is_some_and(|o| !object_fits_declared(&shared, signature.as_deref(), o)) {
                return ERR_TYPE_MISMATCH;
            }
            crate::types::Value::Object(obj)
        }
    };
    frame.set_local(slot, value);
    ERR_NONE
}

/// Slot 21: `GetLocalObject(env, thread, depth, slot, value_ptr)` (wave 44).
extern "C" fn get_local_object(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    value_ptr: *mut JObject,
) -> JInt {
    match read_local(
        env,
        thread,
        depth,
        slot,
        LocalKind::Object,
        value_ptr.is_null(),
    ) {
        Ok(LocalRead::Object(handle)) => {
            // SAFETY: the agent's non-null out-parameter.
            unsafe { *value_ptr = handle };
            ERR_NONE
        }
        Ok(_) => ERR_INTERNAL,
        Err(code) => code,
    }
}

/// Slot 22: `GetLocalInt(env, thread, depth, slot, value_ptr)` (wave 44).
extern "C" fn get_local_int(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    value_ptr: *mut JInt,
) -> JInt {
    match read_local(
        env,
        thread,
        depth,
        slot,
        LocalKind::Int,
        value_ptr.is_null(),
    ) {
        Ok(LocalRead::Int(v)) => {
            // SAFETY: the agent's non-null out-parameter.
            unsafe { *value_ptr = v };
            ERR_NONE
        }
        Ok(_) => ERR_INTERNAL,
        Err(code) => code,
    }
}

/// Slot 23: `GetLocalLong(env, thread, depth, slot, value_ptr)` (wave 44).
extern "C" fn get_local_long(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    value_ptr: *mut i64,
) -> JInt {
    match read_local(
        env,
        thread,
        depth,
        slot,
        LocalKind::Long,
        value_ptr.is_null(),
    ) {
        Ok(LocalRead::Long(v)) => {
            // SAFETY: the agent's non-null out-parameter.
            unsafe { *value_ptr = v };
            ERR_NONE
        }
        Ok(_) => ERR_INTERNAL,
        Err(code) => code,
    }
}

/// Slot 24: `GetLocalFloat(env, thread, depth, slot, value_ptr)` (wave 44).
extern "C" fn get_local_float(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    value_ptr: *mut f32,
) -> JInt {
    match read_local(
        env,
        thread,
        depth,
        slot,
        LocalKind::Float,
        value_ptr.is_null(),
    ) {
        Ok(LocalRead::Float(v)) => {
            // SAFETY: the agent's non-null out-parameter.
            unsafe { *value_ptr = v };
            ERR_NONE
        }
        Ok(_) => ERR_INTERNAL,
        Err(code) => code,
    }
}

/// Slot 25: `GetLocalDouble(env, thread, depth, slot, value_ptr)` (wave 44).
extern "C" fn get_local_double(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    value_ptr: *mut f64,
) -> JInt {
    match read_local(
        env,
        thread,
        depth,
        slot,
        LocalKind::Double,
        value_ptr.is_null(),
    ) {
        Ok(LocalRead::Double(v)) => {
            // SAFETY: the agent's non-null out-parameter.
            unsafe { *value_ptr = v };
            ERR_NONE
        }
        Ok(_) => ERR_INTERNAL,
        Err(code) => code,
    }
}

/// Slot 26: `SetLocalObject(env, thread, depth, slot, value)` (wave 44).
extern "C" fn set_local_object(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    value: JObject,
) -> JInt {
    write_local(
        env,
        thread,
        depth,
        slot,
        LocalKind::Object,
        LocalRead::Object(value),
    )
}

/// Slot 27: `SetLocalInt(env, thread, depth, slot, value)` (wave 44).
extern "C" fn set_local_int(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    value: JInt,
) -> JInt {
    write_local(
        env,
        thread,
        depth,
        slot,
        LocalKind::Int,
        LocalRead::Int(value),
    )
}

/// Slot 28: `SetLocalLong(env, thread, depth, slot, value)` (wave 44).
extern "C" fn set_local_long(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    value: i64,
) -> JInt {
    write_local(
        env,
        thread,
        depth,
        slot,
        LocalKind::Long,
        LocalRead::Long(value),
    )
}

/// Slot 29: `SetLocalFloat(env, thread, depth, slot, value)` (wave 44). The
/// `jfloat` comes in a floating-point register, as C passes it.
extern "C" fn set_local_float(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    value: f32,
) -> JInt {
    write_local(
        env,
        thread,
        depth,
        slot,
        LocalKind::Float,
        LocalRead::Float(value),
    )
}

/// Slot 30: `SetLocalDouble(env, thread, depth, slot, value)` (wave 44).
extern "C" fn set_local_double(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    value: f64,
) -> JInt {
    write_local(
        env,
        thread,
        depth,
        slot,
        LocalKind::Double,
        LocalRead::Double(value),
    )
}

/// Slot 155: `GetLocalInstance(env, thread, depth, value_ptr)` (wave 44):
/// the receiver, slot 0 of an instance method's frame. A static method's
/// frame (a static native's included) is `INVALID_SLOT`, as HotSpot 25.0.3
/// answers before it looks at the frame; an instance native's or a compiled
/// frame is `OPAQUE_FRAME`.
extern "C" fn get_local_instance(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    value_ptr: *mut JObject,
) -> JInt {
    let (shared, current, row) = match local_row(env, thread, depth, value_ptr.is_null()) {
        Ok((shared, LocalTarget::Current(current, row))) => (shared, current, row),
        Ok((shared, LocalTarget::Other(tid, depth))) => {
            return match other_local(&shared, tid, depth, LocalOp::Instance) {
                Ok(Some(LocalRead::Object(global))) => {
                    let handle = global_to_local(&shared, global);
                    // SAFETY: the agent's non-null out-parameter.
                    unsafe { *value_ptr = handle };
                    ERR_NONE
                }
                Ok(_) => ERR_INTERNAL,
                Err(code) => code,
            };
        }
        Err(code) => return code,
    };
    if let (None, Some(record)) = (row.frame, row.proxy_locals) {
        // The i54-L1 retirement: a lambda proxy method's frame (an instance
        // method of the spun class: the proxy).
        let _fx = jni::ForeignJniEntry::enter();
        // SAFETY: the calling thread's own `JvmThread`, inside this call.
        let receiver = match proxy_local_read(unsafe { &*current }, record, 0, LocalKind::Object) {
            Ok(Value::Object(receiver)) => receiver,
            Ok(_) => return ERR_TYPE_MISMATCH,
            Err(code) => return code,
        };
        let prev = jni::replace_jni_context(&shared);
        let handle = receiver.map_or(0, jni::new_local_handle);
        jni::restore_jni_context(prev);
        // SAFETY: the agent's non-null out-parameter.
        unsafe { *value_ptr = handle };
        return ERR_NONE;
    }
    let is_static = with_method(&shared, row.info.method, |_, m| m.is_static()).unwrap_or(true);
    if is_static {
        return ERR_INVALID_SLOT;
    }
    if let (None, Some(home)) = (row.frame, row.home) {
        // j27 wave 5 (lane JVMTIFRAME): a compiled or OSR'd frame.
        let op = LocalOp::Instance;
        return match compiled_locals::current_home_read(&shared, current, &row.info, &home, op) {
            Ok(LocalRead::Object(handle)) => {
                // SAFETY: the agent's non-null out-parameter.
                unsafe { *value_ptr = handle };
                ERR_NONE
            }
            Ok(_) => ERR_INTERNAL,
            Err(code) => code,
        };
    }
    let Some(index) = row.frame else {
        return ERR_OPAQUE_FRAME;
    };
    let _fx = jni::ForeignJniEntry::enter();
    // SAFETY: as in `read_local`.
    let Some(frame) = (unsafe { (*current).frames.get(index) }) else {
        return ERR_OPAQUE_FRAME;
    };
    let crate::types::Value::Object(receiver) = crate::debug::debugger_local(frame, 0) else {
        return ERR_TYPE_MISMATCH;
    };
    let prev = jni::replace_jni_context(&shared);
    let handle = receiver.map_or(0, jni::new_local_handle);
    jni::restore_jni_context(prev);
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *value_ptr = handle };
    ERR_NONE
}

/// Slot 11: `GetCurrentContendedMonitor(env, thread, monitor_ptr)` (wave
/// 44): the monitor the thread is blocked entering (never one it waits on,
/// JDK-8256314, unless it was notified and is blocked re-entering it), NULL
/// when none, from its lock record
/// (`ThreadRegistry::jmx_lock_snapshot`, which any thread's record answers).
/// The current thread (a NULL `thread`) is never blocked entering one.
extern "C" fn get_current_contended_monitor(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    monitor_ptr: *mut JObject,
) -> JInt {
    let shared = match live_phase_vm_with(env, 0, CAN_GET_CURRENT_CONTENDED_MONITOR) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    // Interpreter round i5 wave 101, lane L1: the out-parameter first, as
    // HotSpot's generated entry pre-checks it (see `get_thread_info`).
    if monitor_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let tid = if thread == 0 {
        let Some(current) = jni::current_jvm_thread_of(&shared) else {
            return ERR_UNATTACHED_THREAD;
        };
        // SAFETY: the calling thread's own `JvmThread`; `thread_id` is `Copy`.
        unsafe { (*current).thread_id.0 }
    } else {
        match thread_id_of(&shared, thread) {
            Ok(tid) => tid,
            Err(code) => return code,
        }
    };
    let _fx = jni::ForeignJniEntry::enter();
    // Interpreter round i2 wave 56, lanes L1 (`L1W56JvmtiReentryContended`)
    // and L6: a notified waiter re-entering its monitor -- BLOCKED while its
    // wait's monitor still stands -- contends for that monitor, as HotSpot
    // answers it (JDK-8256314) and as JDWP's twin answers since wave 55
    // (`ThreadRegistry::current_contended_monitor`). This answered NULL.
    let contended = shared
        .threads
        .thread_registry
        .current_contended_monitor(ThreadId(tid));
    let prev = jni::replace_jni_context(&shared);
    let handle = contended.map_or(0, jni::new_local_handle);
    jni::restore_jni_context(prev);
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *monitor_ptr = handle };
    ERR_NONE
}

/// Slot 153: `GetOwnedMonitorStackDepthInfo(env, thread, count_ptr,
/// info_ptr)` (wave 44), of the current thread: the monitors it owns, in
/// the order and at the depths JDWP's `OwnedMonitorsStackDepthInfo`
/// answers them (`debug::inspect::jvmti_owned_monitor_order`: by frame,
/// innermost first, each frame's in the order it entered them; -1 for a
/// monitor no interpreter frame records, one entered through JNI), each
/// depth a place in this table's listing ([`listed_rows`]). HotSpot 25.0.3:
/// `count=1 depth=1` for a `synchronized` block of the frame below the
/// native that asks. Another thread's (interpreter round i1 wave 46, lane
/// L1; it was `JVMTI_ERROR_NOT_AVAILABLE`) are read where its frames hold
/// still ([`other_monitors`]), running or not, as HotSpot reads them through
/// a handshake.
extern "C" fn get_owned_monitor_stack_depth_info(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    count_ptr: *mut JInt,
    info_ptr: *mut *mut MonitorDepthRow,
) -> JInt {
    owned_monitor_rows(
        env,
        thread,
        (1, CAN_GET_OWNED_MONITOR_STACK_DEPTH_INFO),
        count_ptr,
        info_ptr,
    )
}

/// Slot 10: `GetOwnedMonitorInfo(env, thread, count_ptr, monitors_ptr)`
/// (interpreter round i1 wave 51, lane L1), behind
/// `can_get_owned_monitor_info`: the monitors
/// [`get_owned_monitor_stack_depth_info`] answers, in its order, without
/// their depths, as HotSpot builds both answers from one list
/// (`JvmtiEnvBase::get_owned_monitors`). It was `JVMTI_ERROR_NOT_AVAILABLE`.
/// The errors come in the same order (a NULL out-parameter is found where
/// that function finds it).
extern "C" fn get_owned_monitor_info(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    count_ptr: *mut JInt,
    monitors_ptr: *mut *mut JObject,
) -> JInt {
    let mut count: JInt = 0;
    let mut rows: *mut MonitorDepthRow = std::ptr::null_mut();
    let (inner_count, inner_rows) = if count_ptr.is_null() || monitors_ptr.is_null() {
        (std::ptr::null_mut(), std::ptr::null_mut())
    } else {
        (
            &mut count as *mut JInt,
            &mut rows as *mut *mut MonitorDepthRow,
        )
    };
    let code = owned_monitor_rows(
        env,
        thread,
        (0, CAN_GET_OWNED_MONITOR_INFO),
        inner_count,
        inner_rows,
    );
    if code != ERR_NONE {
        return code;
    }
    let n = usize::try_from(count).unwrap_or(0);
    let Some(size) = n.checked_mul(std::mem::size_of::<JObject>()) else {
        // SAFETY: `rows` is the block `owned_monitor_rows` just allocated.
        unsafe { free_block(rows.cast::<u8>()) };
        return ERR_OUT_OF_MEMORY;
    };
    let block = allocate_block(size).cast::<JObject>();
    if block.is_null() {
        // SAFETY: as above.
        unsafe { free_block(rows.cast::<u8>()) };
        return ERR_OUT_OF_MEMORY;
    }
    for i in 0..n {
        // SAFETY: `rows` holds `count` rows and `block` room for as many
        // handles (`allocate_block` of their size, 16-aligned).
        unsafe { block.add(i).write((*rows.add(i)).monitor) };
    }
    // SAFETY: `rows` is the block `owned_monitor_rows` allocated; the agent's
    // out-parameters are non-null (checked above).
    unsafe {
        free_block(rows.cast::<u8>());
        *count_ptr = count;
        *monitors_ptr = block;
    }
    ERR_NONE
}

/// The monitors `owned` in `GetOwnedMonitorStackDepthInfo`'s order, each at
/// the depth of the row (`rows`, [`listed_rows`] of the CALLING thread, whose
/// `frames` these are) that locked it.
///
/// JIT round 19 proposal wave 7, lane gc7 (SYNC619-3): with compiled
/// activations on the calling thread's stack, attributed by TRACE index --
/// the interpreter frames' monitors and the compiled activations' own holds
/// (`held_monitors::frame_locked_monitors_with_compiled`, which checks its
/// layout against this trace entry by entry and answers `None` on any
/// disagreement) -- each at the depth of the row listing that trace entry
/// (`ListedRow::at`). Otherwise, or on `None`: by interpreter frame, as
/// before (a compiled hold at `-1`).
fn own_monitor_order(
    shared: &SharedVm,
    frames: &[crate::runtime::frame::Frame],
    rows: &[ListedRow],
    owned: &[ObjectRef],
) -> Vec<(ObjectRef, JInt)> {
    use crate::runtime::interpreter::held_monitors as hm;
    let traced = if crate::jit::conservative_roots::current_thread_jit_depth() != 0
        && hm::compiled_frame_monitor_depths_enabled()
        && hm::jvmti_compiled_frame_monitors_enabled()
    {
        let (trace, _) =
            crate::runtime::stackwalker::capture_trace_with_anchor_positions(shared, frames, &[]);
        hm::frame_locked_monitors_with_compiled(shared, frames, &trace, trace.len(), &[])
    } else {
        None
    };
    match traced {
        Some(pairs) => crate::debug::inspect::jvmti_owned_monitor_order(owned, pairs, |at| {
            rows.iter()
                .position(|row| row.at == Some(at))
                .and_then(|depth| JInt::try_from(depth).ok())
        }),
        None => {
            let pairs = hm::frame_locked_monitors(frames);
            crate::debug::inspect::jvmti_owned_monitor_order(owned, pairs, |index| {
                rows.iter()
                    .position(|row| row.frame == Some(index))
                    .and_then(|depth| JInt::try_from(depth).ok())
            })
        }
    }
}

/// [`get_owned_monitor_stack_depth_info`]'s body, behind the capability
/// `(word, bit)` the asking function needs.
fn owned_monitor_rows(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    (word, bit): (usize, u32),
    count_ptr: *mut JInt,
    info_ptr: *mut *mut MonitorDepthRow,
) -> JInt {
    let shared = match live_phase_vm_with(env, word, bit) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    // Interpreter round i5 wave 101, lane L1: the out-parameters first, as
    // HotSpot's generated entry pre-checks them (see `get_thread_info`).
    if count_ptr.is_null() || info_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let Some(current) = jni::current_jvm_thread_of(&shared) else {
        return ERR_UNATTACHED_THREAD;
    };
    // SAFETY: the calling thread's own `JvmThread`; `thread_id` is `Copy`.
    let current_tid = unsafe { (*current).thread_id.0 };
    let other = if thread == 0 {
        None
    } else {
        match thread_id_of(&shared, thread) {
            Ok(tid) => Some(tid).filter(|&tid| tid != current_tid),
            Err(code) => return code,
        }
    };
    if let Some(tid) = other {
        // Wave 46 (lane L1): read where the thread holds still, as HotSpot
        // reads a running thread's through a handshake.
        return match other_monitors(&shared, tid) {
            Ok(monitors) => write_monitor_rows(&shared, &monitors, count_ptr, info_ptr),
            Err(code) => code,
        };
    }
    // SAFETY: the calling thread's own `JvmThread`, inside this call.
    let rows = listed_rows(&shared, unsafe { &*current }, true);
    let _fx = jni::ForeignJniEntry::enter();
    let Some((contended, waiting, locked, ..)) = shared
        .threads
        .thread_registry
        .jmx_lock_snapshot(ThreadId(current_tid))
    else {
        return ERR_INTERNAL;
    };
    let owned: Vec<ObjectRef> = locked
        .into_iter()
        .filter(|&o| Some(o) != waiting && Some(o) != contended)
        .collect();
    // SAFETY: as above.
    let monitors = own_monitor_order(&shared, unsafe { &(*current).frames }, &rows, &owned);
    let Ok(count) = JInt::try_from(monitors.len()) else {
        return ERR_INTERNAL;
    };
    let Some(size) = monitors
        .len()
        .checked_mul(std::mem::size_of::<MonitorDepthRow>())
    else {
        return ERR_OUT_OF_MEMORY;
    };
    let block = allocate_block(size).cast::<MonitorDepthRow>();
    if block.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    let prev = jni::replace_jni_context(&shared);
    for (i, &(obj, depth)) in monitors.iter().enumerate() {
        let row = MonitorDepthRow {
            monitor: jni::new_local_handle(obj),
            stack_depth: depth,
        };
        // SAFETY: `block` holds `monitors.len()` rows (`allocate_block` of
        // their size, 16-aligned).
        unsafe { block.add(i).write(row) };
    }
    jni::restore_jni_context(prev);
    // SAFETY: the agent's non-null out-parameters.
    unsafe {
        *count_ptr = count;
        *info_ptr = block;
    }
    ERR_NONE
}

// ---------------------------------------------------------------------------
// Another thread's locals and monitors (interpreter round i1 wave 46, lane L1)
// ---------------------------------------------------------------------------
//
// What remained of
// `docs/feature-designs/jvmti-delivery-threading.md`:
// the local-variable functions of another (suspended) thread and
// `GetOwnedMonitorStackDepthInfo` of another (any) thread. Both are answered
// where the thread's frames hold still, by the same two routes the stack
// functions use ([`other_thread_frames`]):
//
// * blocked in a native region: through its inspection window
//   (`interpreter::with_blocked_frames`), read by the caller while the
//   thread's leave waits. A reference local is its current address from the
//   blocking deposit's per-slot record; a primitive may be written in place
//   (the wake rewrites only reference slots); a reference is not (it would
//   be neither a root nor remapped while the thread sleeps): since wave 47
//   it is recorded, held by a global reference, for the thread's wake to
//   store after its write-back (`debug::inspect::defer_jvmti_object_write`),
//   and a read before the wake answers it; wave 46 answered
//   `JVMTI_ERROR_NOT_AVAILABLE`.
// * parked at an interpreter suspend point: on the thread itself
//   (`debug::run_on_parked_thread`), which is a registered mutator between
//   two polls, while the caller waits GC-safe. An object crosses between the
//   two threads as a JNI global reference ([`global_ref_of`],
//   [`global_to_local`]), so a collection between the answer and the
//   caller's local handle cannot move it unseen.
//
// Depths count the rows [`frames_of`] answers for the thread
// ([`other_rows`]); a row's interpreter frame is checked against the frame
// before it is touched. Each code is HotSpot 25.0.3's, measured
// (`tools/probes/interp/L1/L1W46JvmtiOtherThreadLocalsAndMonitors.java`).
// Positive control: `CRATONVM_FRAME_TRACE=1` prints `[JVMTI_OTHER_LOCAL]
// tid=<n> depth=<d> via=window|parked err=<code>` per local read or write and
// `[JVMTI_OTHER_MONITORS] tid=<n> count=<k> via=window|parked
// handshake=<bool>` per monitor read.

/// A local-variable function's operation on another thread's frame.
#[derive(Clone, Copy)]
enum LocalOp {
    /// Read local `slot` (the agent's `jint`) as `kind`.
    Read(LocalKind, JInt),
    /// Write a `kind` value into local `slot`; an object value is a JNI
    /// global reference (0: null) the caller made ([`write_other_local`]).
    Write(LocalKind, JInt, LocalRead),
    /// `GetLocalInstance`: the receiver.
    Instance,
}

impl LocalOp {
    fn name(self) -> &'static str {
        match self {
            LocalOp::Read(..) => "read",
            LocalOp::Write(..) => "write",
            LocalOp::Instance => "instance",
        }
    }
}

/// Run `op` on the frame at `depth` of suspended thread `tid`, another than
/// the caller, where its frames hold still ([`on_stopped_thread`]). An
/// object read comes back as a JNI global reference, which
/// [`caller_value`] makes the caller's; a write answers `None`.
fn other_local(
    shared: &SharedVm,
    tid: u64,
    depth: usize,
    op: LocalOp,
) -> Result<Option<LocalRead>, JInt> {
    let answer = on_stopped_thread(
        shared,
        tid,
        move |shared: &SharedVm, thread: &mut JvmThread| {
            local_op_on_parked(shared, thread, depth, op)
        },
        |held: &crate::runtime::interpreter::BlockedFrames<'_>| {
            local_op_through_window(shared, held, depth, op)
        },
    );
    let (answer, via) = match answer {
        Ok((answer, via)) => (answer, via),
        Err(code) => (Err(code), "none"),
    };
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[JVMTI_OTHER_LOCAL] tid={tid} depth={depth} op={} via={via} err={}",
            op.name(),
            answer.as_ref().err().copied().unwrap_or(ERR_NONE)
        );
    }
    answer
}

/// A write of `value` (of `kind`) into local `slot` of the frame at `depth`
/// of suspended thread `tid` ([`other_local`]): an object `value`, the
/// agent's handle (0: null), is carried as a global reference, deleted once
/// the write is done.
fn write_other_local(
    shared: &SharedVm,
    tid: u64,
    depth: usize,
    slot: JInt,
    kind: LocalKind,
    value: LocalRead,
) -> JInt {
    let carried = match value {
        LocalRead::Object(handle) => match local_to_global(shared, handle) {
            Ok(global) => LocalRead::Object(global),
            Err(code) => return code,
        },
        other => other,
    };
    let answer = other_local(shared, tid, depth, LocalOp::Write(kind, slot, carried));
    if let LocalRead::Object(global) = carried {
        release_global(shared, global);
    }
    match answer {
        Ok(_) => ERR_NONE,
        Err(code) => code,
    }
}

/// Run `parked` on thread `tid` while it is parked at an interpreter suspend
/// point, or `window` on its frames while it is blocked in a native region,
/// whichever it is in first, waiting up to [`STOP_WAIT`] for either (a
/// suspended thread in a compiled loop stops at its next exit-capable poll).
/// Answers the work's answer and the route (`"parked"` / `"window"`);
/// `JVMTI_ERROR_THREAD_NOT_ALIVE` when the thread ended,
/// `JVMTI_ERROR_NOT_AVAILABLE` when it did not stand still in time. The
/// caller holds no lock; it waits GC-safe.
fn on_stopped_thread<R, P, W>(
    shared: &SharedVm,
    tid: u64,
    parked: P,
    window: W,
) -> Result<(R, &'static str), JInt>
where
    R: Send + 'static,
    P: FnOnce(&SharedVm, &mut JvmThread) -> R + Send + Clone + 'static,
    W: Fn(&crate::runtime::interpreter::BlockedFrames<'_>) -> R,
{
    let deadline = std::time::Instant::now() + STOP_WAIT;
    loop {
        if let Some(answer) = crate::runtime::interpreter::with_blocked_frames(shared, tid, &window)
        {
            return Ok((answer, "window"));
        }
        // An unmounted virtual thread (a continuation that yielded in a sleep
        // or a park) has neither a window nor a carrier to park on: its
        // deposited frames are read where they lie (interpreter round i1
        // wave 51, lane L1;
        // `interpreter-L1-the-c-jvmti-table-reads-no-locals-of-an-unmounted-virtual-thread-FIXED-20261015`).
        if let Some(answer) =
            crate::runtime::interpreter::with_unmounted_frames(shared, tid, &window)
        {
            return Ok((answer, "unmounted"));
        }
        if shared.debug.debug_state.lock().is_parked(tid) {
            let work = parked.clone();
            let ran = gc_safe(shared, || {
                crate::debug::run_on_parked_thread(shared, Some(tid), work)
            });
            match ran {
                Ok(answer) => return Ok((answer, "parked")),
                Err(crate::debug::ParkedError::Lost) => return Err(ERR_INTERNAL),
                // Busy (another request's work), or it left its park: again.
                Err(_) => {}
            }
        }
        if !shared.threads.thread_registry.is_alive(ThreadId(tid)) {
            return Err(ERR_THREAD_NOT_ALIVE);
        }
        if std::time::Instant::now() >= deadline {
            return Err(ERR_NOT_AVAILABLE);
        }
        gc_safe(shared, || {
            std::thread::sleep(std::time::Duration::from_millis(1))
        });
    }
}

/// Is row `depth` of a blocked thread's `rows` (top first) a blocking
/// stand-in's `Object.wait()V` / `Object.wait(J)V` Java level -- a row with
/// no interpreter frame and no compiled activation, above the thread's first
/// interpreter frame -- and if so, its `long` timeout when known (`Some(0)`
/// for `wait(J)V` called by `wait()V`, the row below it)? Interpreter round
/// i6 wave 121, lane L1 (i119-L1 step 4). Above the first interpreter frame,
/// every `Object.wait` row is a level of the wait the thread is blocked in,
/// so its receiver is the monitor it waits on. (An inlined `wait` level of a
/// compiled caller qualifies too, and has the same receiver.)
fn waited_monitor_level(shared: &SharedVm, rows: &[OtherRow], depth: usize) -> Option<Option<i64>> {
    if rows.get(..=depth)?.iter().any(|r| r.raw.frame.is_some()) {
        return None;
    }
    let is_wait = |row: &OtherRow, descriptor: &str| -> bool {
        if row.raw.frame.is_some() || row.raw.chain.is_some() || row.info.method == 0 {
            return false;
        }
        // Cast: the id's halves, as `with_method` reads them.
        let class_id = ClassId::new((row.info.method >> 32) as u32);
        let index = (row.info.method & 0xFFFF) as usize;
        let cm = shared.classes.class_manager.read();
        cm.get_class(class_id).is_some_and(|class| {
            &*class.name == "java/lang/Object"
                && class
                    .methods
                    .get(index)
                    .is_some_and(|m| &*m.name == "wait" && &*m.descriptor == descriptor)
        })
    };
    let row = rows.get(depth)?;
    if is_wait(row, "()V") {
        return Some(None);
    }
    if is_wait(row, "(J)V") {
        let under_wait = rows
            .get(depth + 1)
            .is_some_and(|below| is_wait(below, "()V"));
        return Some(under_wait.then_some(0));
    }
    None
}

/// `op` on a stand-in's `Object.wait` level of blocked thread `tid`
/// ([`waited_monitor_level`]): the receiver (`GetLocalInstance`, or slot 0 as
/// an object) is the monitor the thread waits on, read from the slot a moving
/// collection forwards (`ThreadRegistry::peek_jmx_waiting_monitor`) and made
/// global inside the foreign-thread entry; slot 1 as a `long` is the timeout
/// when known. Anything else, a write included (the level has no frame to
/// write), and a thread that published no monitor: `OPAQUE_FRAME`, as
/// before.
fn waited_level_local(
    shared: &SharedVm,
    tid: u64,
    timeout: Option<i64>,
    op: LocalOp,
) -> Result<Option<LocalRead>, JInt> {
    match op {
        LocalOp::Instance | LocalOp::Read(LocalKind::Object, 0) => {
            let _fx = jni::ForeignJniEntry::enter();
            let monitor = shared
                .threads
                .thread_registry
                .peek_jmx_waiting_monitor(ThreadId(tid))
                .ok_or(ERR_OPAQUE_FRAME)?;
            Ok(Some(LocalRead::Object(global_ref_of(
                shared,
                Some(monitor),
            ))))
        }
        LocalOp::Read(LocalKind::Long, 1) => timeout
            .map(|t| Some(LocalRead::Long(t)))
            .ok_or(ERR_OPAQUE_FRAME),
        _ => Err(ERR_OPAQUE_FRAME),
    }
}

/// `op` on a `VirtualThread` instance row of unmounted bound virtual thread
/// `tid`'s continuation head (`StandinLevelLocals::VirtualThreadThis`;
/// interpreter round i6 wave 132, lane L1, the i119-L1 residual): the
/// receiver (`GetLocalInstance`, or slot 0 as an object) is the virtual
/// thread itself, `ThreadRegistry::java_thread_obj` (a forwarded root), made
/// global inside the foreign-thread entry -- the object JDWP's `ThisObject`
/// answers there (`publish_frame_snapshot`, wave 129) and HotSpot 25.0.3's
/// `ThisObject` answers (`L1W129RawJdwpVirtualWaitThisObject`). Anything
/// else, a write included (an unmounted thread's frames are read only, as
/// HotSpot's stack chunk is), and a thread with no mirror: `OPAQUE_FRAME`,
/// as before.
fn virtual_thread_level_local(
    shared: &SharedVm,
    tid: u64,
    op: LocalOp,
) -> Result<Option<LocalRead>, JInt> {
    match op {
        LocalOp::Instance | LocalOp::Read(LocalKind::Object, 0) => {
            let _fx = jni::ForeignJniEntry::enter();
            let thread = shared
                .threads
                .thread_registry
                .java_thread_obj(ThreadId(tid))
                .ok_or(ERR_OPAQUE_FRAME)?;
            Ok(Some(LocalRead::Object(global_ref_of(shared, Some(thread)))))
        }
        _ => Err(ERR_OPAQUE_FRAME),
    }
}

/// The interpreter frame whose locals `row` names: `OPAQUE_FRAME` for a
/// native method's row, a compiled activation, and a frame whose body runs
/// compiled (its locals are its compiled entry's), as for the current thread.
fn local_frame_index(row: &OtherRow) -> Result<usize, JInt> {
    row.raw
        .frame
        .filter(|_| !row.raw.runs_compiled)
        .ok_or(ERR_OPAQUE_FRAME)
}

/// Does `frame` run the method `raw` lists (the listing describes these
/// frames)?
fn row_names_frame(raw: &BlockedRow, frame: &crate::runtime::frame::Frame) -> bool {
    u64::from(frame.class_id.as_u32()) == raw.class_id
        && crate::debug::frame_method_id(frame) == raw.method_id
}

/// `GetLocalInstance` of a row whose method is static: `INVALID_SLOT`, as
/// HotSpot 25.0.3 answers before it looks at the frame.
fn instance_of_static(shared: &SharedVm, row: &OtherRow) -> bool {
    with_method(shared, row.info.method, |_, m| m.is_static()).unwrap_or(true)
}

/// [`other_local`] on the parked thread itself: `thread` runs this, between
/// two polls, and lists its own frames as the current thread's are listed
/// ([`listed_rows`]: the natives it runs, a JNI native in the middle of its
/// stack included, and the frames whose bodies run compiled), so a depth
/// names the frame [`frames_of`] lists there.
fn local_op_on_parked(
    shared: &SharedVm,
    thread: &mut JvmThread,
    depth: usize,
    op: LocalOp,
) -> Result<Option<LocalRead>, JInt> {
    let rows = listed_rows(shared, thread, true);
    let row = rows.get(depth).ok_or(ERR_NO_MORE_FRAMES)?;
    if let (None, Some(record)) = (row.frame, row.proxy_locals) {
        // The i54-L1 retirement: a lambda proxy method's frame.
        return match op {
            LocalOp::Instance => match proxy_local_read(thread, record, 0, LocalKind::Object)? {
                Value::Object(receiver) => {
                    Ok(Some(LocalRead::Object(global_ref_of(shared, receiver))))
                }
                _ => Err(ERR_TYPE_MISMATCH),
            },
            LocalOp::Read(kind, slot) => {
                let value = proxy_local_read(thread, record, slot, kind)?;
                local_read_of(kind, value, |obj| global_ref_of(shared, obj)).map(Some)
            }
            LocalOp::Write(kind, slot, value) => {
                let value = match value {
                    LocalRead::Int(v) => Value::Int(v),
                    LocalRead::Long(v) => Value::Long(v),
                    LocalRead::Float(v) => Value::Float(v),
                    LocalRead::Double(v) => Value::Double(v),
                    LocalRead::Object(global) => {
                        let obj = if global == 0 {
                            None
                        } else {
                            shared.natives.jni_global_refs.lock().resolve(global)
                        };
                        if global != 0 && obj.is_none() {
                            return Err(ERR_INVALID_OBJECT);
                        }
                        Value::Object(obj)
                    }
                };
                proxy_local_write(thread, record, slot, kind, value).map(|()| None)
            }
        };
    }
    if matches!(op, LocalOp::Instance)
        && with_method(shared, row.info.method, |_, m| m.is_static()).unwrap_or(true)
    {
        return Err(ERR_INVALID_SLOT);
    }
    if let (None, Some(home)) = (row.frame, row.home) {
        // j27 wave 5 (lane JVMTIFRAME): a compiled or OSR'd frame, read on
        // the parked thread itself (its own walk); a write stays opaque.
        return compiled_locals::parked_home_read(shared, thread, &row.info, &home, op);
    }
    let index = row.frame.ok_or(ERR_OPAQUE_FRAME)?;
    let frame = thread.frames.get(index).ok_or(ERR_OPAQUE_FRAME)?;
    match op {
        LocalOp::Instance => {
            let crate::types::Value::Object(receiver) = crate::debug::debugger_local(frame, 0)
            else {
                return Err(ERR_TYPE_MISMATCH);
            };
            Ok(Some(LocalRead::Object(global_ref_of(shared, receiver))))
        }
        LocalOp::Read(kind, slot) => {
            let declared =
                u16::try_from(slot).map_or(Declared::NoTable, |s| declared_local(shared, frame, s));
            let slot = checked_local(declared, frame, slot, kind)?;
            // gcf f2/f: a liveness-dead reference slot may hold a stale address.
            let live = frame.live_locals_mask_here();
            let value = crate::debug::debugger_local_for_read(shared, frame, slot, &live);
            local_read_of(kind, value, |obj| global_ref_of(shared, obj)).map(Some)
        }
        LocalOp::Write(kind, slot, value) => {
            let declared =
                u16::try_from(slot).map_or(Declared::NoTable, |s| declared_local(shared, frame, s));
            let signature = (kind == LocalKind::Object)
                .then(|| declared_reference_signature(shared, frame, slot))
                .flatten();
            let slot = checked_local(declared, frame, slot, kind)?;
            let value = match value {
                LocalRead::Int(v) => crate::types::Value::Int(v),
                LocalRead::Long(v) => crate::types::Value::Long(v),
                LocalRead::Float(v) => crate::types::Value::Float(v),
                LocalRead::Double(v) => crate::types::Value::Double(v),
                LocalRead::Object(global) => {
                    let obj = if global == 0 {
                        None
                    } else {
                        shared.natives.jni_global_refs.lock().resolve(global)
                    };
                    if global != 0 && obj.is_none() {
                        return Err(ERR_INVALID_OBJECT);
                    }
                    // Wave 84 (lane L1): HotSpot's `check_slot_type_lvt`.
                    if obj.is_some_and(|o| !object_fits_declared(shared, signature.as_deref(), o)) {
                        return Err(ERR_TYPE_MISMATCH);
                    }
                    crate::types::Value::Object(obj)
                }
            };
            // The frame is not executing while its thread is parked; the
            // bytecode after the park reads the new value (as JDWP's
            // `StackFrame.SetValues` writes it, `debug::inspect`).
            thread
                .frames
                .get_mut(index)
                .ok_or(ERR_OPAQUE_FRAME)?
                .set_local(slot, value);
            Ok(None)
        }
    }
}

/// [`other_local`] through a blocked thread's inspection window: the caller
/// runs this while the thread's leave waits.
fn local_op_through_window(
    shared: &SharedVm,
    held: &crate::runtime::interpreter::BlockedFrames<'_>,
    depth: usize,
    op: LocalOp,
) -> Result<Option<LocalRead>, JInt> {
    let rows = other_rows(shared, held.rows.clone());
    let row = *rows.get(depth).ok_or(ERR_NO_MORE_FRAMES)?;
    if matches!(op, LocalOp::Instance) && instance_of_static(shared, &row) {
        return Err(ERR_INVALID_SLOT);
    }
    // Interpreter round i6 wave 121, lane L1 (i119-L1 step 4,
    // `CRATONVM_DEBUG_STANDIN_LEVEL_LOCALS`, default off): a blocking
    // stand-in's `Object.wait` level, which has no frame.
    if shared.debug.debugger_gates.standin_level_locals() {
        // Interpreter round i6 wave 132, lane L1 (i119-L1 residual): an
        // unmounted bound virtual thread's `VirtualThread` continuation row,
        // tagged by the listing that headed it
        // (`continuation_head_of_unmounted`, wave 129), as JDWP reads it.
        // Wave 133 (lane L9 review): the tags are by LISTING position, and
        // `other_rows` drops a row it cannot name, so `depth` names the same
        // row only while nothing at or above it was dropped (the kept rows
        // keep their order); otherwise the old answer.
        if row.raw.frame.is_none()
            && rows[..=depth].iter().zip(&held.rows).all(|(kept, listed)| {
                kept.raw.class_id == listed.class_id
                    && kept.raw.method_id == listed.method_id
                    && kept.raw.location == listed.location
            })
            && held.level_locals_at(depth)
                == crate::runtime::interpreter::StandinLevelLocals::VirtualThreadThis
        {
            return virtual_thread_level_local(shared, held.tid(), op);
        }
        if let Some(timeout) = waited_monitor_level(shared, &rows, depth) {
            return waited_level_local(shared, held.tid(), timeout, op);
        }
    }
    let index = local_frame_index(&row)?;
    let frame = held.frames().get(index).ok_or(ERR_OPAQUE_FRAME)?;
    if !row_names_frame(&row.raw, frame) {
        return Err(ERR_NOT_AVAILABLE);
    }
    // The class manager first (the declared kind), then the references,
    // read and made global inside the foreign-thread entry, so no collection
    // moves them meanwhile.
    let declared = match op {
        LocalOp::Read(_, slot) | LocalOp::Write(_, slot, _) => {
            u16::try_from(slot).map_or(Declared::NoTable, |s| declared_local(shared, frame, s))
        }
        LocalOp::Instance => Declared::NoTable,
    };
    let signature = match op {
        LocalOp::Write(LocalKind::Object, slot, _) => {
            declared_reference_signature(shared, frame, slot)
        }
        _ => None,
    };
    let _fx = jni::ForeignJniEntry::enter();
    let now = |slot: u16, v: crate::types::Value| match v {
        crate::types::Value::Object(Some(held_ref)) => {
            crate::types::Value::Object(held.current_reference(shared, index, slot, held_ref))
        }
        other => other,
    };
    match op {
        LocalOp::Instance => {
            let crate::types::Value::Object(receiver) =
                now(0, crate::debug::debugger_local(frame, 0))
            else {
                return Err(ERR_TYPE_MISMATCH);
            };
            Ok(Some(LocalRead::Object(global_ref_of(shared, receiver))))
        }
        LocalOp::Read(kind, slot) => {
            let slot = checked_local(declared, frame, slot, kind)?;
            // Wave 47: a reference written while the thread sleeps waits for
            // its wake; a read before then answers it, as HotSpot answers the
            // value just written.
            // gcf f2/f: a liveness-dead reference slot may hold a stale
            // address; screened after the window's remap.
            let value = crate::debug::inspect::deferred_local_value(
                shared,
                held.tid(),
                held.frames(),
                index,
                slot,
            )
            .unwrap_or_else(|| {
                let live = frame.live_locals_mask_here();
                crate::debug::screen_dead_local(
                    shared,
                    &live,
                    slot,
                    now(slot, crate::debug::debugger_local(frame, slot)),
                )
            });
            local_read_of(kind, value, |obj| global_ref_of(shared, obj)).map(Some)
        }
        LocalOp::Write(kind, slot, value) => {
            let slot = checked_local(declared, frame, slot, kind)?;
            // Wave 51 (lane L1): an unmounted virtual thread's frames lie in
            // HotSpot's heap stack chunk, which `SetLocal*` does not write:
            // `JVMTI_ERROR_OPAQUE_FRAME`, after the slot checks
            // (`VM_BaseGetOrSetLocal::doit`).
            if held.unmounted() {
                return Err(ERR_OPAQUE_FRAME);
            }
            let value = match value {
                LocalRead::Int(v) => crate::types::Value::Int(v),
                LocalRead::Long(v) => crate::types::Value::Long(v),
                LocalRead::Float(v) => crate::types::Value::Float(v),
                LocalRead::Double(v) => crate::types::Value::Double(v),
                // Wave 47 (lane L1; item 1 of
                // `docs/feature-designs/jvmti-delivery-threading.md`):
                // not stored into the sleeping thread's frame, where it would
                // be neither a root nor remapped (see the section note), but
                // recorded for the thread's wake, held by a global reference
                // of its own, as JDWP's `SetValues` of wave 41 is.
                LocalRead::Object(global) => {
                    let obj = if global == 0 {
                        None
                    } else {
                        Some(
                            shared
                                .natives
                                .jni_global_refs
                                .lock()
                                .resolve(global)
                                .ok_or(ERR_INVALID_OBJECT)?,
                        )
                    };
                    // Wave 84 (lane L1): HotSpot's `check_slot_type_lvt`.
                    if obj.is_some_and(|o| !object_fits_declared(shared, signature.as_deref(), o)) {
                        return Err(ERR_TYPE_MISMATCH);
                    }
                    let own = obj.map(|obj| global_ref_of(shared, Some(obj)));
                    crate::debug::inspect::defer_jvmti_object_write(
                        shared,
                        held.tid(),
                        index,
                        held.frames().len(),
                        row.raw.class_id,
                        row.raw.method_id,
                        slot,
                        own,
                    );
                    return Ok(None);
                }
            };
            if !held.set_primitive_local(index, slot, value) {
                return Err(ERR_OPAQUE_FRAME);
            }
            Ok(None)
        }
    }
}

/// `value`, read from a local of `kind`, as a [`LocalRead`]; an object is
/// made a handle by `handle`. `TYPE_MISMATCH` when the value is of another
/// kind.
fn local_read_of(
    kind: LocalKind,
    value: crate::types::Value,
    handle: impl FnOnce(Option<ObjectRef>) -> JObject,
) -> Result<LocalRead, JInt> {
    Ok(match (kind, value) {
        (LocalKind::Int, crate::types::Value::Int(v)) => LocalRead::Int(v),
        (LocalKind::Long, crate::types::Value::Long(v)) => LocalRead::Long(v),
        (LocalKind::Float, crate::types::Value::Float(v)) => LocalRead::Float(v),
        (LocalKind::Double, crate::types::Value::Double(v)) => LocalRead::Double(v),
        (LocalKind::Object, crate::types::Value::Object(obj)) => LocalRead::Object(handle(obj)),
        _ => return Err(ERR_TYPE_MISMATCH),
    })
}

/// A JNI global reference to `obj` (0 for null), made by a thread no
/// collection can overtake meanwhile (a parked thread between two polls, or
/// a caller inside its foreign-thread entry).
fn global_ref_of(shared: &SharedVm, obj: Option<ObjectRef>) -> JObject {
    obj.map_or(0, |obj| shared.natives.jni_global_refs.lock().add(obj))
}

/// The calling thread's local reference to what `global` (a reference
/// [`global_ref_of`] made; 0: null) names, `global` deleted.
fn global_to_local(shared: &SharedVm, global: JObject) -> JObject {
    if global == 0 {
        return 0;
    }
    let _fx = jni::ForeignJniEntry::enter();
    let obj = shared.natives.jni_global_refs.lock().resolve(global);
    let prev = jni::replace_jni_context(shared);
    let handle = obj.map_or(0, jni::new_local_handle);
    jni::restore_jni_context(prev);
    let _ = shared.natives.jni_global_refs.lock().remove(global);
    handle
}

/// [`LocalRead`] `value` for the caller: an object's global reference made
/// its local one ([`global_to_local`]).
fn caller_value(shared: &SharedVm, value: LocalRead) -> LocalRead {
    match value {
        LocalRead::Object(global) => LocalRead::Object(global_to_local(shared, global)),
        other => other,
    }
}

/// A global reference to what the agent's `handle` names (0: null), for a
/// write on another thread; `INVALID_OBJECT` for a handle that names none.
fn local_to_global(shared: &SharedVm, handle: JObject) -> Result<JObject, JInt> {
    if handle == 0 {
        return Ok(0);
    }
    let _fx = jni::ForeignJniEntry::enter();
    let prev = jni::replace_jni_context(shared);
    let obj = jni::jobject_to_obj(handle);
    jni::restore_jni_context(prev);
    obj.map(|obj| shared.natives.jni_global_refs.lock().add(obj))
        .ok_or(ERR_INVALID_OBJECT)
}

/// Delete a global reference [`local_to_global`] made (0: none).
fn release_global(shared: &SharedVm, global: JObject) {
    if global != 0 {
        let _ = shared.natives.jni_global_refs.lock().remove(global);
    }
}

/// The monitors thread `tid` (another than the caller) owns, in
/// `GetOwnedMonitorStackDepthInfo`'s order and at its depths, each as a
/// JNI global reference ([`global_to_local`] makes it the caller's): read
/// through its window when it is blocked, else on the thread parked at a
/// suspend point, after a handshake of its own when it is not suspended.
fn other_monitors(shared: &SharedVm, tid: u64) -> Result<Vec<(JObject, JInt)>, JInt> {
    let window = |held: &crate::runtime::interpreter::BlockedFrames<'_>| {
        monitors_through_window(shared, tid, held)
    };
    if let Some(answer) = crate::runtime::interpreter::with_blocked_frames(shared, tid, &window) {
        return finish_monitors(tid, answer, "window", false);
    }
    // Wave 51 (lane L1): an unmounted virtual thread, read where it lies, as
    // its listing is (`other_thread_frames`); a handshake would wait in vain.
    if let Some(answer) = crate::runtime::interpreter::with_unmounted_frames(shared, tid, &window) {
        return finish_monitors(tid, answer, "unmounted", false);
    }
    let handshake = !shared.debug.debug_state.lock().is_thread_suspended(tid);
    if handshake {
        let mut ds = shared.debug.debug_state.lock();
        ds.begin_handshake(tid);
        crate::debug::publish_debugger_gates(shared, &ds);
    }
    let answer = on_stopped_thread(
        shared,
        tid,
        |shared: &SharedVm, thread: &mut JvmThread| monitors_on_parked(shared, thread),
        window,
    );
    if handshake {
        let mut ds = shared.debug.debug_state.lock();
        ds.end_handshake(tid);
        crate::debug::publish_debugger_gates(shared, &ds);
    }
    let (answer, via) = answer?;
    finish_monitors(tid, answer, via, handshake)
}

/// [`other_monitors`]' answer, traced.
fn finish_monitors(
    tid: u64,
    answer: Result<Vec<(JObject, JInt)>, JInt>,
    via: &str,
    handshake: bool,
) -> Result<Vec<(JObject, JInt)>, JInt> {
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[JVMTI_OTHER_MONITORS] tid={tid} count={} via={via} handshake={handshake}",
            answer
                .as_ref()
                .map_or(-1, |m| i64::try_from(m.len()).unwrap_or(-1))
        );
    }
    answer
}

/// The monitors `owned` in `GetOwnedMonitorStackDepthInfo`'s order, each at
/// the depth of the row whose interpreter frame locked it (`pairs`,
/// `held_monitors::frame_locked_monitors` of the frames, indexed from the
/// bottom), as global references.
fn monitor_rows_of(
    shared: &SharedVm,
    rows: &[OtherRow],
    owned: &[ObjectRef],
    pairs: Vec<(usize, ObjectRef)>,
) -> Vec<(JObject, JInt)> {
    let monitors = crate::debug::inspect::jvmti_owned_monitor_order(owned, pairs, |index| {
        rows.iter()
            .position(|row| row.raw.frame == Some(index))
            .and_then(|depth| JInt::try_from(depth).ok())
    });
    let mut refs = shared.natives.jni_global_refs.lock();
    monitors
        .into_iter()
        .map(|(obj, depth)| (refs.add(obj), depth))
        .collect()
}

/// The monitors thread `tid` owns: its lock stack less the monitor it waits
/// on and the one it is blocked entering (neither is owned), as the current
/// thread's are ([`get_owned_monitor_stack_depth_info`]).
fn owned_monitors(shared: &SharedVm, tid: u64) -> Result<Vec<ObjectRef>, JInt> {
    let (contended, waiting, locked, ..) = shared
        .threads
        .thread_registry
        .jmx_lock_snapshot(ThreadId(tid))
        .ok_or(ERR_INTERNAL)?;
    Ok(locked
        .into_iter()
        .filter(|&o| Some(o) != waiting && Some(o) != contended)
        .collect())
}

/// [`other_monitors`] on the parked thread itself, between two polls, its
/// depths places in its own listing ([`listed_rows`]), as the current
/// thread's are.
fn monitors_on_parked(
    shared: &SharedVm,
    thread: &mut JvmThread,
) -> Result<Vec<(JObject, JInt)>, JInt> {
    let rows = listed_rows(shared, thread, true);
    let owned = owned_monitors(shared, thread.thread_id.0)?;
    // SYNC619-3 (round 19 wave 7, lane gc7): this runs ON the parked thread,
    // so its compiled activations are the calling thread's own.
    let monitors = own_monitor_order(shared, &thread.frames, &rows, &owned);
    let mut refs = shared.natives.jni_global_refs.lock();
    Ok(monitors
        .into_iter()
        .map(|(obj, depth)| (refs.add(obj), depth))
        .collect())
}

/// [`other_monitors`] through blocked thread `tid`'s inspection window. The
/// frames' own monitor records are the addresses the monitors had when the
/// thread blocked, so the attribution the thread published as it blocked
/// (`ThreadRegistry::jmx_frame_monitors`, read from its lock stack, which
/// every collection updates) is taken first, as JDWP's
/// `OwnedMonitorsStackDepthInfo` takes it; the frames' records only when
/// every monitor they name is still one the thread owns at that address.
fn monitors_through_window(
    shared: &SharedVm,
    tid: u64,
    held: &crate::runtime::interpreter::BlockedFrames<'_>,
) -> Result<Vec<(JObject, JInt)>, JInt> {
    let rows = other_rows(shared, held.rows.clone());
    let _fx = jni::ForeignJniEntry::enter();
    let frames = held.frames();
    // An unmounted virtual thread (wave 51) may have no registry entry of
    // its own while no carrier runs it: the monitors its deposited frames
    // locked are then the ones it owns.
    let owned = match owned_monitors(shared, tid) {
        Err(_) if held.unmounted() => {
            crate::runtime::interpreter::held_monitors::frame_locked_monitors(frames)
                .into_iter()
                .map(|(_, obj)| obj)
                .collect()
        }
        other => other?,
    };
    // GC719-2 (JIT round 19 proposal wave 8, lane gc8): compiled frames on
    // the blocked thread's stack -- the published trace's attribution, rows
    // matched to its entries by name.
    if let Some(monitors) = window_compiled_monitor_rows(shared, tid, &rows, &owned, frames.len()) {
        return Ok(monitors);
    }
    let pairs = shared
        .threads
        .thread_registry
        .jmx_frame_monitors(ThreadId(tid), frames.len())
        .unwrap_or_else(|| {
            let pairs = crate::runtime::interpreter::held_monitors::frame_locked_monitors(frames);
            if pairs.iter().all(|(_, obj)| owned.contains(obj)) {
                pairs
            } else {
                Vec::new()
            }
        });
    Ok(monitor_rows_of(shared, &rows, &owned, pairs))
}

/// `CRATONVM_JVMTI_WINDOW_COMPILED_MONITOR_DEPTHS`, **default ON** (JIT round
/// 19 proposal wave 8, lane gc8, GC719-2; a HotSpot-parity fix, `=0` is the
/// kill switch): [`monitors_through_window`] of a thread whose published
/// trace interleaves compiled activations attributes through that trace
/// ([`window_compiled_monitor_rows`]). Off: by interpreter frame only, as in
/// wave 7 (a compiled activation's hold at depth -1). Read per call (cold).
fn window_compiled_monitor_depths_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JVMTI_WINDOW_COMPILED_MONITOR_DEPTHS")
}

/// GC719-2 (JIT round 19 proposal wave 8, lane gc8): the monitors blocked
/// thread `tid` owns (`owned`), each at the depth of the window row
/// (`rows`, top first) whose frame locked it, when the thread's published
/// trace interleaves compiled activations with its `frames_len` interpreter
/// frames.
///
/// The thread published, at the deposit it is blocked in, its trace
/// (`ThreadRegistry::frame_trace_of`, innermost first) and the attribution
/// indexed into that trace -- interpreter frames' monitors and, since wave 7
/// (SYNC619-2), the compiled activations' own holds
/// (`ThreadRegistry::jmx_frame_monitors` asked with the trace's length,
/// which it answers only for a publication of that length and an unchanged
/// lock stack). The window's listing is built from the same deposit's chain
/// copy, so the two describe one stack; [`window_rows_for_trace`] proves it
/// entry by entry (names, and at most a native heading the listing adds) and
/// maps each trace entry to its row. Any disagreement, a trace with no
/// compiled entry, the switch off, or no publication answers `None`, and the
/// caller keeps wave 7's interpreter-frame answer.
fn window_compiled_monitor_rows(
    shared: &SharedVm,
    tid: u64,
    rows: &[OtherRow],
    owned: &[ObjectRef],
    frames_len: usize,
) -> Option<Vec<(JObject, JInt)>> {
    use crate::runtime::interpreter::held_monitors as hm;
    if !hm::compiled_frame_monitor_depths_enabled() || !window_compiled_monitor_depths_enabled() {
        return None;
    }
    let registry = &shared.threads.thread_registry;
    let trace = registry.frame_trace_of(ThreadId(tid));
    // A trace one-to-one with the interpreter frames has no compiled entry,
    // and `jmx_frame_monitors` would answer its frame-indexed table.
    if trace.is_empty() || trace.len() == frames_len {
        return None;
    }
    let pairs = registry.jmx_frame_monitors(ThreadId(tid), trace.len())?;
    let names: Vec<Option<(String, Arc<str>)>> = rows
        .iter()
        .map(|row| row_class_and_method(shared, row))
        .collect();
    let row_of = window_rows_for_trace(&names, rows, &trace)?;
    let n = trace.len();
    let monitors = crate::debug::inspect::jvmti_owned_monitor_order(owned, pairs, |index| {
        // `index` counts the trace from the bottom; `row_of` from the top.
        let from_top = n.checked_sub(index + 1)?;
        JInt::try_from(*row_of.get(from_top)?).ok()
    });
    let mut refs = shared.natives.jni_global_refs.lock();
    Some(
        monitors
            .into_iter()
            .map(|(obj, depth)| (refs.add(obj), depth))
            .collect(),
    )
}

/// The `(class name, dotted; method name)` of a window row, or `None` when
/// the row names no method of a loaded class (a lambda proxy's row, an
/// obsolete method): [`window_rows_for_trace`] then refuses the whole match.
fn row_class_and_method(shared: &SharedVm, row: &OtherRow) -> Option<(String, Arc<str>)> {
    let (class_id, name, _) = method_or_proxy_method(shared, row.info.method)?;
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(class_id)?;
    Some((class.name.replace('/', "."), name))
}

/// GC719-2: the row (index into `rows`, top first) of each entry of `trace`
/// (innermost first), when the two list one stack: equal length, or the
/// listing one longer with a native method heading it (`location == -1`, no
/// interpreter frame), and every entry's class and method name equal to its
/// row's (`names`, [`row_class_and_method`] per row; class names compared
/// dotted). `None` on any disagreement -- never a shifted attribution.
fn window_rows_for_trace(
    names: &[Option<(String, Arc<str>)>],
    rows: &[OtherRow],
    trace: &[crate::native::registry::StackTraceEntry],
) -> Option<Vec<usize>> {
    let head = rows.len().checked_sub(trace.len())?;
    match head {
        0 => {}
        1 if rows[0].raw.location == -1 && rows[0].raw.frame.is_none() => {}
        _ => return None,
    }
    let mut out = Vec::with_capacity(trace.len());
    for (k, entry) in trace.iter().enumerate() {
        let at = k + head;
        let (class, method) = names.get(at)?.as_ref()?;
        if **method != *entry.method_name || *class != entry.class_name.replace('/', ".") {
            return None;
        }
        out.push(at);
    }
    Some(out)
}

/// Write `monitors` (global references and depths, [`other_monitors`]) to
/// `GetOwnedMonitorStackDepthInfo`'s out-parameters as the caller's local
/// references, in an `Allocate`d block.
fn write_monitor_rows(
    shared: &SharedVm,
    monitors: &[(JObject, JInt)],
    count_ptr: *mut JInt,
    info_ptr: *mut *mut MonitorDepthRow,
) -> JInt {
    let Ok(count) = JInt::try_from(monitors.len()) else {
        monitors
            .iter()
            .for_each(|&(global, _)| release_global(shared, global));
        return ERR_INTERNAL;
    };
    let Some(size) = monitors
        .len()
        .checked_mul(std::mem::size_of::<MonitorDepthRow>())
    else {
        monitors
            .iter()
            .for_each(|&(global, _)| release_global(shared, global));
        return ERR_OUT_OF_MEMORY;
    };
    let block = allocate_block(size).cast::<MonitorDepthRow>();
    if block.is_null() {
        monitors
            .iter()
            .for_each(|&(global, _)| release_global(shared, global));
        return ERR_OUT_OF_MEMORY;
    }
    for (i, &(global, depth)) in monitors.iter().enumerate() {
        let row = MonitorDepthRow {
            monitor: global_to_local(shared, global),
            stack_depth: depth,
        };
        // SAFETY: `block` holds `monitors.len()` rows (`allocate_block` of
        // their size, 16-aligned).
        unsafe { block.add(i).write(row) };
    }
    // SAFETY: the agent's non-null out-parameters.
    unsafe {
        *count_ptr = count;
        *info_ptr = block;
    }
    ERR_NONE
}

/// Slot 88: `GetVersionNumber(env, version_ptr)`.
extern "C" fn get_version_number(env: *mut JvmtiNativeEnv, version_ptr: *mut JInt) -> JInt {
    if let Err(code) = live_env(env) {
        return code;
    }
    if version_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *version_ptr = JVMTI_VERSION_25 };
    ERR_NONE
}

/// Write a capability set to the agent's `jvmtiCapabilities*`.
fn write_capabilities(out: *mut [u32; CAPABILITY_WORDS], caps: [u32; CAPABILITY_WORDS]) -> JInt {
    if out.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null, 16-byte `jvmtiCapabilities`.
    unsafe { out.write_unaligned(caps) };
    ERR_NONE
}

/// Slot 89: `GetCapabilities(env, capabilities_ptr)`.
extern "C" fn get_capabilities(
    env: *mut JvmtiNativeEnv,
    out: *mut [u32; CAPABILITY_WORDS],
) -> JInt {
    match live_env(env) {
        Ok(env) => write_capabilities(out, env.state.lock().capabilities),
        Err(code) => code,
    }
}

/// Slot 140: `GetPotentialCapabilities(env, capabilities_ptr)` (OnLoad or
/// live phase).
extern "C" fn get_potential_capabilities(
    env: *mut JvmtiNativeEnv,
    out: *mut [u32; CAPABILITY_WORDS],
) -> JInt {
    match live_env(env) {
        Ok(env) if env.outside_live_phase() => ERR_WRONG_PHASE,
        Ok(env) => write_capabilities(out, env.potential()),
        Err(code) => code,
    }
}

/// Read the agent's `const jvmtiCapabilities*`.
fn read_capabilities(caps: *const [u32; CAPABILITY_WORDS]) -> Option<[u32; CAPABILITY_WORDS]> {
    if caps.is_null() {
        return None;
    }
    // SAFETY: the agent's non-null, 16-byte `jvmtiCapabilities`.
    Some(unsafe { caps.read_unaligned() })
}

/// Slot 142: `AddCapabilities(env, capabilities_ptr)`: all or nothing;
/// `JVMTI_ERROR_NOT_AVAILABLE` when one asked for is not potentially
/// available. OnLoad or live phase.
extern "C" fn add_capabilities(
    env: *mut JvmtiNativeEnv,
    caps: *const [u32; CAPABILITY_WORDS],
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    if env.outside_live_phase() {
        return ERR_WRONG_PHASE;
    }
    let Some(requested) = read_capabilities(caps) else {
        return ERR_NULL_POINTER;
    };
    if requested
        .iter()
        .zip(env.potential())
        .any(|(&r, p)| r & !p != 0)
    {
        return ERR_NOT_AVAILABLE;
    }
    {
        let mut state = env.state.lock();
        for (have, r) in state.capabilities.iter_mut().zip(requested) {
            *have |= r;
        }
    }
    // t1 wave 3, lane M: a live-phase `can_tag_objects` makes every LATER
    // compile of this VM keep its allocations (`DebuggerGates::c_agent_walks_heap`).
    // An OnLoad-phase env is bound to its VM later (`bind_startup_envs`
    // reads its capabilities then), so `vm()` may be `None` here.
    if has_cap(&requested, CAP_TAG_OBJECTS) {
        if let Some(shared) = env.vm() {
            note_live_heap_walker(&shared);
        }
    }
    ERR_NONE
}

/// t1 wave 5, lane M (`t1-V3-in-flight-compile-keeps-scalar-replacement-after-
/// live-can-tag-objects`): a live-phase env of `shared` acquired
/// `can_tag_objects`. Raise the gate the compile doors read
/// (`DebuggerGates::c_agent_walks_heap`), then, the first time only, refuse
/// every compile already in flight
/// (`JitCache::refuse_in_flight_for_object_observer`,
/// `CRATONVM_JIT_OBSERVER_GAIN_REFUSES_IN_FLIGHT`): its request may have read
/// the gate down and scalar-replaced allocations a heap walk must see.
/// HotSpot's `ciEnv::jvmti_state_changed`. The order matters: a compile that
/// begins after the record (its witness stamps a later invalidation epoch)
/// reads the raised gate. Called with no lock of this env held. Returns
/// whether the record was logged.
fn note_live_heap_walker(shared: &SharedVm) -> bool {
    if !shared.debug.debugger_gates.note_c_agent_walks_heap() {
        return false;
    }
    shared.jit.jit_cache.read().refuse_in_flight_for_object_observer()
}

/// Slot 143: `RelinquishCapabilities(env, capabilities_ptr)` (OnLoad or
/// live phase). Giving up the capability of an event delivered through the
/// VM's manager ([`MANAGER_EVENTS`]: `can_generate_breakpoint_events`,
/// `can_generate_exception_events`, and since wave 17 the method entry and
/// exit ones) disables that event (for every thread), on the VM too (wave
/// 14).
extern "C" fn relinquish_capabilities(
    env: *mut JvmtiNativeEnv,
    caps: *const [u32; CAPABILITY_WORDS],
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    if env.outside_live_phase() {
        return ERR_WRONG_PHASE;
    }
    let Some(released) = read_capabilities(caps) else {
        return ERR_NULL_POINTER;
    };
    {
        let mut state = env.state.lock();
        for (have, r) in state.capabilities.iter_mut().zip(released) {
            *have &= !r;
        }
        for (event, capability) in MANAGER_EVENTS {
            if state.capabilities[0] & capability == 0 {
                state.disable_everywhere(event);
            }
        }
    }
    if let Some(shared) = env.vm() {
        for (event, _) in MANAGER_EVENTS {
            let _ = env.sync_event(&shared, event);
        }
    }
    ERR_NONE
}

/// Slot 127: `DisposeEnvironment(env)`: clears the breakpoints this env set,
/// unregisters its delivery, leaves its VM's list of native envs, and marks
/// it disposed (never freed; see the module doc).
extern "C" fn dispose_environment(env: *mut JvmtiNativeEnv) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    // Serialised with `sync_event`, which then finds the env disposed and
    // its delivery gone.
    let _serial = env.sync.lock();
    let (vm, delivery, breakpoints) = {
        let mut state = env.state.lock();
        state.disposed = true;
        state.enabled = 0;
        state.thread_enabled.clear();
        state.capabilities = [0; CAPABILITY_WORDS];
        (
            state.vm.as_ref().and_then(Weak::upgrade),
            state.delivery.take(),
            std::mem::take(&mut state.breakpoints),
        )
    };
    if let Some(shared) = vm.clone() {
        for (method, location) in breakpoints {
            let _ = rt::clear_breakpoint_for_vm(&shared, method, location);
        }
        let addr = env.addr();
        shared
            .debug
            .jvmti_env
            .lock()
            .c_table
            .native_envs
            .retain(|&listed| listed != addr);
        refresh_class_prepare_gate(&shared);
        events_more::refresh_event_gates(&shared);
    }
    // Detached, its enable leaves the manager's union flags (wave 14).
    if let Some((manager, delivery)) = delivery {
        let _ = manager.unregister_env(&delivery);
    }
    // And the native-call funnel's gate (wave 27), read after the detach.
    if let Some(shared) = vm {
        refresh_method_events_gate(&shared);
    }
    ERR_NONE
}

/// Slot 128: `GetErrorName(env, error, name_ptr)`.
extern "C" fn get_error_name(
    env: *mut JvmtiNativeEnv,
    error: JInt,
    name_ptr: *mut *mut u8,
) -> JInt {
    if let Err(code) = live_env(env) {
        return code;
    }
    if name_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let known = rt::JvmtiError::from_code(error);
    let name = if known as JInt == error {
        known.name()
    } else {
        match jdk25_only_error_name(error) {
            Some(name) => name,
            None => return ERR_ILLEGAL_ARGUMENT,
        }
    };
    let mem = allocate_modified_utf8(name);
    if mem.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *name_ptr = mem };
    ERR_NONE
}

/// The `jvmtiError` names of JDK 25's `jvmti.h` that `runtime::jvmti::JvmtiError`
/// does not list (interpreter round i2 wave 57, lane L1: `GetErrorName`
/// refused them with `JVMTI_ERROR_ILLEGAL_ARGUMENT`; MEASURED,
/// `L1W57JvmtiErrorNames`, HotSpot 25.0.3 names every one).
fn jdk25_only_error_name(error: JInt) -> Option<&'static str> {
    Some(match error {
        26 => "JVMTI_ERROR_INVALID_MODULE",
        72 => "JVMTI_ERROR_UNSUPPORTED_REDEFINITION_CLASS_ATTRIBUTE_CHANGED",
        73 => "JVMTI_ERROR_UNSUPPORTED_OPERATION",
        79 => "JVMTI_ERROR_UNMODIFIABLE_CLASS",
        80 => "JVMTI_ERROR_UNMODIFIABLE_MODULE",
        102 => "JVMTI_ERROR_INVALID_EVENT_TYPE",
        104 => "JVMTI_ERROR_NATIVE_METHOD",
        106 => "JVMTI_ERROR_CLASS_LOADER_UNSUPPORTED",
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// What agents call first: phase, env storage, raw monitors, thread, thread
// group, class and field facts (interpreter round i1 wave 52, lane L1)
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/jvmti-delivery-threading.md`.
// Probe: `tools/probes/interp/L1/L1W52JvmtiThreadAndClassFacts.java`.

/// `jvmtiPhase` (`GetPhase`); the VM's own phases are `crate::debug`'s
/// `JVMTI_PHASE_*` (wave 55).
const PHASE_ONLOAD: JInt = 1;
const PHASE_DEAD: JInt = 8;

/// Slot 133: `GetPhase(env, phase_ptr)`: OnLoad while a startup agent's env
/// waits for its VM, then the VM's phase (`DebuggerGates::jvmti_phase`:
/// primordial until `System.initPhase1` returns, start until `VMInit`, live
/// after it, dead after `VMDeath`), dead once the VM is gone.
extern "C" fn get_phase(env: *mut JvmtiNativeEnv, phase_ptr: *mut JInt) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    if phase_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // Wave 55: the VM's one phase value (primordial and dead only with
    // `DebuggerGates::c_jvmti_phases`).
    let phase = if env.state.lock().startup_pending {
        PHASE_ONLOAD
    } else {
        env.vm().map_or(PHASE_DEAD, |shared| {
            JInt::from(shared.debug.debugger_gates.jvmti_phase())
        })
    };
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *phase_ptr = phase };
    ERR_NONE
}

/// Slot 147: `GetEnvironmentLocalStorage(env, data_ptr)`: the pointer
/// [`set_environment_local_storage`] stored, NULL before any.
extern "C" fn get_environment_local_storage(
    env: *mut JvmtiNativeEnv,
    data_ptr: *mut usize,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    if data_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let data = env.state.lock().local_storage;
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *data_ptr = data };
    ERR_NONE
}

/// Slot 148: `SetEnvironmentLocalStorage(env, data)`: any phase.
extern "C" fn set_environment_local_storage(env: *mut JvmtiNativeEnv, data: usize) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    env.state.lock().local_storage = data;
    ERR_NONE
}

/// The VM thread a `jthread` names for the thread-local storage: NULL is
/// the calling thread (`JVMTI_ERROR_UNATTACHED_THREAD` for one the VM does
/// not know); another must be alive (`JVMTI_ERROR_THREAD_NOT_ALIVE`).
///
/// Interpreter round i4 wave 95 (lane L1): `jvmti.xml` allows both storage
/// functions in the start phase (`phase="start"`), so the phase check is
/// [`JvmtiNativeEnv::start_or_live_vm`]'s, not the live phase's (they
/// answered `JVMTI_ERROR_WRONG_PHASE` to an agent's `VMStart` callback).
/// `null_out`: the caller's out-parameter is NULL; HotSpot's `jvmtiEnter`
/// wrapper answers `JVMTI_ERROR_NULL_POINTER` after the phase and before the
/// thread is looked at.
fn storage_thread(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    null_out: bool,
) -> Result<(&'static JvmtiNativeEnv, u64), JInt> {
    let env = live_env(env)?;
    let shared = env.start_or_live_vm().ok_or(ERR_WRONG_PHASE)?;
    if null_out {
        return Err(ERR_NULL_POINTER);
    }
    let tid = if thread == 0 {
        let current = jni::current_jvm_thread_of(&shared).ok_or(ERR_UNATTACHED_THREAD)?;
        // SAFETY: the calling thread's own `JvmThread`; `thread_id` is `Copy`.
        unsafe { (*current).thread_id.0 }
    } else {
        thread_id_of(&shared, thread)?
    };
    Ok((env, tid))
}

/// Slot 102: `GetThreadLocalStorage(env, thread, data_ptr)` (wave 52): the
/// pointer [`set_thread_local_storage`] stored for the thread, NULL before
/// any.
extern "C" fn get_thread_local_storage(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    data_ptr: *mut usize,
) -> JInt {
    let (env, tid) = match storage_thread(env, thread, data_ptr.is_null()) {
        Ok(found) => found,
        Err(code) => return code,
    };
    let data = env
        .state
        .lock()
        .thread_storage
        .iter()
        .find(|&&(t, _)| t == tid)
        .map_or(0, |&(_, data)| data);
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *data_ptr = data };
    ERR_NONE
}

/// Slot 103: `SetThreadLocalStorage(env, thread, data)` (wave 52). The
/// pointer is the agent's; a thread's entry stays until replaced (a VM
/// thread id is not reused).
extern "C" fn set_thread_local_storage(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    data: usize,
) -> JInt {
    let (env, tid) = match storage_thread(env, thread, false) {
        Ok(found) => found,
        Err(code) => return code,
    };
    let mut state = env.state.lock();
    state.thread_storage.retain(|&(t, _)| t != tid);
    if data != 0 {
        state.thread_storage.push((tid, data));
    }
    ERR_NONE
}

/// `can_get_bytecodes`, the 4th bit field (wave 52: `GetBytecodes`).
const CAN_GET_BYTECODES: u32 = 1 << 3;

/// Slot 75: `GetBytecodes(env, method, bytecode_count_ptr, bytecodes_ptr)`
/// (wave 52; `can_get_bytecodes`): the method's code array as the class
/// file has it, in an `Allocate` block; `JVMTI_ERROR_NATIVE_METHOD` for a
/// native method.
extern "C" fn get_bytecodes(
    env: *mut JvmtiNativeEnv,
    method: u64,
    bytecode_count_ptr: *mut JInt,
    bytecodes_ptr: *mut *mut u8,
) -> JInt {
    let shared = match live_env(env) {
        Ok(e) if !e.possesses(CAN_GET_BYTECODES) => return ERR_MUST_POSSESS_CAPABILITY,
        Ok(e) => match e.start_or_live_vm() {
            Some(shared) => shared,
            None => return ERR_WRONG_PHASE,
        },
        Err(code) => return code,
    };
    let code = with_method(&shared, method, |_, m| {
        if m.is_native() {
            Err(ERR_NATIVE_METHOD)
        } else {
            Ok(m.code().map_or_else(Vec::new, |code| code.code.to_vec()))
        }
    });
    // Wave 56: a lambda proxy's method; wave 57: its spun code
    // ([`proxy_method_code`]).
    let code = code.or_else(|| proxy_method_code(&shared, method));
    let code = match code {
        Some(Ok(code)) => code,
        Some(Err(code)) => return code,
        None => return ERR_INVALID_METHODID,
    };
    write_id_array(bytecode_count_ptr, bytecodes_ptr, &code)
}

/// Slot 129: `GetJLocationFormat(env, format_ptr)`: `JVMTI_JLOCATION_JVMBCI`
/// (1), a `jlocation` being a bytecode index.
extern "C" fn get_jlocation_format(env: *mut JvmtiNativeEnv, format_ptr: *mut JInt) -> JInt {
    if let Err(code) = live_env(env) {
        return code;
    }
    if format_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *format_ptr = 1 };
    ERR_NONE
}

/// Slot 139: `GetTime(env, nanos_ptr)`: HotSpot's `os::javaTimeNanos`, the
/// clock `System.nanoTime` reads, so the two readings compare.
extern "C" fn get_time(env: *mut JvmtiNativeEnv, nanos_ptr: *mut i64) -> JInt {
    if let Err(code) = live_env(env) {
        return code;
    }
    if nanos_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let nanos = cratonvm_native_builtins::lang_system::java_time_nanos();
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *nanos_ptr = nanos };
    ERR_NONE
}

/// Slot 144: `GetAvailableProcessors(env, processor_count_ptr)`.
extern "C" fn get_available_processors(env: *mut JvmtiNativeEnv, count_ptr: *mut JInt) -> JInt {
    if let Err(code) = live_env(env) {
        return code;
    }
    if count_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let count = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *count_ptr = JInt::try_from(count).unwrap_or(JInt::MAX) };
    ERR_NONE
}

// ---------------------------------------------------------------------------
// What agents call at startup (interpreter round i2 wave 61, lane L1)
// ---------------------------------------------------------------------------
//
// The VM system properties (130-132), the class search additions (149 /
// 151), `GetObjectHashCode` (58), `GetObjectSize` (154) and `GetTimerInfo`
// (138), which answered `JVMTI_ERROR_NOT_AVAILABLE` until wave 61
// (`docs/feature-designs/jvmti-delivery-threading.md`).
// MEASURED on HotSpot 25.0.3 with the Rust port of the shim
// (`tools/probes/interp/L1/L1W61JvmtiAgentStartup.java`). No switch: each
// slot answered 98.

// The keys of HotSpot's VM property list (`Arguments::system_properties()`,
// its readable entries) besides the `-D` ones, which `SetSystemProperty` may
// write in the OnLoad phase, and which `-D` ones are internal: one table,
// `cratonvm_types::vm_properties` (interpreter round i4 wave 95, lane L1;
// stage 1 of
// `docs/feature-designs/jdk-only-mode.md`;
// private constants of this file until then). `user.dir`, `os.name`,
// `java.version` and the rest of `System.getProperties()` come from the class
// library, not the VM, and answer `JVMTI_ERROR_NOT_AVAILABLE`.
use cratonvm_types::vm_properties::{
    is_internal_property as internal_property, is_writeable_vm_property, vm_property_keys,
};

/// The VM properties a startup agent reads in `Agent_OnLoad` (wave 61),
/// before `SharedVm::new` builds the property map: the keys of
/// `vm_properties::VM_PROPERTIES` HotSpot has by then, with the initial values that map
/// gives them (one copy each, `vm::vm_init::vm_identity_properties` and the
/// `initial_*` functions), then the `-D` properties; each with whether
/// `SetSystemProperty` may write it.
pub(crate) fn onload_vm_properties(
    config: &crate::config::VmConfig,
) -> Vec<(String, String, bool)> {
    use crate::vm::vm_init as init;
    let mut out: Vec<(String, String, bool)> = Vec::new();
    let mut put = |key: &str, value: String| {
        let writable = is_writeable_vm_property(key);
        out.retain(|(k, _, _)| k != key);
        out.push((key.to_string(), value, writable));
    };
    for (key, value) in init::vm_identity_properties(config) {
        put(key, value);
    }
    let java_home = init::initial_java_home(config);
    put("java.class.path", init::initial_java_class_path(config));
    put(
        "java.library.path",
        init::initial_java_library_path(&java_home),
    );
    put(
        "sun.boot.library.path",
        init::initial_sun_boot_library_path(&java_home),
    );
    put("java.home", java_home.clone());
    for (key, value) in &config.system_properties {
        if !internal_property(key) {
            let value = init::command_line_property_value(&java_home, key, value);
            out.retain(|(k, _, _)| k != key);
            out.push((key.clone(), value, true));
        }
    }
    // Interpreter round i7 wave 135, lane L1b (`CRATONVM_VM_PROPERTY_LIST_ORDER`,
    // parity, default ON, `=0` off): HotSpot's list order, measured; a `-D`
    // of a list key goes back to the key's place (the stable sort keeps the
    // other `-D` ones in command-line order after the list).
    if config.vm_property_list_order {
        out.sort_by_key(|(k, _, _)| cratonvm_types::vm_properties::hotspot_list_sort_key(k));
    }
    out
}

/// What a property function acts on: the OnLoad phase's properties
/// ([`StartupScope::properties`]) or the live VM's map.
enum PropertyPhase {
    OnLoad,
    Live(Arc<SharedVm>),
}

/// The phase of a property or class-search call: OnLoad (a startup env not
/// bound yet) or live; any other is `JVMTI_ERROR_WRONG_PHASE`.
fn property_phase(env: *mut JvmtiNativeEnv) -> Result<PropertyPhase, JInt> {
    let env = live_env(env)?;
    if env.state.lock().startup_pending {
        return Ok(PropertyPhase::OnLoad);
    }
    if env.outside_live_phase() {
        return Err(ERR_WRONG_PHASE);
    }
    env.vm().map(PropertyPhase::Live).ok_or(ERR_WRONG_PHASE)
}

/// The VM properties readable in the live phase, in HotSpot's list order
/// (`vm_properties::VM_PROPERTIES`, then the `-D` ones), with their values now.
fn live_vm_properties(shared: &SharedVm) -> Vec<(String, String)> {
    let props = shared.system_properties.read();
    let mut out: Vec<(String, String)> = Vec::new();
    let user = shared
        .config
        .system_properties
        .iter()
        .map(|(k, _)| k.as_str())
        .filter(|k| !internal_property(k));
    let mut keys: Vec<&str> = Vec::new();
    for key in vm_property_keys() {
        keys.push(key);
    }
    keys.extend(user);
    for key in keys {
        if out.iter().any(|(k, _)| k == key) {
            continue;
        }
        if let Some(value) = props.get(key) {
            out.push((key.to_string(), value.clone()));
        }
    }
    // Wave 135, lane L1b (`CRATONVM_VM_PROPERTY_LIST_ORDER`): HotSpot's
    // order -- the before-heap list, the `-D` ones, then
    // `java.vm.compressedOopsMode`.
    if shared.config.vm_property_list_order {
        out.sort_by_key(|(k, _)| cratonvm_types::vm_properties::hotspot_list_sort_key(k));
    }
    out
}

/// The VM properties of `phase`, as [`live_vm_properties`] lists them.
fn vm_properties_of(phase: &PropertyPhase) -> Vec<(String, String)> {
    match phase {
        PropertyPhase::Live(shared) => live_vm_properties(shared),
        PropertyPhase::OnLoad => STARTUP_ENVS.with(|c| {
            c.borrow()
                .as_ref()
                .map(|scope| {
                    scope
                        .properties
                        .iter()
                        .map(|(k, v, _)| (k.clone(), v.clone()))
                        .collect()
                })
                .unwrap_or_default()
        }),
    }
}

/// `text` as a NUL-terminated platform string (UTF-8) in an `Allocate`
/// block, or null when out of memory: the VM properties are platform
/// strings, not modified UTF-8.
fn allocate_c_string(text: &str) -> *mut u8 {
    let mem = allocate_block(text.len() + 1);
    if !mem.is_null() {
        // SAFETY: `mem` is a fresh block of `text.len() + 1` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(text.as_ptr(), mem, text.len());
            mem.add(text.len()).write(0);
        }
    }
    mem
}

/// The platform string at `ptr`, which is not NULL.
///
/// # Safety
///
/// `ptr` points at a NUL-terminated string the agent passed.
unsafe fn agent_c_string(ptr: *const std::ffi::c_char) -> String {
    std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
}

/// Slot 130: `GetSystemProperties(env, count_ptr, property_ptr)` (wave 61;
/// OnLoad and live phases): the keys of the VM's property list, each an
/// `Allocate` block, in one more.
extern "C" fn get_system_properties(
    env: *mut JvmtiNativeEnv,
    count_ptr: *mut JInt,
    property_ptr: *mut *mut *mut u8,
) -> JInt {
    let phase = match property_phase(env) {
        Ok(phase) => phase,
        Err(code) => return code,
    };
    if count_ptr.is_null() || property_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let keys = vm_properties_of(&phase);
    let array =
        allocate_block(keys.len().max(1) * std::mem::size_of::<*mut u8>()).cast::<*mut u8>();
    if array.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    for (at, (key, _)) in keys.iter().enumerate() {
        let block = allocate_c_string(key);
        if block.is_null() {
            for done in 0..at {
                // SAFETY: each block below `at` was allocated above.
                unsafe { free_block(*array.add(done)) };
            }
            // SAFETY: allocated above.
            unsafe { free_block(array.cast()) };
            return ERR_OUT_OF_MEMORY;
        }
        // SAFETY: `array` has room for `keys.len()` pointers.
        unsafe { *array.add(at) = block };
    }
    // SAFETY: the agent's non-null out-parameters.
    unsafe {
        *count_ptr = JInt::try_from(keys.len()).unwrap_or(JInt::MAX);
        *property_ptr = array;
    }
    ERR_NONE
}

/// Slot 131: `GetSystemProperty(env, property, value_ptr)` (wave 61; OnLoad
/// and live phases): the value of a VM property, an `Allocate` block;
/// `JVMTI_ERROR_NOT_AVAILABLE` for a key the VM does not list (MEASURED:
/// `user.dir`, `os.name`, `java.version`).
extern "C" fn get_system_property(
    env: *mut JvmtiNativeEnv,
    property: *const std::ffi::c_char,
    value_ptr: *mut *mut u8,
) -> JInt {
    let phase = match property_phase(env) {
        Ok(phase) => phase,
        Err(code) => return code,
    };
    if property.is_null() || value_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null string.
    let key = unsafe { agent_c_string(property) };
    let Some((_, value)) = vm_properties_of(&phase)
        .into_iter()
        .find(|(k, _)| *k == key)
    else {
        return ERR_NOT_AVAILABLE;
    };
    let block = allocate_c_string(&value);
    if block.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *value_ptr = block };
    ERR_NONE
}

/// Slot 132: `SetSystemProperty(env, property, value_ptr)` (wave 61; the
/// OnLoad phase only, MEASURED: the live phase answers `WRONG_PHASE` before
/// it reads its arguments): a writable VM property
/// (`vm_properties::is_writeable_vm_property`, or a `-D` one) takes `value_ptr` (NULL: unchanged), and the VM starts
/// with it ([`OnLoadEffects`]); any other key is `JVMTI_ERROR_NOT_AVAILABLE`.
extern "C" fn set_system_property(
    env: *mut JvmtiNativeEnv,
    property: *const std::ffi::c_char,
    value_ptr: *const std::ffi::c_char,
) -> JInt {
    match property_phase(env) {
        Ok(PropertyPhase::OnLoad) => {}
        Ok(PropertyPhase::Live(_)) => return ERR_WRONG_PHASE,
        Err(code) => return code,
    }
    if property.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null strings.
    let key = unsafe { agent_c_string(property) };
    let value = (!value_ptr.is_null()).then(|| unsafe { agent_c_string(value_ptr) });
    STARTUP_ENVS.with(|c| {
        let mut scope = c.borrow_mut();
        let Some(scope) = scope.as_mut() else {
            return ERR_NOT_AVAILABLE;
        };
        let Some(entry) = scope.properties.iter_mut().find(|(k, _, _)| *k == key) else {
            return ERR_NOT_AVAILABLE;
        };
        if !entry.2 {
            return ERR_NOT_AVAILABLE;
        }
        if let Some(value) = value {
            entry.1 = value.clone();
            scope.effects.properties.push((key, value));
        }
        ERR_NONE
    })
}

/// Slot 138: `GetTimerInfo(env, info_ptr)` (wave 61; any phase): the timer of
/// `GetTime`, `System.nanoTime`'s (MEASURED: `max=-1 forward=0 backward=0
/// kind=32`, `JVMTI_TIMER_ELAPSED`).
extern "C" fn get_timer_info(env: *mut JvmtiNativeEnv, info_ptr: *mut TimerInfoRow) -> JInt {
    if let Err(code) = live_env(env) {
        return code;
    }
    if info_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe {
        info_ptr.write(TimerInfoRow {
            max_value: -1,
            may_skip_forward: 0,
            may_skip_backward: 0,
            kind: TIMER_ELAPSED,
            reserved1: 0,
            reserved2: 0,
        });
    }
    ERR_NONE
}

/// `JVMTI_TIMER_ELAPSED`: elapsed time.
const TIMER_ELAPSED: JInt = 32;

/// The object `object` names, read inside the calling thread's
/// foreign-thread entry, then `work` with a native context on the calling
/// thread (no collection can move it meanwhile: the caller is a registered
/// mutator inside a JNI call). The start or live phase.
/// `JVMTI_ERROR_INVALID_OBJECT` for NULL or a handle that names nothing.
fn with_object<R>(
    env: *mut JvmtiNativeEnv,
    object: JObject,
    out_is_null: bool,
    work: impl FnOnce(&mut crate::vm::vm_exec::NativeContextImpl<'_>, ObjectRef) -> R,
) -> Result<R, JInt> {
    // Wave 94 (i54 stage 2): both callers are `phase="start"` functions.
    let shared = live_env(env)?.start_or_live_vm().ok_or(ERR_WRONG_PHASE)?;
    if object == 0 {
        return Err(ERR_INVALID_OBJECT);
    }
    if out_is_null {
        return Err(ERR_NULL_POINTER);
    }
    let current = jni::current_jvm_thread_of(&shared).ok_or(ERR_UNATTACHED_THREAD)?;
    let _fx = jni::ForeignJniEntry::enter();
    let prev = jni::replace_jni_context(&shared);
    let obj = jni::jobject_to_obj(object);
    jni::restore_jni_context(prev);
    let obj = obj.ok_or(ERR_INVALID_OBJECT)?;
    let mut ctx = crate::vm::vm_exec::NativeContextImpl {
        shared: &shared,
        // SAFETY: the calling thread's own, live `JvmThread`, which this call
        // runs on and does not otherwise touch meanwhile.
        thread: unsafe { &mut *current },
    };
    Ok(work(&mut ctx, obj))
}

/// Slot 58: `GetObjectHashCode(env, object, hash_code_ptr)` (wave 61): the
/// object's identity hash, `System.identityHashCode`'s (MEASURED: the same
/// value, and not a `String`'s `hashCode()`).
extern "C" fn get_object_hash_code(
    env: *mut JvmtiNativeEnv,
    object: JObject,
    hash_code_ptr: *mut JInt,
) -> JInt {
    let hash = with_object(env, object, hash_code_ptr.is_null(), |ctx, obj| {
        ctx.identity_hash_code(obj)
    });
    match hash {
        Ok(hash) => {
            // SAFETY: the agent's non-null out-parameter.
            unsafe { *hash_code_ptr = hash };
            ERR_NONE
        }
        Err(code) => code,
    }
}

/// Slot 154: `GetObjectSize(env, object, size_ptr)` (wave 61):
/// `Instrumentation.getObjectSize`'s answer
/// (`runtime::instrument::object_size_of`, which `libinstrument` asks this
/// function for on HotSpot).
extern "C" fn get_object_size(
    env: *mut JvmtiNativeEnv,
    object: JObject,
    size_ptr: *mut i64,
) -> JInt {
    let size = with_object(env, object, size_ptr.is_null(), |ctx, obj| {
        crate::runtime::instrument::object_size_of(&*ctx, obj)
    });
    match size {
        Ok(size) => {
            // SAFETY: the agent's non-null out-parameter.
            unsafe { *size_ptr = size };
            ERR_NONE
        }
        Err(code) => code,
    }
}

/// Is the file at `path` a zip archive, as HotSpot's
/// `ClassLoader::create_class_path_zip_entry` asks in the live phase (a
/// missing path, a directory and a plain file are refused: MEASURED,
/// `ILLEGAL_ARGUMENT` for each)? Its local-header or empty-archive
/// signature.
fn is_zip_file(path: &str) -> bool {
    use std::io::Read as _;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return false;
    }
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).is_ok() && (magic == *b"PK\x03\x04" || magic == *b"PK\x05\x06")
}

/// Slots 149 / 151: `AddToBootstrapClassLoaderSearch` /
/// `AddToSystemClassLoaderSearch(env, segment)` (wave 61). In the OnLoad
/// phase the segment is taken as given, as HotSpot takes it: appended to the
/// bootstrap search, or to `java.class.path` and the application class path,
/// when the VM binds the env ([`OnLoadEffects`]). In the live phase it must
/// be a jar (`ILLEGAL_ARGUMENT` otherwise) and is appended at once, as
/// `Instrumentation.appendTo*ClassLoaderSearch` appends it
/// (`runtime::instrument`'s `appendToClassLoaderSearch0`).
fn add_to_class_loader_search(
    env: *mut JvmtiNativeEnv,
    segment: *const std::ffi::c_char,
    boot: bool,
) -> JInt {
    let phase = match property_phase(env) {
        Ok(phase) => phase,
        Err(code) => return code,
    };
    if segment.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null string.
    let path = unsafe { agent_c_string(segment) };
    match phase {
        PropertyPhase::OnLoad => STARTUP_ENVS.with(|c| {
            let mut scope = c.borrow_mut();
            let Some(scope) = scope.as_mut() else {
                return ERR_WRONG_PHASE;
            };
            if boot {
                scope.effects.boot_segments.push(path);
                return ERR_NONE;
            }
            let separator = if cfg!(windows) { ';' } else { ':' };
            if let Some(entry) = scope
                .properties
                .iter_mut()
                .find(|(k, _, _)| k == "java.class.path")
            {
                if !entry.1.is_empty() {
                    entry.1.push(separator);
                }
                entry.1.push_str(&path);
                scope
                    .effects
                    .properties
                    .push(("java.class.path".to_string(), entry.1.clone()));
            }
            scope.effects.system_segments.push(path);
            ERR_NONE
        }),
        PropertyPhase::Live(shared) => {
            if !is_zip_file(&path) {
                return ERR_ILLEGAL_ARGUMENT;
            }
            let segments = [path];
            let mut cm = shared.classes.class_manager.write();
            if boot {
                cm.extend_bootstrap_classpath(&segments);
            } else {
                cm.extend_application_classpath(&segments);
            }
            ERR_NONE
        }
    }
}

extern "C" fn add_to_bootstrap_class_loader_search(
    env: *mut JvmtiNativeEnv,
    segment: *const std::ffi::c_char,
) -> JInt {
    add_to_class_loader_search(env, segment, true)
}

extern "C" fn add_to_system_class_loader_search(
    env: *mut JvmtiNativeEnv,
    segment: *const std::ffi::c_char,
) -> JInt {
    add_to_class_loader_search(env, segment, false)
}

/// Apply what the startup agents did in `Agent_OnLoad` to `shared`, which
/// now exists ([`OnLoadEffects`], from [`bind_startup_envs`]; wave 61).
fn apply_onload_effects(shared: &SharedVm, effects: OnLoadEffects) {
    if let Some(interval) = effects.heap_sampling_interval {
        shared.debug.jvmti_env.lock().heap_sampling_interval = interval;
    }
    if !effects.properties.is_empty() {
        let mut props = shared.system_properties.write();
        for (key, value) in effects.properties {
            props.insert(key, value);
        }
    }
    if !effects.boot_segments.is_empty() {
        shared
            .classes
            .class_manager
            .write()
            .extend_bootstrap_classpath(&effects.boot_segments);
    }
    if !effects.system_segments.is_empty() {
        shared
            .classes
            .class_manager
            .write()
            .extend_application_classpath(&effects.system_segments);
    }
}

// --- Raw monitors (slots 31-37) --------------------------------------------

/// `JVMTI_ERROR_INVALID_MONITOR` / `NOT_MONITOR_OWNER`.
const ERR_INVALID_MONITOR: JInt = 50;
const ERR_NOT_MONITOR_OWNER: JInt = 51;

/// A `jrawMonitorID`: a reentrant monitor with wait and notify, owned by an
/// OS thread (a raw monitor may be entered by any thread, attached or not,
/// and in `Agent_OnLoad`). Freed by `DestroyRawMonitor`.
struct RawMonitor {
    /// Tells a live monitor from a stray pointer (`INVALID_MONITOR`).
    magic: u64,
    state: parking_lot::Mutex<RawMonitorState>,
    /// Signalled when the monitor becomes free.
    entry: parking_lot::Condvar,
    /// Signalled by `RawMonitorNotify` / `NotifyAll`.
    waiters: parking_lot::Condvar,
}

struct RawMonitorState {
    owner: Option<std::thread::ThreadId>,
    /// Entries of the owner not yet exited.
    count: u32,
}

/// [`RawMonitor::magic`] of a live monitor ("RAWMONIT").
const RAW_MONITOR_MAGIC: u64 = 0x5241_574D_4F4E_4954;

/// The monitor a `jrawMonitorID` names, or `INVALID_MONITOR`.
fn raw_monitor<'a>(monitor: *const RawMonitor) -> Result<&'a RawMonitor, JInt> {
    if monitor.is_null() {
        return Err(ERR_INVALID_MONITOR);
    }
    // SAFETY: an id `CreateRawMonitor` handed out and not destroyed (the
    // spec makes any other value the agent's error; the magic catches the
    // common ones).
    let monitor = unsafe { &*monitor };
    if monitor.magic != RAW_MONITOR_MAGIC {
        return Err(ERR_INVALID_MONITOR);
    }
    Ok(monitor)
}

/// Run `block`, which may wait for another thread, GC-safe when the env has
/// a VM ([`gc_safe`]); plainly in `Agent_OnLoad`.
fn raw_monitor_block<R>(env: &JvmtiNativeEnv, block: impl FnOnce() -> R) -> R {
    match env.vm() {
        Some(shared) => gc_safe(&shared, block),
        None => block(),
    }
}

/// gci i47w lane oz47 (`vi12-ev.list` T: every row a `[stw-stall]` hang):
/// take `raw`'s inner lock without stalling a stop-the-world pause.
///
/// A waiter that wakes in `RawMonitorWait` (or an enterer woken in
/// [`raw_monitor_acquire`]) re-takes this lock INSIDE its GC-safe region and
/// keeps it while `end_blocking_region` waits for a pause to finish. A thread
/// that then reached the lock with a plain `lock()` blocked counted and
/// running, so the pause never got its arrival and the holder never got out:
/// the probe's four `ThreadStart` callbacks (`RawMonitorEnter` / `Wait(1)`)
/// beside a collecting thread deadlocked every run. HotSpot's
/// `JvmtiRawMonitor` blocks a contended entry `_thread_blocked`. So: the
/// uncontended `try_lock` as before, and a contended one GC-safe
/// ([`raw_monitor_block`]); both modes, every collector.
fn lock_raw_monitor_state<'a>(
    env: &JvmtiNativeEnv,
    raw: &'a RawMonitor,
) -> parking_lot::MutexGuard<'a, RawMonitorState> {
    if let Some(state) = raw.state.try_lock() {
        return state;
    }
    raw_monitor_block(env, || raw.state.lock())
}

/// Slot 31: `CreateRawMonitor(env, name, monitor_ptr)` (OnLoad and live
/// phases, as HotSpot allows; the start phase too).
extern "C" fn create_raw_monitor(
    env: *mut JvmtiNativeEnv,
    name: *const std::ffi::c_char,
    monitor_ptr: *mut *mut RawMonitor,
) -> JInt {
    if let Err(code) = live_env(env) {
        return code;
    }
    if name.is_null() || monitor_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let monitor = Box::new(RawMonitor {
        magic: RAW_MONITOR_MAGIC,
        state: parking_lot::Mutex::new(RawMonitorState {
            owner: None,
            count: 0,
        }),
        entry: parking_lot::Condvar::new(),
        waiters: parking_lot::Condvar::new(),
    });
    // SAFETY: the agent's non-null out-parameter. OWNERSHIP: freed by
    // `DestroyRawMonitor`.
    unsafe { *monitor_ptr = Box::into_raw(monitor) };
    ERR_NONE
}

/// Slot 32: `DestroyRawMonitor(env, monitor)`: `NOT_MONITOR_OWNER` while
/// another thread owns it; the owner may destroy it (HotSpot exits it
/// first).
extern "C" fn destroy_raw_monitor(env: *mut JvmtiNativeEnv, monitor: *mut RawMonitor) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let raw = match raw_monitor(monitor) {
        Ok(raw) => raw,
        Err(code) => return code,
    };
    {
        let state = lock_raw_monitor_state(env, raw);
        if state
            .owner
            .is_some_and(|owner| owner != std::thread::current().id())
        {
            return ERR_NOT_MONITOR_OWNER;
        }
    }
    // SAFETY: an id `CreateRawMonitor` made from a `Box`; the magic is
    // cleared first so a later use of the stale id is likelier refused.
    unsafe {
        std::ptr::addr_of_mut!((*monitor).magic).write(0);
        drop(Box::from_raw(monitor));
    }
    ERR_NONE
}

/// Take `raw` for the calling thread, waiting while another owns it
/// (`state` is its lock, held).
fn raw_monitor_acquire(
    env: &JvmtiNativeEnv,
    raw: &RawMonitor,
    state: &mut parking_lot::MutexGuard<'_, RawMonitorState>,
    count: u32,
) {
    let me = std::thread::current().id();
    if state.owner.is_some() {
        raw_monitor_block(env, || {
            while state.owner.is_some() {
                raw.entry.wait(state);
            }
        });
    }
    state.owner = Some(me);
    state.count = count;
}

/// Slot 33: `RawMonitorEnter(env, monitor)`: reentrant.
extern "C" fn raw_monitor_enter(env: *mut JvmtiNativeEnv, monitor: *mut RawMonitor) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let raw = match raw_monitor(monitor) {
        Ok(raw) => raw,
        Err(code) => return code,
    };
    let mut state = lock_raw_monitor_state(env, raw);
    if state.owner == Some(std::thread::current().id()) {
        state.count = state.count.saturating_add(1);
        return ERR_NONE;
    }
    raw_monitor_acquire(env, raw, &mut state, 1);
    ERR_NONE
}

/// Slot 34: `RawMonitorExit(env, monitor)`.
extern "C" fn raw_monitor_exit(env: *mut JvmtiNativeEnv, monitor: *mut RawMonitor) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let raw = match raw_monitor(monitor) {
        Ok(raw) => raw,
        Err(code) => return code,
    };
    let mut state = lock_raw_monitor_state(env, raw);
    if state.owner != Some(std::thread::current().id()) {
        return ERR_NOT_MONITOR_OWNER;
    }
    state.count -= 1;
    if state.count == 0 {
        state.owner = None;
        raw.entry.notify_one();
    }
    ERR_NONE
}

/// Slot 35: `RawMonitorWait(env, monitor, millis)`: releases the monitor
/// entirely, waits for a notify or `millis` (none when `millis <= 0`), and
/// takes it back with its entry count.
extern "C" fn raw_monitor_wait(
    env: *mut JvmtiNativeEnv,
    monitor: *mut RawMonitor,
    millis: i64,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let raw = match raw_monitor(monitor) {
        Ok(raw) => raw,
        Err(code) => return code,
    };
    let mut state = lock_raw_monitor_state(env, raw);
    if state.owner != Some(std::thread::current().id()) {
        return ERR_NOT_MONITOR_OWNER;
    }
    let count = state.count;
    state.owner = None;
    state.count = 0;
    raw.entry.notify_one();
    raw_monitor_block(env, || match u64::try_from(millis) {
        Ok(ms) if ms > 0 => {
            let _ = raw
                .waiters
                .wait_for(&mut state, std::time::Duration::from_millis(ms));
        }
        _ => raw.waiters.wait(&mut state),
    });
    raw_monitor_acquire(env, raw, &mut state, count);
    ERR_NONE
}

/// Slots 36 / 37: `RawMonitorNotify` / `RawMonitorNotifyAll(env, monitor)`.
fn raw_monitor_notify(env: *mut JvmtiNativeEnv, monitor: *mut RawMonitor, all: bool) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let raw = match raw_monitor(monitor) {
        Ok(raw) => raw,
        Err(code) => return code,
    };
    let state = lock_raw_monitor_state(env, raw);
    if state.owner != Some(std::thread::current().id()) {
        return ERR_NOT_MONITOR_OWNER;
    }
    if all {
        raw.waiters.notify_all();
    } else {
        raw.waiters.notify_one();
    }
    ERR_NONE
}

extern "C" fn raw_monitor_notify_one(env: *mut JvmtiNativeEnv, monitor: *mut RawMonitor) -> JInt {
    raw_monitor_notify(env, monitor, false)
}

extern "C" fn raw_monitor_notify_all(env: *mut JvmtiNativeEnv, monitor: *mut RawMonitor) -> JInt {
    raw_monitor_notify(env, monitor, true)
}

// --- Threads and thread groups (slots 9, 13, 14) ---------------------------

/// `jvmtiThreadInfo`: `{ char* name; jint priority; jboolean is_daemon;
/// jthreadGroup thread_group; jobject context_class_loader; }`, 32 bytes.
#[repr(C)]
struct ThreadInfoRow {
    name: *mut u8,
    priority: JInt,
    is_daemon: u8,
    thread_group: JObject,
    context_class_loader: JObject,
}

/// `jvmtiThreadGroupInfo`: `{ jthreadGroup parent; char* name; jint
/// max_priority; jboolean is_daemon; }`, 24 bytes.
#[repr(C)]
struct ThreadGroupInfoRow {
    parent: JObject,
    name: *mut u8,
    max_priority: JInt,
    is_daemon: u8,
}

/// `JVMTI_THREAD_NORM_PRIORITY`, a virtual thread's.
const NORM_PRIORITY: JInt = 5;

/// Run `read` on the calling thread's own `JvmThread` (a mutator: a JNI
/// native, an event callback, an attached agent thread) inside its
/// foreign-thread entry and in `shared`'s JNI context, so the handles it
/// decodes and makes are the caller's local references.
/// `JVMTI_ERROR_UNATTACHED_THREAD` for a thread the VM does not know.
fn on_calling_thread<R>(
    shared: &SharedVm,
    read: impl FnOnce(&mut crate::vm::vm_exec::NativeContextImpl<'_>) -> R,
) -> Result<R, JInt> {
    let current = jni::current_jvm_thread_of(shared).ok_or(ERR_UNATTACHED_THREAD)?;
    let _fx = jni::ForeignJniEntry::enter();
    let prev = jni::replace_jni_context(shared);
    // gci i18/na: this context is not under `safe_native_call_impl` (an
    // agent's C code called the JVMTI function), so its allocations used to
    // take the infallible arm, which aborts the process on Generational and
    // ZGC (`RedefineClasses` / `RetransformClasses` allocate). Inside the
    // scope a refusal unwinds no further than here -- never into the agent's
    // C frames -- and answers `JVMTI_ERROR_OUT_OF_MEMORY`, as HotSpot's
    // functions do when an allocation fails.
    let out = crate::runtime::native_oom::catch_alloc_oom_on(
        // SAFETY: the calling thread's own, live `JvmThread`, used only by
        // it for this call (as the event context installs it).
        unsafe { &mut *current },
        |thread| {
            let mut ctx = crate::vm::vm_exec::NativeContextImpl { shared, thread };
            read(&mut ctx)
        },
    );
    jni::restore_jni_context(prev);
    out.map_err(|_| ERR_OUT_OF_MEMORY)
}

/// A string field of `obj` as text (NULL and a non-string: `None`).
fn string_field(
    ctx: &crate::vm::vm_exec::NativeContextImpl<'_>,
    obj: ObjectRef,
    slot: Option<usize>,
) -> Option<String> {
    match slot.map(|s| ctx.get_field(obj, s)) {
        Some(Value::Object(Some(s))) => crate::vm::read_java_string(&ctx.shared.mem.heap, s),
        _ => None,
    }
}

/// What `GetThreadInfo` answers of `thread_obj`, as HotSpot reads it
/// (`JvmtiEnv::GetThreadInfo`): `Thread.name`, the `FieldHolder`'s
/// priority, daemon bit and group (a terminated thread's group is NULL), the
/// context class loader; a virtual thread is a daemon of normal priority in
/// `Thread$Constants.VTHREAD_GROUP`. The fields are `java.lang.Thread`'s own
/// (`vm_exec::thread_field_owner`), not a subclass's of the same name.
/// Without a `FieldHolder` (a `Thread` of another layout) the registry's
/// daemon bit and the normal priority answer.
fn thread_info_of(
    ctx: &crate::vm::vm_exec::NativeContextImpl<'_>,
    thread_obj: ObjectRef,
    terminated: bool,
) -> (String, JInt, bool, Option<ObjectRef>, Option<ObjectRef>) {
    let shared = ctx.shared;
    let (name_slot, holder_slot, loader_slot, is_virtual) = {
        let cm = shared.classes.class_manager.read();
        let class_id = shared.mem.heap.class_id_of(thread_obj);
        let owner = crate::vm::vm_exec::thread_field_owner(shared, &cm.class_store, class_id);
        let slot = |name: &str| {
            crate::vm::vm_exec::resolve_field_index_in_hierarchy(owner, name, &cm.class_store)
        };
        (
            slot("name"),
            slot("holder"),
            slot("contextClassLoader"),
            cm.is_subclass_of_by_name(class_id, "java/lang/BaseVirtualThread"),
        )
    };
    let name = string_field(ctx, thread_obj, name_slot).unwrap_or_default();
    let loader = match loader_slot.map(|s| ctx.get_field(thread_obj, s)) {
        Some(Value::Object(loader)) => loader,
        _ => None,
    };
    if is_virtual {
        let group = if terminated {
            None
        } else {
            ctx.class_id_by_name("java/lang/Thread$Constants")
                .filter(|&class_id| ctx.is_class_initialized(class_id))
                .and_then(|class_id| {
                    let index = ctx.static_field_index_by_name(class_id, "VTHREAD_GROUP")?;
                    match ctx.get_static_field(class_id, index) {
                        Value::Object(group) => group,
                        _ => None,
                    }
                })
        };
        return (name, NORM_PRIORITY, true, group, loader);
    }
    let holder = match holder_slot.map(|s| ctx.get_field(thread_obj, s)) {
        Some(Value::Object(Some(holder))) => Some(holder),
        _ => None,
    };
    let (priority, daemon) = match holder {
        Some(holder) => (
            match ctx.get_field_by_name(holder, "priority") {
                Value::Int(p) => p,
                _ => NORM_PRIORITY,
            },
            matches!(ctx.get_field_by_name(holder, "daemon"), Value::Int(d) if d != 0),
        ),
        None => {
            let registry = &shared.threads.thread_registry;
            let daemon = crate::vm::vm_exec::read_java_thread_tid(shared, thread_obj)
                .and_then(|java_tid| registry.find_thread_id_by_java_tid(java_tid))
                .is_some_and(|tid| registry.is_daemon(tid));
            (NORM_PRIORITY, daemon)
        }
    };
    let group = if terminated {
        None
    } else {
        crate::debug::inspect::group_of_thread(ctx, thread_obj)
    };
    (name, priority, daemon, group, loader)
}

/// Slot 9: `GetThreadInfo(env, thread, info_ptr)`: a NULL `thread` is the
/// calling thread. A thread not started yet or terminated is answered too,
/// as HotSpot answers it from the `Thread` object (a terminated one with a
/// NULL group). The name is an `Allocate` block; the group and the loader
/// are local references.
extern "C" fn get_thread_info(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    info_ptr: *mut ThreadInfoRow,
) -> JInt {
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    // Interpreter round i5 wave 101, lane L1: HotSpot's generated entry
    // (`jvmtiEnter`) pre-checks the out-parameter; the thread, a `noconvert`
    // parameter, is checked by the function after it (`GetThreadState` here
    // already did so). A NULL `info_ptr` with a bad handle answered
    // `INVALID_THREAD` where HotSpot answers `NULL_POINTER`.
    if info_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let terminated = if thread == 0 {
        false
    } else {
        match lookup_thread(&shared, thread) {
            Ok(lookup) => matches!(lookup, ThreadLookup::Dead),
            Err(code) => return code,
        }
    };
    let answer = on_calling_thread(&shared, |ctx| {
        let thread_obj = if thread == 0 {
            let tid = ctx.thread.thread_id;
            shared.threads.thread_registry.java_thread_obj(tid)
        } else {
            jni::jobject_to_obj(thread)
        };
        let thread_obj = thread_obj
            .filter(|&obj| is_thread_object(&shared, obj))
            .ok_or(ERR_INVALID_THREAD)?;
        let (name, priority, daemon, group, loader) = thread_info_of(ctx, thread_obj, terminated);
        let local = |obj: Option<ObjectRef>| obj.map_or(0, jni::new_local_handle);
        Ok((name, priority, daemon, local(group), local(loader)))
    });
    let (name, priority, daemon, group, loader) = match answer {
        Ok(Ok(facts)) => facts,
        Ok(Err(code)) | Err(code) => return code,
    };
    let name = allocate_modified_utf8(&name);
    if name.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe {
        info_ptr.write(ThreadInfoRow {
            name,
            priority,
            is_daemon: u8::from(daemon),
            thread_group: group,
            context_class_loader: loader,
        });
    }
    ERR_NONE
}

/// Is `obj` a `java.lang.ThreadGroup`?
fn is_thread_group_object(shared: &SharedVm, obj: ObjectRef) -> bool {
    let class_id = shared.mem.heap.class_id_of(obj);
    shared
        .classes
        .class_manager
        .read()
        .is_subclass_of_by_name(class_id, "java/lang/ThreadGroup")
}

/// Slot 13: `GetTopThreadGroups(env, group_count_ptr, groups_ptr)`: the
/// root of the VM's thread groups (`system`), found from the live threads'
/// groups, as local references in an `Allocate`d array.
extern "C" fn get_top_thread_groups(
    env: *mut JvmtiNativeEnv,
    group_count_ptr: *mut JInt,
    groups_ptr: *mut *mut JObject,
) -> JInt {
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    if group_count_ptr.is_null() || groups_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let roots = on_calling_thread(&shared, |ctx| {
        let threads = shared
            .threads
            .thread_registry
            .alive_thread_objects(usize::MAX);
        let mut roots: Vec<ObjectRef> = Vec::new();
        for thread_obj in threads {
            let Some(group) = crate::debug::inspect::group_of_thread(ctx, thread_obj) else {
                continue;
            };
            let root = crate::debug::inspect::group_root(ctx, group);
            if !roots.iter().any(|r| r.as_ptr() == root.as_ptr()) {
                roots.push(root);
            }
        }
        roots
            .into_iter()
            .map(jni::new_local_handle)
            .collect::<Vec<JObject>>()
    });
    let roots = match roots {
        Ok(roots) => roots,
        Err(code) => return code,
    };
    let Ok(count) = JInt::try_from(roots.len()) else {
        return ERR_INTERNAL;
    };
    let block =
        allocate_block(roots.len().max(1) * std::mem::size_of::<JObject>()).cast::<JObject>();
    if block.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    // SAFETY: `block` holds `roots.len()` handles; the out-parameters are
    // the agent's, non-null.
    unsafe {
        std::ptr::copy_nonoverlapping(roots.as_ptr(), block, roots.len());
        *group_count_ptr = count;
        *groups_ptr = block;
    }
    ERR_NONE
}

/// Slot 14: `GetThreadGroupInfo(env, group, info_ptr)`: the group's
/// parent, name, `maxPriority` and daemon bit (`ThreadGroup`'s fields).
/// `JVMTI_ERROR_INVALID_THREAD_GROUP` for a handle naming no group.
extern "C" fn get_thread_group_info(
    env: *mut JvmtiNativeEnv,
    group: JObject,
    info_ptr: *mut ThreadGroupInfoRow,
) -> JInt {
    const ERR_INVALID_THREAD_GROUP: JInt = 11;
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let answer = on_calling_thread(&shared, |ctx| {
        let group = jni::jobject_to_obj(group)
            .filter(|&obj| is_thread_group_object(&shared, obj))
            .ok_or(ERR_INVALID_THREAD_GROUP)?;
        let name = match ctx.get_field_by_name(group, "name") {
            Value::Object(Some(s)) => crate::vm::read_java_string(&shared.mem.heap, s),
            _ => None,
        };
        let max_priority = match ctx.get_field_by_name(group, "maxPriority") {
            Value::Int(p) => p,
            _ => 10,
        };
        let daemon = matches!(ctx.get_field_by_name(group, "daemon"), Value::Int(d) if d != 0);
        let parent =
            crate::debug::inspect::group_parent(ctx, group).map_or(0, jni::new_local_handle);
        Ok((parent, name.unwrap_or_default(), max_priority, daemon))
    });
    let (parent, name, max_priority, daemon) = match answer {
        Ok(Ok(facts)) => facts,
        Ok(Err(code)) | Err(code) => return code,
    };
    if info_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let name = allocate_modified_utf8(&name);
    if name.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe {
        info_ptr.write(ThreadGroupInfoRow {
            parent,
            name,
            max_priority,
            is_daemon: u8::from(daemon),
        });
    }
    ERR_NONE
}

// --- Classes (slots 50, 51, 53-57, 145) -------------------------------------

/// `can_get_source_file_name`, the 12th bit field (wave 52:
/// `GetSourceFileName`), and `can_get_synthetic_attribute`, the 5th
/// (`IsFieldSynthetic`, `IsMethodSynthetic`). HotSpot grants both in every
/// phase.
const CAN_GET_SOURCE_FILE_NAME: u32 = 1 << 11;
const CAN_GET_SYNTHETIC_ATTRIBUTE: u32 = 1 << 4;
/// `JVMTI_ERROR_INVALID_FIELDID`.
const ERR_INVALID_FIELDID: JInt = 25;
/// `ACC_INTERFACE`.
const ACC_INTERFACE: u16 = 0x0200;
/// `ACC_SYNTHETIC`.
const ACC_SYNTHETIC: u16 = 0x1000;

/// Is `name` a primitive type's class (`int`, ..., `void`)?
fn is_primitive_class_name(name: &str) -> bool {
    matches!(
        name,
        "boolean" | "byte" | "char" | "short" | "int" | "long" | "float" | "double" | "void"
    )
}

/// Store one `jint` out-parameter.
fn write_jint(ptr: *mut JInt, value: Result<JInt, JInt>) -> JInt {
    let value = match value {
        Ok(value) => value,
        Err(code) => return code,
    };
    if ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *ptr = value };
    ERR_NONE
}

/// Store one `jboolean` out-parameter.
fn write_jboolean(ptr: *mut u8, value: Result<bool, JInt>) -> JInt {
    let value = match value {
        Ok(value) => value,
        Err(code) => return code,
    };
    if ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *ptr = u8::from(value) };
    ERR_NONE
}

/// `ids` in a fresh `Allocate` block, with its count, into the agent's
/// out-parameters.
fn write_id_array<T: Copy>(count_ptr: *mut JInt, array_ptr: *mut *mut T, ids: &[T]) -> JInt {
    if count_ptr.is_null() || array_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let Ok(count) = JInt::try_from(ids.len()) else {
        return ERR_INTERNAL;
    };
    let Some(bytes) = ids.len().max(1).checked_mul(std::mem::size_of::<T>()) else {
        return ERR_OUT_OF_MEMORY;
    };
    let block = allocate_block(bytes).cast::<T>();
    if block.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    // SAFETY: `block` holds `ids.len()` elements, 16-aligned; the
    // out-parameters are the agent's, non-null.
    unsafe {
        std::ptr::copy_nonoverlapping(ids.as_ptr(), block, ids.len());
        *count_ptr = count;
        *array_ptr = block;
    }
    ERR_NONE
}

/// Slot 50: `GetSourceFileName(env, klass, source_name_ptr)`
/// (`can_get_source_file_name`): the `SourceFile` attribute;
/// `JVMTI_ERROR_ABSENT_INFORMATION` without one, and for an array or a
/// primitive class.
extern "C" fn get_source_file_name(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    source_name_ptr: *mut *mut u8,
) -> JInt {
    match live_env(env) {
        Ok(e) if !e.possesses(CAN_GET_SOURCE_FILE_NAME) => return ERR_MUST_POSSESS_CAPABILITY,
        Ok(_) => {}
        Err(code) => return code,
    }
    // Wave 55: a lambda proxy class has none (measured 101).
    // Wave 56: a primitive class has none (measured 101).
    let source = match with_jvmti_class(
        env,
        klass,
        |_, class| class.source_file.clone(),
        |_, _| None,
        |_| None,
    ) {
        Ok(source) => source,
        Err(code) => return code,
    };
    if source_name_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let Some(source) = source else {
        return ERR_ABSENT_INFORMATION;
    };
    if write_modified_utf8_outs(&[(source_name_ptr, Some(&source))]) {
        ERR_NONE
    } else {
        ERR_OUT_OF_MEMORY
    }
}

/// Slot 51: `GetClassModifiers(env, klass, modifiers_ptr)`: HotSpot 25's
/// answer, as JDWP's `ReferenceType.Modifiers` gives it
/// (`debug::jdwp_class_modifiers`: the `InnerClasses` flags of a nested
/// class, with `ACC_SUPER` for every class and interface, an array
/// `0x431`); a primitive class is `public final abstract` (`0x411`).
extern "C" fn get_class_modifiers(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    modifiers_ptr: *mut JInt,
) -> JInt {
    let modifiers = with_jvmti_class(
        env,
        klass,
        |_, class| {
            if is_primitive_class_name(&class.name) {
                PRIMITIVE_CLASS_MODIFIERS
            } else {
                JInt::try_from(crate::debug::jdwp_class_modifiers(class)).unwrap_or(0)
            }
        },
        // Wave 55: a lambda proxy class.
        |_, _| LAMBDA_PROXY_MODIFIERS,
        // Wave 56: a primitive class's mirror.
        |_| PRIMITIVE_CLASS_MODIFIERS,
    );
    write_jint(modifiers_ptr, modifiers)
}

/// The `jfieldID` bit that marks a static field (wave 52): JNI's encoding
/// `(declaring class id << 32) | index` gives a static field its index among
/// the class's static fields and an instance field its absolute slot, so the
/// two collide. `GetStaticFieldID` sets it too, and JNI's decode drops it
/// (`jni::decode_field_id`), so an id this table hands out works with
/// `GetStatic<Type>Field`.
const STATIC_FIELD_ID: u64 = jni::STATIC_FIELD_ID_MARK;

/// The `jfieldID`s of `class`'s declared fields, in class-file order.
fn declared_field_ids(class: &crate::classloading::Class) -> Vec<u64> {
    let class_half = u64::from(class.id.as_u32()) << 32;
    let (mut statics, mut instances) = (0u64, 0u64);
    let first = u64::try_from(class.first_field_index).unwrap_or(0);
    class
        .fields
        .iter()
        .map(|field| {
            if field.is_static() {
                statics += 1;
                STATIC_FIELD_ID | class_half | (statics - 1)
            } else {
                instances += 1;
                class_half | (first + instances - 1)
            }
        })
        .collect()
}

/// `JVMTI_ERROR_CLASS_NOT_PREPARED`.
const ERR_CLASS_NOT_PREPARED: JInt = 22;
/// `JVMTI_CLASS_STATUS_PREPARED`.
const CLASS_STATUS_PREPARED: JInt = 2;

/// `JVMTI_ERROR_CLASS_NOT_PREPARED` for a class that is neither prepared
/// nor an array, as HotSpot answers `GetClassFields` and
/// `GetImplementedInterfaces` (measured on 25.0.3: a nested class named only
/// by its class literal answers 22, L1W52JvmtiThreadAndClassFacts); a
/// primitive class is never refused.
fn prepared(class: &crate::classloading::Class) -> Result<(), JInt> {
    let status = class_status(class);
    if status & (CLASS_STATUS_PREPARED | CLASS_STATUS_ARRAY | CLASS_STATUS_PRIMITIVE) == 0 {
        return Err(ERR_CLASS_NOT_PREPARED);
    }
    Ok(())
}

/// Slot 53: `GetClassFields(env, klass, field_count_ptr, fields_ptr)`: the
/// fields `klass` declares, in class-file order (none for an array or a
/// primitive class); `JVMTI_ERROR_CLASS_NOT_PREPARED` before the class is
/// prepared.
extern "C" fn get_class_fields(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    field_count_ptr: *mut JInt,
    fields_ptr: *mut *mut u64,
) -> JInt {
    let ids = with_jvmti_class(
        env,
        klass,
        |_, class| prepared(class).map(|()| declared_field_ids(class)),
        // The i54-L1 retirement: a lambda proxy class's `arg$N` fields
        // ([`proxy_field`]).
        |shared, id| {
            let count = lambda_proxy_facts(shared, id).map_or(0, |facts| facts.fields.len());
            let class_half = u64::from(id.as_u32()) << 32;
            // Widening: a capture index into the id's low half.
            Ok((0..count).map(|i| class_half | i as u64).collect())
        },
        // Wave 56: a primitive class declares none.
        |_| Ok(Vec::new()),
    );
    match ids {
        Ok(Ok(ids)) => write_id_array(field_count_ptr, fields_ptr, &ids),
        Ok(Err(code)) | Err(code) => code,
    }
}

/// Slot 54: `GetImplementedInterfaces(env, klass, interface_count_ptr,
/// interfaces_ptr)`: the direct superinterfaces, in declaration order (none
/// for an array or a primitive class); `JVMTI_ERROR_CLASS_NOT_PREPARED`
/// before the class is prepared.
extern "C" fn get_implemented_interfaces(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    interface_count_ptr: *mut JInt,
    interfaces_ptr: *mut *mut JObject,
) -> JInt {
    let interfaces = with_jvmti_class(
        env,
        klass,
        |_, class| {
            prepared(class)?;
            Ok(if class.name.starts_with('[') {
                Vec::new()
            } else {
                class
                    .interfaces
                    .iter()
                    .map(|&id| jni::class_id_to_jclass(id))
                    .collect::<Vec<JObject>>()
            })
        },
        // Wave 55: a lambda proxy class (always prepared).
        |shared, id| {
            Ok(lambda_proxy_facts(shared, id)
                .map(|facts| facts.interfaces)
                .unwrap_or_default()
                .into_iter()
                .map(jni::class_id_to_jclass)
                .collect())
        },
        // Wave 56: a primitive class has none.
        |_| Ok(Vec::new()),
    );
    match interfaces {
        Ok(Ok(ids)) => write_id_array(interface_count_ptr, interfaces_ptr, &ids),
        Ok(Err(code)) | Err(code) => code,
    }
}

/// Slot 78: `GetLoadedClasses(env, class_count_ptr, classes_ptr)`
/// (interpreter round i2 wave 53, lane L1): every class of the VM, array
/// classes included and the primitive classes not, as HotSpot lists the
/// class-loader data graph; a class still being defined (`Loading`) is not
/// loaded yet. Live phase only.
extern "C" fn get_loaded_classes(
    env: *mut JvmtiNativeEnv,
    class_count_ptr: *mut JInt,
    classes_ptr: *mut *mut JObject,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let Some(shared) = env.vm() else {
        return ERR_WRONG_PHASE;
    };
    if shared.debug.debugger_gates.jvmti_outside_live() {
        return ERR_WRONG_PHASE;
    }
    if class_count_ptr.is_null() || classes_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    const PRIMITIVES: [&str; 9] = [
        "boolean", "byte", "char", "short", "int", "long", "float", "double", "void",
    ];
    let mut classes: Vec<JObject> = shared
        .classes
        .class_manager
        .read()
        .class_store
        // zt t18/df7 (x13 item 21a): not an unbound or withdrawn define.
        .iter_named()
        .filter(|class| {
            !matches!(class.state, crate::classloading::ClassState::Loading)
                && !PRIMITIVES.contains(&&*class.name)
        })
        .map(|class| jni::class_id_to_jclass(class.id))
        .collect();
    // Wave 55: and the lambda proxy classes, which HotSpot lists as the
    // hidden classes they are (`Instrumentation.getAllLoadedClasses` lists
    // them too, `instrument::native_get_all_loaded_classes0`), while
    // `DebuggerGates::c_lambda_classes` is on.
    if shared.debug.debugger_gates.c_lambda_classes() {
        classes.extend(
            shared
                .classes
                .lambda_proxies
                .read()
                .keys()
                .map(|&id| jni::class_id_to_jclass(id)),
        );
    }
    write_id_array(class_count_ptr, classes_ptr, &classes)
}

/// Slot 79: `GetClassLoaderClasses(env, initiating_loader, class_count_ptr,
/// classes_ptr)` (interpreter round i2 wave 54, lane L1): the classes
/// `initiating_loader` (NULL: the bootstrap loader) is an initiating loader
/// of -- libinstrument's `getInitiatedClasses0` is this function, so the list
/// is `Instrumentation.getInitiatedClasses`'
/// (`instrument::initiated_class_ids`, measured against HotSpot by lane L3
/// in waves 41-42). Live phase only; the loader is decoded on the calling
/// thread, which must be attached.
extern "C" fn get_class_loader_classes(
    env: *mut JvmtiNativeEnv,
    initiating_loader: JObject,
    class_count_ptr: *mut JInt,
    classes_ptr: *mut *mut JObject,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let Some(shared) = env.vm() else {
        return ERR_WRONG_PHASE;
    };
    if shared.debug.debugger_gates.jvmti_outside_live() {
        return ERR_WRONG_PHASE;
    }
    if class_count_ptr.is_null() || classes_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let ids = on_calling_thread(&shared, |ctx| {
        let loader = match jni::jobject_to_obj(initiating_loader) {
            Some(loader) => Some(loader),
            None if initiating_loader != 0 => return Err(ERR_INVALID_OBJECT),
            None => None,
        };
        Ok(crate::runtime::instrument::initiated_class_ids(ctx, loader))
    });
    match ids {
        Ok(Ok(ids)) => {
            let classes: Vec<JObject> = ids.into_iter().map(jni::class_id_to_jclass).collect();
            write_id_array(class_count_ptr, classes_ptr, &classes)
        }
        Ok(Err(code)) | Err(code) => code,
    }
}

// --- One decode of a `jclass` (interpreter round i2 wave 57, lane L1) ------
//
// Proposal
// `docs/feature-designs/jvmti-delivery-threading.md`.
// A class function decodes its handle once, inside one foreign-thread entry,
// into the kind of class it names ([`jvmti_class`]), and answers per kind
// ([`with_jvmti_class`]); waves 54-56 decoded it with `class_of` and, on
// `JVMTI_ERROR_INVALID_CLASS`, once more per fallback (a lambda proxy, a
// primitive class's mirror). The stored-class path is first and costs what
// `class_of` cost.

/// What a `jclass` names to this table, before the gates
/// ([`decode_class`]).
#[derive(Clone, Debug, PartialEq, Eq)]
enum DecodedClass {
    /// A class the class store holds ([`class_of`]'s answer).
    Stored(ClassId),
    /// A lambda proxy class the VM minted outside the store
    /// (`ClassRealm::lambda_proxies`).
    LambdaProxy(ClassId),
    /// A primitive class's mirror (`int.class`), by its Java name.
    Primitive(String),
    /// Some other `java.lang.Class` mirror with no `ClassId`.
    OtherMirror,
}

/// What a `jclass` names to the class functions: [`DecodedClass`] after
/// the gates (`DebuggerGates::c_lambda_classes`,
/// `DebuggerGates::c_primitive_classes`; a gated-off kind is
/// `JVMTI_ERROR_INVALID_CLASS`, as before the kind was known).
#[derive(Clone, Debug, PartialEq, Eq)]
enum JvmtiClass {
    Stored(ClassId),
    LambdaProxy(ClassId),
    Primitive(String),
}

/// The kind of class `klass` names in `shared`, ungated; `None` for NULL
/// and for a handle that names no class (a `String`, a stale id). One
/// foreign-thread entry; a stored class answers with no further lock than
/// [`class_of`]'s.
fn decode_class(shared: &SharedVm, klass: JObject) -> Option<DecodedClass> {
    if klass == 0 {
        return None;
    }
    let fx = jni::ForeignJniEntry::enter();
    let prev = jni::replace_jni_context(shared);
    // A mirror with no `ClassId` is read inside the entry (the object may
    // move once it is left), as wave 54's `is_unlisted_class` read it.
    let decoded = match jni::jclass_class_id_exact(klass) {
        Some(id) => Ok(id),
        None => Err(jni::jobject_to_obj(klass).and_then(|obj| decode_mirror(shared, obj))),
    };
    jni::restore_jni_context(prev);
    drop(fx);
    match decoded {
        Ok(id) => {
            if shared.classes.class_manager.read().get_class(id).is_some() {
                Some(DecodedClass::Stored(id))
            } else if is_lambda_proxy(shared, id) {
                Some(DecodedClass::LambdaProxy(id))
            } else {
                None
            }
        }
        Err(mirror) => mirror,
    }
}

/// [`decode_class`] of a handle with no `ClassId`: a primitive class's
/// mirror, another `java.lang.Class` mirror, or no class.
#[cold]
#[inline(never)]
fn decode_mirror(shared: &SharedVm, obj: ObjectRef) -> Option<DecodedClass> {
    let primitive = shared
        .classes
        .primitive_mirrors
        .read()
        .iter()
        .find(|(_, mirror)| mirror.as_ptr() == obj.as_ptr())
        .map(|(name, _)| name.clone());
    if let Some(name) = primitive {
        return Some(DecodedClass::Primitive(name));
    }
    let cid = shared.mem.heap.class_id_of(obj);
    let is_mirror = shared
        .classes
        .class_manager
        .read()
        .get_class(cid)
        .is_some_and(|class| &*class.name == "java/lang/Class");
    is_mirror.then_some(DecodedClass::OtherMirror)
}

/// [`decode_class`] under the gates: the class a class function answers
/// for, or `JVMTI_ERROR_INVALID_CLASS`.
fn jvmti_class(shared: &SharedVm, klass: JObject) -> Result<JvmtiClass, JInt> {
    match decode_class(shared, klass) {
        Some(DecodedClass::Stored(id)) => Ok(JvmtiClass::Stored(id)),
        Some(DecodedClass::LambdaProxy(id)) if shared.debug.debugger_gates.c_lambda_classes() => {
            Ok(JvmtiClass::LambdaProxy(id))
        }
        Some(DecodedClass::Primitive(name))
            if shared.debug.debugger_gates.c_primitive_classes() =>
        {
            Ok(JvmtiClass::Primitive(name))
        }
        _ => Err(ERR_INVALID_CLASS),
    }
}

/// A class function's answer for `klass` by its kind ([`jvmti_class`]):
/// `stored` reads a stored class under the class-manager read lock, `proxy`
/// answers a lambda proxy class, `primitive` a primitive class by its Java
/// name. Live or start phase (wave 52), the VM must exist.
fn with_jvmti_class<R>(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    stored: impl FnOnce(&SharedVm, &crate::classloading::Class) -> R,
    proxy: impl FnOnce(&SharedVm, ClassId) -> R,
    primitive: impl FnOnce(&str) -> R,
) -> Result<R, JInt> {
    let env = live_env(env)?;
    // Wave 94 (i54 stage 2): every caller is a `phase="start"` function.
    let shared = env.start_or_live_vm().ok_or(ERR_WRONG_PHASE)?;
    match jvmti_class(&shared, klass)? {
        JvmtiClass::Stored(class_id) => {
            let cm = shared.classes.class_manager.read();
            let class = cm.get_class(class_id).ok_or(ERR_INVALID_CLASS)?;
            Ok(stored(&shared, class))
        }
        JvmtiClass::LambdaProxy(id) => Ok(proxy(&shared, id)),
        JvmtiClass::Primitive(name) => Ok(primitive(&name)),
    }
}

// --- Lambda proxy classes (interpreter round i2 wave 55, lane L1) ----------
//
// `docs/feature-designs/jvmti-delivery-threading.md`.
// The VM mints a lambda's class outside the class store
// (`ClassRealm::lambda_proxies`), so [`class_of`] answers `None` for one; the
// class functions answer it as [`JvmtiClass::LambdaProxy`] (wave 57,
// [`with_jvmti_class`]; waves 55-56 on `class_of`'s failure path), with
// HotSpot's facts of the hidden class `InnerClassLambdaMetafactory` spins
// (measured, `L1W55JvmtiLambdaClass`).

/// `ACC_SYNTHETIC | ACC_FINAL | ACC_SUPER`, a lambda class's modifiers
/// (measured `0x1030`).
const LAMBDA_PROXY_MODIFIERS: JInt =
    crate::runtime::lambda_proxy::LAMBDA_PROXY_CLASS_MODIFIERS as JInt;
/// `VERIFIED | PREPARED | INITIALIZED`: a lambda class exists only once its
/// call site was linked (measured `7`).
const LAMBDA_PROXY_STATUS: JInt = 7;
/// The class-file major version of a spun lambda class: the running JDK's
/// latest (`ClassFileFormatVersion.latest()`, 69 for JDK 25; measured).
const LAMBDA_PROXY_MAJOR_VERSION: JInt = 69;

/// Is `id` one of `shared`'s lambda proxy classes?
fn is_lambda_proxy(shared: &SharedVm, id: ClassId) -> bool {
    shared.classes.lambda_proxies.read().contains_key(&id)
}

// --- Primitive classes (interpreter round i2 wave 56, lane L1) -------------
//
// A primitive class's mirror (`int.class`) has no `ClassId`, so
// [`class_of`] answers `None` for one and every class function refused it
// with `JVMTI_ERROR_INVALID_CLASS`. Behind `CRATONVM_JVMTI_C_PRIMITIVE_CLASSES`
// (`DebuggerGates::c_primitive_classes`; [`JvmtiClass::Primitive`] since
// wave 57) they answer HotSpot's facts
// (measured, `L1W56JvmtiPrimitiveClass`): the type's letter, status
// `PRIMITIVE`, `0x411`, no methods, fields or interfaces, neither an
// interface nor an array, the bootstrap loader, no source file and no
// class-file version (101).

/// `public final abstract`, a primitive class's modifiers (measured `0x411`).
const PRIMITIVE_CLASS_MODIFIERS: JInt = 0x0411;

/// The facts of lambda proxy `id` (wave 56: one record,
/// `runtime::lambda_proxy::lambda_proxy_facts`, where wave 55 re-derived
/// each one here): its host, signature, interfaces and methods.
fn lambda_proxy_facts(shared: &SharedVm, id: ClassId) -> Option<LambdaProxyFacts> {
    crate::runtime::lambda_proxy::lambda_proxy_facts(shared, id)
}

/// `GetClassSignature` of lambda proxy `id`: HotSpot's hidden-class
/// signature ([`LambdaProxyFacts::signature`]).
fn lambda_proxy_signature(shared: &SharedVm, id: ClassId) -> Option<String> {
    lambda_proxy_facts(shared, id).map(|facts| facts.signature(id))
}

// --- A lambda proxy's methods (interpreter round i2 wave 56, lane L1) -----
//
// Behind `CRATONVM_JVMTI_C_LAMBDA_METHODS` (`DebuggerGates::c_lambda_methods`).
// A proxy has no `Class` in the store, so its `jmethodID`s are its id and the
// method's index in [`LambdaProxyFacts::methods`] (`(proxy id << 32) |
// index`, as a real class's); the method functions answer one on their
// `JVMTI_ERROR_INVALID_METHODID` path only ([`proxy_method`], cold), with the
// facts HotSpot answers for the spun class's methods (measured,
// `L1W56JvmtiLambdaMethods`): not native, not synthetic, no generic
// signature, no line numbers. The spun code itself is not modelled:
// `GetMethodLocation` and `GetBytecodes` answer
// `JVMTI_ERROR_ABSENT_INFORMATION` where HotSpot answers the code's range and
// bytes (open on the lambda page).

/// The proxy method a `jmethodID` names, with its declaring proxy class;
/// `None` while `DebuggerGates::c_lambda_methods` is off, and for any id that
/// names no proxy method.
#[cold]
#[inline(never)]
fn proxy_method(shared: &SharedVm, method: u64) -> Option<(ClassId, LambdaProxyMethod)> {
    if !shared.debug.debugger_gates.c_lambda_methods() {
        return None;
    }
    // Cast: the id's halves, as `jni::decode_method_id` reads them.
    let class_id = ClassId::new((method >> 32) as u32);
    // Cast: the low 32 bits; a proxy declares a handful of methods.
    let index = (method & 0xFFFF_FFFF) as usize;
    if !is_lambda_proxy(shared, class_id) {
        return None;
    }
    let facts = lambda_proxy_facts(shared, class_id)?;
    let m = facts.methods.into_iter().nth(index)?;
    Some((class_id, m))
}

/// `GetClassMethods`' ids of lambda proxy class `id`, in the spun class
/// file's order; `None` while `DebuggerGates::c_lambda_methods` is off or
/// the proxy's facts are not known.
#[cold]
#[inline(never)]
fn lambda_proxy_method_ids(shared: &SharedVm, id: ClassId) -> Option<Vec<u64>> {
    if !shared.debug.debugger_gates.c_lambda_methods() {
        return None;
    }
    let count = lambda_proxy_facts(shared, id)?.methods.len();
    let ids = (0..count)
        .filter_map(|index| u32::try_from(index).ok())
        .map(|index| (u64::from(id.as_u32()) << 32) | u64::from(index))
        .collect();
    // Positive control: `CRATONVM_FRAME_TRACE=1`, as the other-thread rows.
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[JVMTI_C_LAMBDA_METHODS] class={:#x} methods={count}",
            id.as_u32()
        );
    }
    Some(ids)
}

/// `answer`, a method function's answer read from the class store; when that
/// is `None` (no such method) and `method` names a lambda proxy's method,
/// `proxy`'s answer for it instead.
fn or_proxy_method<R>(
    shared: &SharedVm,
    method: u64,
    answer: Option<R>,
    proxy: impl FnOnce(ClassId, &LambdaProxyMethod) -> R,
) -> Option<R> {
    match answer {
        Some(answer) => Some(answer),
        None => proxy_method(shared, method).map(|(class_id, m)| proxy(class_id, &m)),
    }
}

/// Slot 45: `IsModifiableClass(env, klass, is_modifiable_class_ptr)`
/// (interpreter round i2 wave 54, lane L1): `JNI_FALSE` for a primitive,
/// array or hidden class, as `Instrumentation.isModifiableClass` answers
/// (`instrument::class_id_is_modifiable`); no capability, as on HotSpot.
/// Start or live phase.
extern "C" fn is_modifiable_class(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    is_modifiable_class_ptr: *mut u8,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let Some(shared) = env.start_or_live_vm() else {
        return ERR_WRONG_PHASE;
    };
    // A class the store does not hold but HotSpot knows (a primitive class,
    // a lambda proxy, whatever the gates say) is not modifiable (wave 54).
    let class_id = match decode_class(&shared, klass) {
        Some(DecodedClass::Stored(class_id)) => class_id,
        Some(_) => return write_jboolean(is_modifiable_class_ptr, Ok(false)),
        None => return ERR_INVALID_CLASS,
    };
    let answer = on_calling_thread(&shared, |ctx| {
        crate::runtime::instrument::class_id_is_modifiable(&*ctx, class_id)
    });
    write_jboolean(is_modifiable_class_ptr, answer)
}

/// The `jvmtiError` of a refused redefinition, from libinstrument's
/// throwable for it (`RedefinitionRefusal::instrument_throwable`, the
/// inverse of `JPLISAgent`'s mapping of these errors). Shared with the JDWP
/// server's `VirtualMachine.RedefineClasses` since interpreter round i5 wave
/// 104 (lane L1; `debug::redefine_classes`).
#[cold]
pub(crate) fn redefinition_refusal_error(refusal: &cratonvm_native_api::InstrumentRefusal) -> JInt {
    let message = refusal.message.unwrap_or_default();
    match refusal.throwable {
        "java/lang/ClassFormatError" => 60,
        "java/lang/VerifyError" => 62,
        "java/lang/NoClassDefFoundError" => 69,
        "java/lang/UnsupportedClassVersionError" => 68,
        "java/lang/ClassCircularityError" => 61,
        "java/lang/instrument/UnmodifiableClassException" => ERR_UNMODIFIABLE_CLASS,
        "java/lang/UnsupportedOperationException" => {
            if message.contains("add a method") {
                63
            } else if message.contains("schema") {
                64
            } else if message.contains("superclass or interfaces") {
                66
            } else if message.contains("delete a method") {
                67
            } else if message.contains("class modifiers") {
                70
            } else if message.contains("method modifiers") {
                71
            } else if message.contains("NestHost") {
                72
            } else {
                73
            }
        }
        _ => ERR_INTERNAL,
    }
}

/// `jvmtiClassDefinition`: `{ jclass klass; jint class_byte_count; const
/// unsigned char* class_bytes; }`, 24 bytes with its padding.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct ClassDefinition {
    klass: JObject,
    class_byte_count: JInt,
    class_bytes: *const u8,
}

/// Slot 87: `RedefineClasses(env, class_count, class_definitions)`
/// (interpreter round i2 wave 57, lane L1; `can_redefine_classes`, while
/// `DebuggerGates::c_redefine` is on): each new class file through the
/// whole `ClassFileLoadHook` chain (the C envs and the Java transformers, as
/// a redefinition offers every one of them the bytes), then every class
/// checked and redefined at once (`instrument::redefine_classes_for_jvmti`,
/// `Instrumentation.redefineClasses`' path), on the calling thread, which
/// must be attached. HotSpot's checks, in its order (MEASURED,
/// `L1W57JvmtiRedefine`): the live phase, the capability, a negative count
/// (`JVMTI_ERROR_ILLEGAL_ARGUMENT`), a NULL array (`JVMTI_ERROR_NULL_POINTER`),
/// then per definition an invalid class (`JVMTI_ERROR_INVALID_CLASS`), a NULL
/// class file (`JVMTI_ERROR_NULL_POINTER`) and a primitive, array or hidden
/// class (`JVMTI_ERROR_UNMODIFIABLE_CLASS`), before any hook runs; a refused
/// redefinition answers its error ([`redefinition_refusal_error`]).
extern "C" fn redefine_classes(
    env: *mut JvmtiNativeEnv,
    class_count: JInt,
    class_definitions: *const ClassDefinition,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let Some(shared) = env.vm() else {
        return ERR_WRONG_PHASE;
    };
    if shared.debug.debugger_gates.jvmti_outside_live() {
        return ERR_WRONG_PHASE;
    }
    if !env.possesses(CAN_REDEFINE_CLASSES) {
        return ERR_MUST_POSSESS_CAPABILITY;
    }
    let Ok(count) = usize::try_from(class_count) else {
        return ERR_ILLEGAL_ARGUMENT;
    };
    if class_definitions.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's array of `class_count` definitions.
    let definitions = unsafe { std::slice::from_raw_parts(class_definitions, count) };
    let mut classes: Vec<(ClassId, Vec<u8>)> = Vec::with_capacity(count);
    for definition in definitions {
        let id = match decode_class(&shared, definition.klass) {
            Some(DecodedClass::Stored(id)) => Some(id),
            Some(_) => None,
            None => return ERR_INVALID_CLASS,
        };
        if definition.class_bytes.is_null() {
            return ERR_NULL_POINTER;
        }
        let Some(id) = id else {
            return ERR_UNMODIFIABLE_CLASS;
        };
        let len = usize::try_from(definition.class_byte_count).unwrap_or(0);
        // SAFETY: the agent's class file of `class_byte_count` bytes.
        let bytes = unsafe { std::slice::from_raw_parts(definition.class_bytes, len) }.to_vec();
        classes.push((id, bytes));
    }
    let outcome = on_calling_thread(&shared, |ctx| {
        if classes
            .iter()
            .any(|&(id, _)| !crate::runtime::instrument::class_id_is_modifiable(&*ctx, id))
        {
            return ERR_UNMODIFIABLE_CLASS;
        }
        match crate::runtime::instrument::redefine_classes_for_jvmti(ctx, &classes) {
            None => ERR_NONE,
            Some(refusal) => {
                tracing::warn!("RedefineClasses: {}", refusal.detail);
                redefinition_refusal_error(&refusal)
            }
        }
    });
    match outcome {
        Ok(code) | Err(code) => code,
    }
}

/// Slot 152: `RetransformClasses(env, class_count, classes)` (interpreter
/// round i2 wave 54, lane L1; `can_retransform_classes`, while
/// `DebuggerGates::c_retransform` is on): each class's retransformation base
/// through the retransform-capable `ClassFileLoadHook`s and Java transformers,
/// then every class redefined at once (`instrument::retransform_classes_for_jvmti`),
/// on the calling thread, which must be attached. HotSpot's checks, in its
/// order: the live phase, the capability, a negative count
/// (`JVMTI_ERROR_ILLEGAL_ARGUMENT`), a NULL array (`JVMTI_ERROR_NULL_POINTER`),
/// then per class an invalid one (`JVMTI_ERROR_INVALID_CLASS`) and a
/// primitive, array or hidden one (`JVMTI_ERROR_UNMODIFIABLE_CLASS`), before
/// any hook runs; a refused redefinition answers its error.
extern "C" fn retransform_classes(
    env: *mut JvmtiNativeEnv,
    class_count: JInt,
    classes: *const JObject,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let Some(shared) = env.vm() else {
        return ERR_WRONG_PHASE;
    };
    if shared.debug.debugger_gates.jvmti_outside_live() {
        return ERR_WRONG_PHASE;
    }
    if !env.can_retransform() {
        return ERR_MUST_POSSESS_CAPABILITY;
    }
    let Ok(count) = usize::try_from(class_count) else {
        return ERR_ILLEGAL_ARGUMENT;
    };
    if classes.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's array of `class_count` `jclass`es.
    let handles = unsafe { std::slice::from_raw_parts(classes, count) };
    let mut ids = Vec::with_capacity(count);
    for &klass in handles {
        match decode_class(&shared, klass) {
            Some(DecodedClass::Stored(id)) => ids.push(id),
            Some(_) => return ERR_UNMODIFIABLE_CLASS,
            None => return ERR_INVALID_CLASS,
        }
    }
    let outcome = on_calling_thread(&shared, |ctx| {
        if ids
            .iter()
            .any(|&id| !crate::runtime::instrument::class_id_is_modifiable(&*ctx, id))
        {
            return ERR_UNMODIFIABLE_CLASS;
        }
        match crate::runtime::instrument::retransform_classes_for_jvmti(ctx, &ids) {
            None => ERR_NONE,
            Some(refusal) => {
                tracing::warn!("RetransformClasses: {}", refusal.detail);
                redefinition_refusal_error(&refusal)
            }
        }
    });
    match outcome {
        Ok(code) | Err(code) => code,
    }
}

/// Slot 55: `IsInterface(env, klass, is_interface_ptr)`.
extern "C" fn is_interface(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    is_interface_ptr: *mut u8,
) -> JInt {
    // Wave 55: a lambda proxy class is neither an interface nor an array.
    // Wave 56: so is a primitive class.
    let answer = with_jvmti_class(
        env,
        klass,
        |_, class| class.access_flags.bits() & ACC_INTERFACE != 0,
        |_, _| false,
        |_| false,
    );
    write_jboolean(is_interface_ptr, answer)
}

/// Slot 56: `IsArrayClass(env, klass, is_array_class_ptr)`.
extern "C" fn is_array_class(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    is_array_class_ptr: *mut u8,
) -> JInt {
    let answer = with_jvmti_class(
        env,
        klass,
        |_, class| class.name.starts_with('['),
        |_, _| false,
        |_| false,
    );
    write_jboolean(is_array_class_ptr, answer)
}

/// Slot 57: `GetClassLoader(env, klass, classloader_ptr)`: the class's
/// defining loader as the program sees it (`Class.getClassLoader0`,
/// `debug::inspect::class_loader_of`), NULL for the bootstrap loader, as a
/// local reference. Asked on the calling thread, which must be attached.
extern "C" fn get_class_loader(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    classloader_ptr: *mut JObject,
) -> JInt {
    let shared = match live_env(env).and_then(|e| e.start_or_live_vm().ok_or(ERR_WRONG_PHASE)) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let class_id = match jvmti_class(&shared, klass) {
        Ok(JvmtiClass::Stored(class_id)) => Some(class_id),
        // Wave 55: a lambda proxy class is its host's loader's (its defining
        // loader, as the hidden class's lookup class's); NULL when the host
        // is not known.
        Ok(JvmtiClass::LambdaProxy(proxy)) => {
            lambda_proxy_facts(&shared, proxy).and_then(|facts| facts.host_id)
        }
        // Wave 56: a primitive class is the bootstrap loader's (NULL).
        Ok(JvmtiClass::Primitive(_)) => None,
        Err(code) => return code,
    };
    if classloader_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let loader = on_calling_thread(&shared, |ctx| {
        class_id
            .and_then(|class_id| {
                crate::debug::inspect::class_loader_of(&shared, ctx.thread, class_id)
            })
            .map_or(0, jni::new_local_handle)
    });
    match loader {
        Ok(loader) => {
            // SAFETY: the agent's non-null out-parameter.
            unsafe { *classloader_ptr = loader };
            ERR_NONE
        }
        Err(code) => code,
    }
}

/// Slot 145: `GetClassVersionNumbers(env, klass, minor_version_ptr,
/// major_version_ptr)`: `JVMTI_ERROR_ABSENT_INFORMATION` for an array or a
/// primitive class.
extern "C" fn get_class_version_numbers(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    minor_version_ptr: *mut JInt,
    major_version_ptr: *mut JInt,
) -> JInt {
    let version = with_jvmti_class(
        env,
        klass,
        |_, class| {
            (!class.name.starts_with('[') && !is_primitive_class_name(&class.name)).then(|| {
                (
                    JInt::from(class.version.minor),
                    JInt::from(class.version.major),
                )
            })
        },
        // Wave 55: a lambda proxy class.
        |_, _| Some((0, LAMBDA_PROXY_MAJOR_VERSION)),
        // Wave 56: a primitive class has no class file (measured 101).
        |_| None,
    );
    let version = match version {
        Ok(version) => version,
        Err(code) => return code,
    };
    if minor_version_ptr.is_null() || major_version_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let Some((minor, major)) = version else {
        return ERR_ABSENT_INFORMATION;
    };
    // SAFETY: the agent's non-null out-parameters.
    unsafe {
        *minor_version_ptr = minor;
        *major_version_ptr = major;
    }
    ERR_NONE
}

// --- Fields (slots 60-63) ---------------------------------------------------

/// `read` of the field a `jfieldID` names in `klass` (its declaring class),
/// or the error to answer. An id with [`STATIC_FIELD_ID`] names a static
/// field; one without it names the instance field of that slot, else (an
/// unmarked id `FromReflectedField` made of a static field) the static field
/// of that index.
fn with_field<R>(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    field: u64,
    read: impl FnOnce(&crate::classloading::Class, &cratonvm_reader::field::ClassFileField) -> R,
) -> Result<R, JInt> {
    let env = live_env(env)?;
    // Wave 94 (i54 stage 2): the field facts are `phase="start"`, the field
    // watches (`events_more::watched_field`) live-only.
    let shared = env.start_or_live_vm().ok_or(ERR_WRONG_PHASE)?;
    let class_id = class_of(&shared, klass).ok_or(ERR_INVALID_CLASS)?;
    // Cast: the id's class half, without the static mark.
    let declaring = ClassId::new(((field & !STATIC_FIELD_ID) >> 32) as u32);
    if field == 0 || declaring != class_id {
        return Err(ERR_INVALID_FIELDID);
    }
    let index = usize::try_from(field & 0xFFFF_FFFF).map_err(|_| ERR_INVALID_FIELDID)?;
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(class_id).ok_or(ERR_INVALID_CLASS)?;
    let nth_static = || class.fields.iter().filter(|f| f.is_static()).nth(index);
    let found = if field & STATIC_FIELD_ID != 0 {
        nth_static()
    } else {
        class.field_at_index(index).or_else(nth_static)
    };
    let found = found.ok_or(ERR_INVALID_FIELDID)?;
    Ok(read(class, found))
}

/// `(proxy id, name, descriptor)` of capture field `field` of `klass` when
/// `klass` is a lambda proxy class (the i54-L1 retirement): its `arg$N`
/// fields' ids are `(proxy id << 32) | capture index` -- the capture's field
/// slot in the proxy object, which JNI's `Get<Type>Field` reads
/// (`jni::decode_field_id`). `None` for any other class. Cold: asked on
/// [`with_field`]'s error path only.
#[cold]
#[inline(never)]
fn proxy_field(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    field: u64,
) -> Option<Result<(ClassId, String, String), JInt>> {
    let shared = live_env(env).ok()?.vm()?;
    let JvmtiClass::LambdaProxy(id) = jvmti_class(&shared, klass).ok()? else {
        return None;
    };
    // Cast: the id's class half, whole: a proxy's id has bit 31 set
    // (`LAMBDA_PROXY_ID_BASE`), so its field ids carry the bit that marks a
    // stored class's static field ([`STATIC_FIELD_ID`]); a proxy declares none.
    if (field >> 32) as u32 != id.as_u32() {
        return Some(Err(ERR_INVALID_FIELDID));
    }
    let Ok(index) = usize::try_from(field & 0xFFFF_FFFF) else {
        return Some(Err(ERR_INVALID_FIELDID));
    };
    let Some(facts) = lambda_proxy_facts(&shared, id) else {
        return Some(Err(ERR_INVALID_CLASS));
    };
    Some(
        facts
            .fields
            .into_iter()
            .nth(index)
            .map(|(name, descriptor)| (id, name, descriptor))
            .ok_or(ERR_INVALID_FIELDID),
    )
}

/// `answer`, or on its `JVMTI_ERROR_INVALID_CLASS` or `INVALID_FIELDID` (the
/// proxy's id names no stored class) the answer `proxy` gives for a lambda
/// proxy's capture field ([`proxy_field`]).
fn or_proxy_field<R>(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    field: u64,
    answer: Result<R, JInt>,
    proxy: impl FnOnce(ClassId, String, String) -> R,
) -> Result<R, JInt> {
    match answer {
        Err(code @ (ERR_INVALID_CLASS | ERR_INVALID_FIELDID)) => {
            match proxy_field(env, klass, field) {
                Some(found) => found.map(|(id, name, descriptor)| proxy(id, name, descriptor)),
                None => Err(code),
            }
        }
        other => other,
    }
}

/// Slot 60: `GetFieldName(env, klass, field, name_ptr, signature_ptr,
/// generic_ptr)`: any out-parameter may be NULL; the generic signature is
/// the field's `Signature` attribute, NULL without one.
extern "C" fn get_field_name(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    field: u64,
    name_ptr: *mut *mut u8,
    signature_ptr: *mut *mut u8,
    generic_ptr: *mut *mut u8,
) -> JInt {
    let names = with_field(env, klass, field, |_, f| {
        (
            f.name.to_string(),
            f.descriptor.to_string(),
            crate::debug::signature_attribute(&f.attributes),
        )
    });
    let names = or_proxy_field(env, klass, field, names, |_, name, descriptor| {
        (name, descriptor, None)
    });
    let (name, signature, generic) = match names {
        Ok(names) => names,
        Err(code) => return code,
    };
    let out = [
        (name_ptr, Some(name.as_str())),
        (signature_ptr, Some(signature.as_str())),
        (generic_ptr, generic.as_deref()),
    ];
    if write_modified_utf8_outs(&out) {
        ERR_NONE
    } else {
        ERR_OUT_OF_MEMORY
    }
}

/// Slot 61: `GetFieldDeclaringClass(env, klass, field, declaring_class_ptr)`.
extern "C" fn get_field_declaring_class(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    field: u64,
    declaring_class_ptr: *mut JObject,
) -> JInt {
    let declaring = with_field(env, klass, field, |class, _| class.id);
    let declaring = match or_proxy_field(env, klass, field, declaring, |id, _, _| id) {
        Ok(id) => id,
        Err(code) => return code,
    };
    if declaring_class_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *declaring_class_ptr = jni::class_id_to_jclass(declaring) };
    ERR_NONE
}

/// Slot 62: `GetFieldModifiers(env, klass, field, modifiers_ptr)`: the
/// field's access flags that JVMTI recognises
/// (`JVM_RECOGNIZED_FIELD_MODIFIERS`, synthetic and enum included).
extern "C" fn get_field_modifiers(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    field: u64,
    modifiers_ptr: *mut JInt,
) -> JInt {
    let modifiers = with_field(env, klass, field, |_, f| {
        JInt::from(f.access_flags.bits()) & crate::debug::JVM_RECOGNIZED_FIELD_MODIFIERS as JInt
    });
    let modifiers = or_proxy_field(env, klass, field, modifiers, |_, _, _| {
        JInt::from(crate::runtime::lambda_proxy::LAMBDA_PROXY_FIELD_MODIFIERS)
    });
    write_jint(modifiers_ptr, modifiers)
}

/// Slot 63: `IsFieldSynthetic(env, klass, field, is_synthetic_ptr)`
/// (`can_get_synthetic_attribute`): `ACC_SYNTHETIC` or a `Synthetic`
/// attribute.
extern "C" fn is_field_synthetic(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    field: u64,
    is_synthetic_ptr: *mut u8,
) -> JInt {
    match live_env(env) {
        Ok(e) if !e.possesses(CAN_GET_SYNTHETIC_ATTRIBUTE) => return ERR_MUST_POSSESS_CAPABILITY,
        Ok(_) => {}
        Err(code) => return code,
    }
    let answer = with_field(env, klass, field, |_, f| {
        f.access_flags.bits() & ACC_SYNTHETIC != 0
            || crate::debug::has_synthetic_attribute(&f.attributes)
    });
    // A capture field is not synthetic (measured).
    let answer = or_proxy_field(env, klass, field, answer, |_, _, _| false);
    write_jboolean(is_synthetic_ptr, answer)
}

// --- Methods (slots 68, 69, 77) ---------------------------------------------

/// Slot 68: `GetMaxLocals(env, method, max_ptr)`: the `Code` attribute's
/// `max_locals`; `JVMTI_ERROR_NATIVE_METHOD` for a native method, 0 for an
/// abstract one.
extern "C" fn get_max_locals(env: *mut JvmtiNativeEnv, method: u64, max_ptr: *mut JInt) -> JInt {
    let shared = match live_env(env).and_then(|e| e.start_or_live_vm().ok_or(ERR_WRONG_PHASE)) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let answer = with_method(&shared, method, |_, m| {
        if m.is_native() {
            Err(ERR_NATIVE_METHOD)
        } else {
            Ok(m.code().map_or(0, |code| JInt::from(code.max_locals)))
        }
    });
    // Wave 56: a lambda proxy's method (its code stores no local, measured).
    let answer = or_proxy_method(&shared, method, answer, |_, m| Ok(JInt::from(m.args_size)))
        .unwrap_or(Err(ERR_INVALID_METHODID));
    write_jint(max_ptr, answer)
}

/// Slot 69: `GetArgumentsSize(env, method, size_ptr)`: the local slots the
/// arguments take (`this` included, a `long` or a `double` two);
/// `JVMTI_ERROR_NATIVE_METHOD` for a native method.
extern "C" fn get_arguments_size(
    env: *mut JvmtiNativeEnv,
    method: u64,
    size_ptr: *mut JInt,
) -> JInt {
    let shared = match live_env(env).and_then(|e| e.start_or_live_vm().ok_or(ERR_WRONG_PHASE)) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let answer = with_method(&shared, method, |_, m| {
        if m.is_native() {
            return Err(ERR_NATIVE_METHOD);
        }
        let slots = cratonvm_reader::method_descriptor::MethodDescriptor::parse(&m.descriptor)
            .map_err(|_| ERR_INVALID_METHODID)?
            .parameters_stack_slots();
        let this = usize::from(!m.is_static());
        JInt::try_from(slots + this).map_err(|_| ERR_INTERNAL)
    });
    // Wave 56: a lambda proxy's method.
    let answer = or_proxy_method(&shared, method, answer, |_, m| Ok(JInt::from(m.args_size)))
        .unwrap_or(Err(ERR_INVALID_METHODID));
    write_jint(size_ptr, answer)
}

/// Slot 77: `IsMethodSynthetic(env, method, is_synthetic_ptr)`
/// (`can_get_synthetic_attribute`).
extern "C" fn is_method_synthetic(
    env: *mut JvmtiNativeEnv,
    method: u64,
    is_synthetic_ptr: *mut u8,
) -> JInt {
    let shared = match live_env(env) {
        Ok(e) if !e.possesses(CAN_GET_SYNTHETIC_ATTRIBUTE) => return ERR_MUST_POSSESS_CAPABILITY,
        Ok(e) => match e.start_or_live_vm() {
            Some(shared) => shared,
            None => return ERR_WRONG_PHASE,
        },
        Err(code) => return code,
    };
    let answer = with_method(&shared, method, |_, m| {
        m.access_flags.bits() & ACC_SYNTHETIC != 0
            || crate::debug::has_synthetic_attribute(&m.attributes)
    });
    // Wave 56: a lambda proxy's method (never `ACC_SYNTHETIC`, measured).
    let answer = or_proxy_method(&shared, method, answer, |_, m| {
        m.access_flags & ACC_SYNTHETIC != 0
    })
    .ok_or(ERR_INVALID_METHODID);
    write_jboolean(is_synthetic_ptr, answer)
}

// --- Thread CPU time, thread-group children, obsolete methods (interpreter
// round i2 wave 56, lane L1) ------------------------------------------------
//
// `docs/feature-designs/jvmti-delivery-threading.md`.
// Each answer HotSpot 25.0.3's, measured (`L1W56JvmtiThreadTimes`).

/// `can_get_current_thread_cpu_time`, the 23rd bit field, and
/// `can_get_thread_cpu_time`, the 24th: potential in the OnLoad and the live
/// phase (measured).
const CAN_GET_CURRENT_THREAD_CPU_TIME: u32 = 1 << 22;
const CAN_GET_THREAD_CPU_TIME: u32 = 1 << 23;

/// `jvmtiTimerInfo`: `{ jlong max_value; jboolean may_skip_forward;
/// jboolean may_skip_backward; jvmtiTimerKind kind; jlong reserved1; jlong
/// reserved2; }`, 32 bytes.
#[repr(C)]
struct TimerInfoRow {
    max_value: i64,
    may_skip_forward: u8,
    may_skip_backward: u8,
    kind: JInt,
    reserved1: i64,
    reserved2: i64,
}

/// `JVMTI_TIMER_TOTAL_CPU`: user and system time.
const TIMER_TOTAL_CPU: JInt = 31;

/// The env behind `env` with VM, when it holds `capability` (word 0):
/// `JVMTI_ERROR_MUST_POSSESS_CAPABILITY` without it (measured: an env
/// obtained in the live phase that did not add it gets 99),
/// `JVMTI_ERROR_WRONG_PHASE` before the VM is bound.
fn cpu_time_env(env: *mut JvmtiNativeEnv, capability: u32) -> Result<Arc<SharedVm>, JInt> {
    let env = live_env(env)?;
    if !env.possesses(capability) {
        return Err(ERR_MUST_POSSESS_CAPABILITY);
    }
    // Wave 94 (i54 stage 2): the current thread's CPU time is
    // `phase="start"`, another thread's live-only.
    env.start_or_live_vm().ok_or(ERR_WRONG_PHASE)
}

/// Slots 134 / 136: `GetCurrentThreadCpuTimerInfo` /
/// `GetThreadCpuTimerInfo(env, info_ptr)`: a total-CPU timer of 64 bits that
/// skips neither way (measured: `max=-1 forward=0 backward=0 kind=31`).
fn cpu_timer_info(env: *mut JvmtiNativeEnv, capability: u32, info_ptr: *mut TimerInfoRow) -> JInt {
    if let Err(code) = cpu_time_env(env, capability) {
        return code;
    }
    if info_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe {
        info_ptr.write(TimerInfoRow {
            max_value: -1,
            may_skip_forward: 0,
            may_skip_backward: 0,
            kind: TIMER_TOTAL_CPU,
            reserved1: 0,
            reserved2: 0,
        });
    }
    ERR_NONE
}

extern "C" fn get_current_thread_cpu_timer_info(
    env: *mut JvmtiNativeEnv,
    info_ptr: *mut TimerInfoRow,
) -> JInt {
    cpu_timer_info(env, CAN_GET_CURRENT_THREAD_CPU_TIME, info_ptr)
}

extern "C" fn get_thread_cpu_timer_info(
    env: *mut JvmtiNativeEnv,
    info_ptr: *mut TimerInfoRow,
) -> JInt {
    cpu_timer_info(env, CAN_GET_THREAD_CPU_TIME, info_ptr)
}

/// Does `thread` -- or, for NULL, the calling thread -- name a virtual
/// thread (interpreter round i2 wave 58, lane L1)? HotSpot answers the CPU
/// time functions `JVMTI_ERROR_UNSUPPORTED_OPERATION` for one (MEASURED,
/// `tools/probes/interp/L1/L1W58JvmtiVirtualCpuTime.java`). The handle is
/// read inside the caller's foreign-thread entry and `shared`'s JNI context,
/// as [`lookup_thread`] reads it; a handle that names no object is left to
/// the caller's own checks.
fn names_virtual_thread(shared: &SharedVm, thread: JObject) -> bool {
    if thread == 0 {
        return jni::current_jvm_thread_of(shared)
            // SAFETY: the calling thread's own `JvmThread`; `kind` is a
            // plain field it does not change while it runs this call.
            .is_some_and(|current| unsafe {
                matches!(
                    (*current).kind,
                    crate::threading::jvm_thread::ThreadKind::Virtual
                )
            });
    }
    let _fx = jni::ForeignJniEntry::enter();
    let prev = jni::replace_jni_context(shared);
    let obj = jni::jobject_to_obj(thread);
    jni::restore_jni_context(prev);
    obj.is_some_and(|obj| {
        let class_id = shared.mem.heap.class_id_of(obj);
        shared
            .classes
            .class_manager
            .read()
            .is_subclass_of_by_name(class_id, "java/lang/BaseVirtualThread")
    })
}

/// The calling thread's CPU time (user and system), in nanoseconds.
fn current_thread_cpu_nanos() -> Option<i64> {
    cratonvm_jit::tiered::current_thread_cpu_time()
        .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

/// Slot 135: `GetCurrentThreadCpuTime(env, nanos_ptr)`
/// (`can_get_current_thread_cpu_time`).
extern "C" fn get_current_thread_cpu_time(env: *mut JvmtiNativeEnv, nanos_ptr: *mut i64) -> JInt {
    let shared = match cpu_time_env(env, CAN_GET_CURRENT_THREAD_CPU_TIME) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    // Wave 58 (lane L1): a virtual thread asking is `UNSUPPORTED_OPERATION`,
    // as on HotSpot (`L1W58JvmtiVirtualCpuTime`).
    if names_virtual_thread(&shared, 0) {
        return ERR_UNSUPPORTED_OPERATION;
    }
    if nanos_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let Some(nanos) = current_thread_cpu_nanos() else {
        return ERR_INTERNAL;
    };
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *nanos_ptr = nanos };
    ERR_NONE
}

/// Slot 137: `GetThreadCpuTime(env, thread, nanos_ptr)`
/// (`can_get_thread_cpu_time`): a NULL `thread` or the calling thread reads
/// the calling thread's clock, another live thread its OS thread's
/// (`ThreadRegistry::os_tid_of`, read as `ThreadMXBean.getThreadCpuTime`
/// reads it); a thread not started or terminated is
/// `JVMTI_ERROR_THREAD_NOT_ALIVE`, an object that is no thread
/// `JVMTI_ERROR_INVALID_THREAD` (measured).
extern "C" fn get_thread_cpu_time(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    nanos_ptr: *mut i64,
) -> JInt {
    let shared = match cpu_time_env(env, CAN_GET_THREAD_CPU_TIME) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    // Wave 58 (lane L1): a virtual thread -- the handle's, or the calling
    // one for NULL -- is `UNSUPPORTED_OPERATION`, before the out-parameter
    // check, as HotSpot answers it (`L1W58JvmtiVirtualCpuTime`).
    if names_virtual_thread(&shared, thread) {
        return ERR_UNSUPPORTED_OPERATION;
    }
    if nanos_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let current = jni::current_jvm_thread_of(&shared)
        // SAFETY: the calling thread's own `JvmThread`; `thread_id` is `Copy`.
        .map(|current| unsafe { (*current).thread_id.0 });
    let tid = if thread == 0 {
        None
    } else {
        match lookup_thread(&shared, thread) {
            Ok(ThreadLookup::Live(tid)) => Some(tid).filter(|&tid| Some(tid) != current),
            Ok(ThreadLookup::Unstarted | ThreadLookup::Dead) => return ERR_THREAD_NOT_ALIVE,
            Err(code) => return code,
        }
    };
    let nanos = match tid {
        None => current_thread_cpu_nanos(),
        Some(tid) => shared
            .threads
            .thread_registry
            .os_tid_of(ThreadId(tid))
            .and_then(cratonvm_native_builtins::phases_late::management::os_thread_cpu_time_ns)
            .map(|(cpu, _)| cpu),
    };
    let Some(nanos) = nanos else {
        return ERR_INTERNAL;
    };
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *nanos_ptr = nanos };
    ERR_NONE
}

/// Slot 15: `GetThreadGroupChildren(env, group, thread_count_ptr,
/// threads_ptr, group_count_ptr, groups_ptr)`: the live threads whose group
/// is `group` and its subgroups (`ThreadGroup.subgroups()`'s, as JDWP's
/// `ThreadGroupReference.Children` lists them), as local references in two
/// `Allocate` blocks. A handle that names no thread group answers
/// `JVMTI_ERROR_INTERNAL`, as HotSpot answers it for a `Thread` (measured).
extern "C" fn get_thread_group_children(
    env: *mut JvmtiNativeEnv,
    group: JObject,
    thread_count_ptr: *mut JInt,
    threads_ptr: *mut *mut JObject,
    group_count_ptr: *mut JInt,
    groups_ptr: *mut *mut JObject,
) -> JInt {
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    if thread_count_ptr.is_null()
        || threads_ptr.is_null()
        || group_count_ptr.is_null()
        || groups_ptr.is_null()
    {
        return ERR_NULL_POINTER;
    }
    let answer = on_calling_thread(&shared, |ctx| {
        let group = jni::jobject_to_obj(group)
            .filter(|&obj| is_thread_group_object(&shared, obj))
            .ok_or(ERR_INTERNAL)?;
        // Interpreter round i5 wave 102, lane L1: platform threads only, in
        // attach order ([`live_platform_thread_objects`]).
        let threads: Vec<JObject> = live_platform_thread_objects(&shared)
            .into_iter()
            .filter(|&t| {
                crate::debug::inspect::group_of_thread(ctx, t)
                    .is_some_and(|g| g.as_ptr() == group.as_ptr())
            })
            .map(jni::new_local_handle)
            .collect();
        let groups: Vec<JObject> = crate::debug::inspect::group_subgroups(&shared, ctx, group)
            .into_iter()
            .map(jni::new_local_handle)
            .collect();
        Ok((threads, groups))
    });
    let (threads, groups) = match answer {
        Ok(Ok(children)) => children,
        Ok(Err(code)) | Err(code) => return code,
    };
    match write_id_array(thread_count_ptr, threads_ptr, &threads) {
        ERR_NONE => {}
        code => return code,
    }
    write_id_array(group_count_ptr, groups_ptr, &groups)
}

/// Slot 91: `IsMethodObsolete(env, method, is_obsolete_ptr)`: false for
/// every method a `jmethodID` names, which is always its class's current
/// method of that name and descriptor (as JDWP's `Method.IsObsolete`
/// answers for every id but the null one); a lambda proxy's too.
extern "C" fn is_method_obsolete(
    env: *mut JvmtiNativeEnv,
    method: u64,
    is_obsolete_ptr: *mut u8,
) -> JInt {
    let shared = match live_env(env).and_then(|e| e.start_or_live_vm().ok_or(ERR_WRONG_PHASE)) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let known = with_method(&shared, method, |_, _| ());
    let answer = or_proxy_method(&shared, method, known, |_, _| ())
        .map(|()| false)
        .ok_or(ERR_INVALID_METHODID);
    write_jboolean(is_obsolete_ptr, answer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::frame::Frame;
    use crate::types::Value;
    use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    /// `set` less the capabilities of `less`.
    fn without(set: Caps, less: Caps) -> Caps {
        let mut out = set;
        for (word, drop) in out.iter_mut().zip(less) {
            *word &= !drop;
        }
        out
    }

    static HITS: AtomicUsize = AtomicUsize::new(0);
    static HITS_AT_9: AtomicUsize = AtomicUsize::new(0);
    static HITS_WITH_JNI_ENV: AtomicUsize = AtomicUsize::new(0);

    // No assertion in here: a panic cannot leave an `extern "C"` function.
    unsafe extern "C" fn on_breakpoint(
        _env: *mut JvmtiNativeEnv,
        jni_env: JNIEnv,
        _thread: JObject,
        _method: u64,
        location: i64,
    ) {
        HITS.fetch_add(1, Ordering::SeqCst);
        if location == 9 {
            HITS_AT_9.fetch_add(1, Ordering::SeqCst);
        }
        if !jni_env.is_null() {
            HITS_WITH_JNI_ENV.fetch_add(1, Ordering::SeqCst);
        }
    }

    type GetEnvFn = extern "C" fn(jni::JavaVM, *mut *mut c_void, JInt) -> JInt;
    type Caps = [u32; CAPABILITY_WORDS];

    /// Function `n` (1-based, as `jvmti.h` numbers them) of a `jvmtiEnv*`.
    fn function(env: *mut c_void, n: usize) -> usize {
        // SAFETY: a `jvmtiEnv*` points at its function-table pointer, and
        // the table has `JVMTI_FUNCTION_COUNT` entries.
        unsafe { *(*(env as *const *const usize)).add(n - 1) }
    }

    /// `GetEnv` through the invocation table (slot 6).
    fn get_env(version: JInt) -> (JInt, *mut c_void) {
        let java_vm = jni::get_java_vm();
        // SAFETY: the invocation table's slot 6 is `GetEnv`.
        let get_env: GetEnvFn =
            unsafe { std::mem::transmute::<usize, GetEnvFn>(*(*java_vm).add(6)) };
        let mut env: *mut c_void = std::ptr::null_mut();
        let code = get_env(java_vm, &mut env, version);
        (code, env)
    }

    /// A VM fixture with the weak self-reference `Vm::new()` installs (the
    /// delivery path reaches `SharedVm::get_arc`) and its JVMTI manager.
    fn vm_fixture() -> Arc<SharedVm> {
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        *shared.self_arc.write() = Some(Arc::downgrade(&shared));
        let vm = shared.vm_identity;
        rt::install_manager_for_vm(vm, Arc::new(rt::JvmtiEventManager::new_for_vm(vm)));
        // As if a startup agent had acquired the OnLoad-only capabilities
        // (wave 46): every test's live-phase env may add [`POTENTIAL`].
        shared.debug.jvmti_env.lock().c_table.onload_acquired = ONLOAD_ONLY;
        shared
    }

    /// Wave 46: in the live phase the three capabilities of the local and
    /// monitor functions are potential only as far as a startup env acquired
    /// them ([`ONLOAD_ONLY`], HotSpot 25.0.3's answer), each on its own.
    #[test]
    fn the_onload_only_capabilities_follow_what_a_startup_env_acquired() {
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        shared.debug.jvmti_env.lock().c_table.onload_acquired = [0; CAPABILITY_WORDS];
        jni::set_jni_context_arc(Arc::clone(&shared));
        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        // SAFETY (every transmute below): the slot's `jvmti.h` signature.
        let potential: extern "C" fn(*mut c_void, *mut Caps) -> JInt =
            unsafe { std::mem::transmute(function(env, 140)) };
        let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
            unsafe { std::mem::transmute(function(env, 142)) };
        let dispose: extern "C" fn(*mut c_void) -> JInt =
            unsafe { std::mem::transmute(function(env, 127)) };
        let locals: Caps = [CAN_ACCESS_LOCAL_VARIABLES, 0, 0, 0];
        let mut caps: Caps = [0; CAPABILITY_WORDS];
        assert_eq!(potential(env, &mut caps), ERR_NONE);
        for (have, only) in caps.iter().zip(ONLOAD_ONLY) {
            assert_eq!(have & only, 0, "none acquired: none potential");
        }
        assert_eq!(add(env, &locals), ERR_NOT_AVAILABLE);
        shared.debug.jvmti_env.lock().c_table.onload_acquired = locals;
        assert_eq!(potential(env, &mut caps), ERR_NONE);
        assert_eq!(
            caps[0] & CAN_ACCESS_LOCAL_VARIABLES,
            CAN_ACCESS_LOCAL_VARIABLES
        );
        assert_eq!(caps[0] & CAN_GET_CURRENT_CONTENDED_MONITOR, 0);
        assert_eq!(caps[1] & CAN_GET_OWNED_MONITOR_STACK_DEPTH_INFO, 0);
        assert_eq!(add(env, &locals), ERR_NONE);
        assert_eq!(dispose(env), ERR_NONE);
        rt::forget_vm_jvmti_state(shared.vm_identity);
        jni::clear_jni_context();
    }

    /// Interpreter round i6 wave 132, lane L1 (i126-L1;
    /// `CRATONVM_JVMTI_C_AGENT_KEEPS_LOCALS_ALIVE`): a startup agent that
    /// acquired `can_access_local_variables` in `Agent_OnLoad` makes every
    /// local of the VM it is bound to a root (`debugger_observes_locals`),
    /// as a JDWP port does; one that did not leaves the VM's answer false;
    /// a live-phase env's add (offered because a startup env acquired it,
    /// `vm_fixture`) does not set it.
    #[test]
    fn a_startup_agent_with_local_access_makes_every_local_a_root() {
        let _lock = rt::jvmti_registry_test_lock();
        let observes =
            |shared: &Arc<SharedVm>| crate::runtime::jvmti::debugger_observes_locals(shared);
        for wants_locals in [false, true] {
            let shared = vm_fixture();
            let vm = shared.vm_identity;
            jni::set_jni_context_arc(Arc::clone(&shared));
            let mut slot: Option<*mut c_void> = None;
            let envs = collect_startup_envs(|| {
                let (code, env) = get_env(0x3001_0200);
                assert_eq!(code, jni::JNI_OK);
                // SAFETY: the slot's `jvmti.h` signature.
                let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
                    unsafe { std::mem::transmute(function(env, 142)) };
                let caps: Caps = if wants_locals {
                    [CAN_ACCESS_LOCAL_VARIABLES | CAN_SUSPEND, 0, 0, 0]
                } else {
                    [CAN_SUSPEND, 0, 0, 0]
                };
                assert_eq!(add(env, &caps), ERR_NONE);
                slot = Some(env);
            });
            let env = slot.expect("the agent obtained an env");
            if crate::config::JDWP_SERVER_COMPILED_IN {
                assert!(shared.config.jdwp_port.is_none(), "no JDWP port");
            }
            assert!(!observes(&shared), "not before the bind");
            shared.debug.jvmti_env.lock().c_table.startup_native_envs = envs;
            bind_startup_envs(&shared);
            let expected = wants_locals && shared.debug.debugger_gates.c_agent_keeps_locals_alive();
            assert_eq!(observes(&shared), expected, "wants_locals={wants_locals}");
            // A live-phase env adding it changes nothing.
            if !wants_locals {
                let (code, live) = get_env(0x3001_0200);
                assert_eq!(code, jni::JNI_OK);
                // SAFETY (both transmutes): the slot's `jvmti.h` signature.
                let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
                    unsafe { std::mem::transmute(function(live, 142)) };
                let dispose: extern "C" fn(*mut c_void) -> JInt =
                    unsafe { std::mem::transmute(function(live, 127)) };
                shared.raise_vm_init_level(3);
                let _ = add(live, &[CAN_ACCESS_LOCAL_VARIABLES, 0, 0, 0]);
                assert!(!observes(&shared), "a live-phase add is not a start fact");
                assert_eq!(dispose(live), ERR_NONE);
            }
            // SAFETY: the slot's `jvmti.h` signature.
            let dispose: extern "C" fn(*mut c_void) -> JInt =
                unsafe { std::mem::transmute(function(env, 127)) };
            assert_eq!(dispose(env), ERR_NONE);
            assert_eq!(observes(&shared), expected, "never undone");
            rt::forget_vm_jvmti_state(vm);
            jni::clear_jni_context();
        }
    }

    /// JIT round 21 wave 24 (lane flipprep, flip review item 2): the per-VM
    /// answer the fresh-allocation volatile-store fence elision declines on
    /// follows what this VM's startup envs acquired of
    /// `can_access_local_variables` only (another VM's record, or another
    /// OnLoad capability, does not count), and is `true` while the VM's
    /// JVMTI lock is held (fail-closed).
    #[test]
    fn flipprep_locals_capability_may_be_held_is_per_vm_and_fail_closed() {
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let other = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        assert!(!locals_capability_may_be_held(&shared), "no agent: no capability");
        shared.debug.jvmti_env.lock().c_table.onload_acquired =
            [CAN_GET_OWNED_MONITOR_INFO, 0, 0, 0];
        assert!(!locals_capability_may_be_held(&shared), "another OnLoad capability");
        shared.debug.jvmti_env.lock().c_table.onload_acquired =
            [CAN_ACCESS_LOCAL_VARIABLES, 0, 0, 0];
        assert!(locals_capability_may_be_held(&shared));
        assert!(!locals_capability_may_be_held(&other), "per VM");
        let held = other.debug.jvmti_env.lock();
        assert!(locals_capability_may_be_held(&other), "lock held: fail-closed");
        drop(held);
        assert!(!locals_capability_may_be_held(&other));
    }

    /// t1 wave 5, lane M: a live-phase `can_tag_objects` raises the per-VM
    /// gate and logs the in-flight refusal once (the epoch advances); a
    /// second acquisition logs nothing; another VM is untouched.
    #[test]
    fn t1m_live_heap_walker_refuses_in_flight_compiles_once() {
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let other = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        assert!(!shared.debug.debugger_gates.c_agent_walks_heap());
        let before = cratonvm_jit::jit_invalidation_epoch();
        assert!(note_live_heap_walker(&shared));
        assert!(shared.debug.debugger_gates.c_agent_walks_heap());
        assert!(cratonvm_jit::jit_invalidation_epoch() > before, "a record was logged");
        assert!(!note_live_heap_walker(&shared), "logged on the first gain only");
        assert!(!other.debug.debugger_gates.c_agent_walks_heap(), "per VM");
    }

    /// t1 wave 2, lane M: exactly the three capabilities under which HotSpot's
    /// C2 annotates safepoints for the escape barrier mark the VM as having a
    /// monitor observer; another OnLoad capability does not.
    #[test]
    fn t1m_onload_acquired_observes_monitors_names_the_three_capabilities() {
        assert!(!onload_acquired_observes_monitors(&[0; CAPABILITY_WORDS]));
        assert!(!onload_acquired_observes_monitors(&[
            CAN_GET_CURRENT_CONTENDED_MONITOR | CAN_POP_FRAME,
            0,
            0,
            0
        ]));
        assert!(onload_acquired_observes_monitors(&[CAN_GET_OWNED_MONITOR_INFO, 0, 0, 0]));
        assert!(onload_acquired_observes_monitors(&[CAN_ACCESS_LOCAL_VARIABLES, 0, 0, 0]));
        assert!(onload_acquired_observes_monitors(&[
            0,
            CAN_GET_OWNED_MONITOR_STACK_DEPTH_INFO,
            0,
            0
        ]));
        // The stack-depth bit read from the wrong word is not it.
        assert!(!onload_acquired_observes_monitors(&[
            CAN_GET_OWNED_MONITOR_STACK_DEPTH_INFO & !(CAN_GET_OWNED_MONITOR_INFO
                | CAN_ACCESS_LOCAL_VARIABLES),
            0,
            0,
            0
        ]));
    }

    /// Interpreter round i2 wave 57, lane L1: slot 72,
    /// `GetLocalVariableTable`, in HotSpot's order (MEASURED,
    /// `L1W57JvmtiLocalVariableTable`): the capability (99), a NULL method
    /// (23), a NULL out-parameter (100), then a native method (104) and a
    /// method without a table (101); and a table's rows.
    #[test]
    fn the_local_variable_table_answers_in_hotspots_order() {
        use cratonvm_reader::attribute::LocalVariableEntry;
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        jni::set_jni_context_arc(Arc::clone(&shared));
        let name = "cratonvm/test/W57Locals";
        let cid = loop_class(&shared, name);
        if let Some(class) = shared.classes.class_manager.write().get_class_mut(cid) {
            let mut with_table = class.methods[0].clone();
            with_table.name = Arc::from("withTable");
            if let Some(LazyAttribute::Decoded(Attribute::Code(code))) =
                with_table.attributes.first_mut()
            {
                code.attributes
                    .push(Attribute::LocalVariableTable(vec![LocalVariableEntry {
                        start_pc: 2,
                        length: 20,
                        name_index: 0,
                        descriptor_index: 0,
                        index: 1,
                    }]));
            }
            let mut native = class.methods[0].clone();
            native.name = Arc::from("nativeOne");
            native.access_flags = MethodAccessFlags::STATIC | MethodAccessFlags::NATIVE;
            native.attributes.clear();
            class.methods.push(with_table);
            class.methods.push(native);
        }
        let jmid = |index: u64| (u64::from(cid.as_u32()) << 32) | index;
        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        // SAFETY (every transmute below): the slot's `jvmti.h` signature.
        let table: extern "C" fn(*mut c_void, u64, *mut JInt, *mut *mut LocalVariableRow) -> JInt =
            unsafe { std::mem::transmute(function(env, 72)) };
        let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
            unsafe { std::mem::transmute(function(env, 142)) };
        let dealloc: extern "C" fn(*mut c_void, *mut u8) -> JInt =
            unsafe { std::mem::transmute(function(env, 47)) };
        let dispose: extern "C" fn(*mut c_void) -> JInt =
            unsafe { std::mem::transmute(function(env, 127)) };
        let mut count: JInt = -1;
        let mut rows: *mut LocalVariableRow = std::ptr::null_mut();
        assert_eq!(
            table(env, jmid(1), &mut count, &mut rows),
            ERR_MUST_POSSESS_CAPABILITY
        );
        assert_eq!(add(env, &[CAN_ACCESS_LOCAL_VARIABLES, 0, 0, 0]), ERR_NONE);
        assert_eq!(table(env, 0, &mut count, &mut rows), ERR_INVALID_METHODID);
        assert_eq!(
            table(env, jmid(1), std::ptr::null_mut(), &mut rows),
            ERR_NULL_POINTER
        );
        assert_eq!(
            table(env, jmid(2), &mut count, &mut rows),
            104,
            "a native method"
        );
        assert_eq!(
            table(env, jmid(0), &mut count, &mut rows),
            ERR_ABSENT_INFORMATION
        );
        assert_eq!(count, -1, "nothing written");
        assert_eq!(table(env, jmid(1), &mut count, &mut rows), ERR_NONE);
        assert_eq!(count, 1);
        // SAFETY: one row, just answered.
        let row = unsafe { *rows };
        assert_eq!((row.start_location, row.length, row.slot), (2, 20, 1));
        assert!(
            row.generic_signature.is_null(),
            "no LocalVariableTypeTable row"
        );
        for block in [row.name, row.signature, rows.cast::<u8>()] {
            assert_eq!(dealloc(env, block), ERR_NONE);
        }
        assert_eq!(dispose(env), ERR_NONE);
        rt::forget_vm_jvmti_state(shared.vm_identity);
        jni::clear_jni_context();
    }

    /// Wave 51: slot 10, `GetOwnedMonitorInfo`, is in the table behind
    /// `can_get_owned_monitor_info`, which is potential (as one of the
    /// OnLoad-only capabilities) and refused to an env without it.
    #[test]
    fn get_owned_monitor_info_needs_its_capability() {
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        jni::set_jni_context_arc(Arc::clone(&shared));
        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        assert_ne!(
            function(env, 10),
            not_available as *const () as usize,
            "slot 10 is implemented"
        );
        // SAFETY (every transmute below): the slot's `jvmti.h` signature.
        let potential: extern "C" fn(*mut c_void, *mut Caps) -> JInt =
            unsafe { std::mem::transmute(function(env, 140)) };
        let owned: extern "C" fn(*mut c_void, JObject, *mut JInt, *mut *mut JObject) -> JInt =
            unsafe { std::mem::transmute(function(env, 10)) };
        let dispose: extern "C" fn(*mut c_void) -> JInt =
            unsafe { std::mem::transmute(function(env, 127)) };
        let mut caps: Caps = [0; CAPABILITY_WORDS];
        assert_eq!(potential(env, &mut caps), ERR_NONE);
        assert_eq!(
            caps[0] & CAN_GET_OWNED_MONITOR_INFO,
            CAN_GET_OWNED_MONITOR_INFO
        );
        assert_eq!(
            ONLOAD_ONLY[0] & CAN_GET_OWNED_MONITOR_INFO,
            CAN_GET_OWNED_MONITOR_INFO
        );
        let mut count: JInt = -1;
        let mut monitors: *mut JObject = std::ptr::null_mut();
        assert_eq!(
            owned(env, 0, &mut count, &mut monitors),
            ERR_MUST_POSSESS_CAPABILITY
        );
        assert_eq!(count, -1, "nothing written");
        assert_eq!(dispose(env), ERR_NONE);
        rt::forget_vm_jvmti_state(shared.vm_identity);
        jni::clear_jni_context();
    }

    /// javac's `for (i = 0; i < 10; i++) s += i; return s;`: the
    /// `if_icmpge` at pc 9 runs eleven times.
    fn loop_code() -> Vec<u8> {
        vec![
            0x10, 10, 0x3b, // 0: bipush 10; istore_0
            0x03, 0x3c, // 3: iconst_0; istore_1
            0x03, 0x3d, // 5: iconst_0; istore_2
            0x1c, 0x1a, 0xa2, 0x00, 13, // 7: iload_2; iload_0; if_icmpge +13 (-> 22)
            0x1b, 0x1c, 0x60, 0x3c, // 12: iload_1; iload_2; iadd; istore_1
            0x84, 2, 1, // 16: iinc 2 1
            0xa7, 0xff, 0xf4, // 19: goto -12 (-> 7)
            0x1b, 0xac, // 22: iload_1; ireturn
        ]
    }

    /// A class `name` declaring the static loop method `jvmtiBpLoop()I`;
    /// its `jmethodID` is `class id << 32`.
    fn loop_class(shared: &SharedVm, name: &str) -> ClassId {
        let cid = shared
            .classes
            .class_manager
            .write()
            .try_ensure_synthetic_class(name, 0)
            .expect("Compatible mode fabricates");
        if let Some(class) = shared.classes.class_manager.write().get_class_mut(cid) {
            class.methods = vec![cratonvm_reader::method::ClassFileMethod {
                access_flags: MethodAccessFlags::STATIC,
                name: Arc::from("jvmtiBpLoop"),
                descriptor: Arc::from("()I"),
                attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
                    CodeAttribute {
                        max_stack: 2,
                        max_locals: 3,
                        code: cratonvm_reader::ByteView::from_vec(loop_code()),
                        exception_table: vec![],
                        attributes: vec![],
                    },
                )))],
            }];
        }
        cid
    }

    /// Run the loop method of `cid` on a thread with id `tid`.
    fn run_loop(shared: &Arc<SharedVm>, cid: ClassId, name: &str, tid: u64) {
        let mut thread = JvmThread::new(ThreadId(tid), "w13-native-agent");
        let frame = Frame::new(
            cid,
            name.to_string(),
            "jvmtiBpLoop".to_string(),
            "()I".to_string(),
            None,
            loop_code(),
            Vec::new(),
            2,
            3,
            &[],
        );
        let r = crate::runtime::interpreter::execute_prebuilt_frame(shared, &mut thread, frame);
        assert!(matches!(r, Ok(Some(Value::Int(45)))), "answer: {r:?}");
    }

    /// Members of `jvmtiEventCallbacks` as `jvmti.h` orders them.
    fn callbacks_with(members: &[(JInt, usize)]) -> [usize; CALLBACK_SLOTS] {
        let mut callbacks = [0usize; CALLBACK_SLOTS];
        for &(event, function) in members {
            callbacks[usize::try_from(event - MIN_EVENT).expect("an event")] = function;
        }
        callbacks
    }

    /// A native agent's life through the C table: `GetEnv` for JVMTI 1.2,
    /// capability negotiation (only the delivered events' capabilities are
    /// granted), `SetEventCallbacks` + `SetEventNotificationMode`,
    /// `SetBreakpoint` on the `if_icmpge` of a ten-trip loop — eleven C
    /// callbacks through `jvmti.h`'s `Breakpoint` member (event 62, member
    /// 12) — `GetMethodName` / `GetErrorName` / `Deallocate`,
    /// `ClearBreakpoint`, `DisposeEnvironment`; an unimplemented slot answers
    /// `JVMTI_ERROR_NOT_AVAILABLE`.
    #[test]
    fn a_native_agent_sets_a_breakpoint_and_receives_it_through_the_c_table() {
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        let vm = shared.vm_identity;
        jni::set_jni_context_arc(Arc::clone(&shared));
        HITS.store(0, Ordering::SeqCst);
        HITS_AT_9.store(0, Ordering::SeqCst);
        HITS_WITH_JNI_ENV.store(0, Ordering::SeqCst);

        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        assert!(!env.is_null());
        assert_eq!(
            get_env(0x3063_0000).0,
            jni::JNI_EVERSION,
            "JVMTI 99 is not provided"
        );
        // Round i2 wave 53, measured on HotSpot 25: 9 and 11 take minor 0
        // only, 12 is not provided.
        for refused in [0x3009_0100, 0x300B_0100, 0x300C_0000, 0x300A_0000] {
            assert_eq!(get_env(refused).0, jni::JNI_EVERSION, "{refused:#x}");
        }

        // SAFETY (every transmute below): the slot's `jvmti.h` signature.
        let version: extern "C" fn(*mut c_void, *mut JInt) -> JInt =
            unsafe { std::mem::transmute(function(env, 88)) };
        let mut v = 0;
        assert_eq!(version(env, &mut v), ERR_NONE);
        assert_eq!(v, JVMTI_VERSION_25);
        let potential: extern "C" fn(*mut c_void, *mut Caps) -> JInt =
            unsafe { std::mem::transmute(function(env, 140)) };
        let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
            unsafe { std::mem::transmute(function(env, 142)) };
        let get_caps: extern "C" fn(*mut c_void, *mut Caps) -> JInt =
            unsafe { std::mem::transmute(function(env, 89)) };
        let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
            unsafe { std::mem::transmute(function(env, 2)) };
        let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
            unsafe { std::mem::transmute(function(env, 122)) };
        let set_bp: extern "C" fn(*mut c_void, u64, i64) -> JInt =
            unsafe { std::mem::transmute(function(env, 38)) };
        let clear_bp: extern "C" fn(*mut c_void, u64, i64) -> JInt =
            unsafe { std::mem::transmute(function(env, 39)) };
        let method_name: extern "C" fn(
            *mut c_void,
            u64,
            *mut *mut u8,
            *mut *mut u8,
            *mut *mut u8,
        ) -> JInt = unsafe { std::mem::transmute(function(env, 64)) };
        let dealloc: extern "C" fn(*mut c_void, *mut u8) -> JInt =
            unsafe { std::mem::transmute(function(env, 47)) };
        let error_name: extern "C" fn(*mut c_void, JInt, *mut *mut u8) -> JInt =
            unsafe { std::mem::transmute(function(env, 128)) };
        let dispose: extern "C" fn(*mut c_void) -> JInt =
            unsafe { std::mem::transmute(function(env, 127)) };
        let get_tag: extern "C" fn(*mut c_void, JObject, *mut i64) -> JInt =
            unsafe { std::mem::transmute(function(env, 106)) };
        let mut tag = 0i64;
        assert_eq!(
            get_tag(env, 0, &mut tag),
            ERR_MUST_POSSESS_CAPABILITY,
            "GetTag (wave 63) wants can_tag_objects first"
        );

        let mut caps: Caps = [0; CAPABILITY_WORDS];
        assert_eq!(potential(env, &mut caps), ERR_NONE);
        // Less the switches' class-hook capabilities (waves 54 and 55) and
        // `can_redefine_classes` (wave 57).
        caps[0] &= !(CAN_GENERATE_ALL_CLASS_HOOK_EVENTS | CAN_REDEFINE_CLASSES);
        caps[1] &= !CAN_RETRANSFORM_CLASSES;
        let mut expected = POTENTIAL;
        expected[0] &= !(CAN_GENERATE_ALL_CLASS_HOOK_EVENTS | CAN_REDEFINE_CLASSES);
        expected[1] &= !CAN_RETRANSFORM_CLASSES;
        assert_eq!(caps, expected);
        let name = "cratonvm/test/W12NativeAgent";
        let cid = loop_class(&shared, name);
        // Widening: the class id's half of the jmethodID.
        let jmid = u64::from(cid.as_u32()) << 32;

        // Nothing is granted yet.
        assert_eq!(set_bp(env, jmid, 9), ERR_MUST_POSSESS_CAPABILITY);
        assert_eq!(
            set_mode(env, 1, EVENT_BREAKPOINT, 0),
            ERR_MUST_POSSESS_CAPABILITY
        );
        let tag_objects: Caps = [1, 0, 0, 0];
        assert_eq!(add(env, &tag_objects), ERR_NONE, "delivered since wave 63");
        assert_eq!(get_tag(env, 0, &mut tag), ERR_INVALID_OBJECT);
        assert_eq!(add(env, &POTENTIAL), ERR_NONE);
        assert_eq!(get_caps(env, &mut caps), ERR_NONE);
        assert_eq!(
            caps[0] & CAN_GENERATE_BREAKPOINT_EVENTS,
            CAN_GENERATE_BREAKPOINT_EVENTS
        );

        // The agent's structure, as long as `jvmti.h` makes it up to
        // `Breakpoint` (member 12).
        let callbacks = callbacks_with(&[(EVENT_BREAKPOINT, on_breakpoint as *const () as usize)]);
        let size = JInt::try_from(13 * std::mem::size_of::<usize>()).expect("small");
        assert_eq!(
            set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
            ERR_NONE
        );
        assert_eq!(set_mode(env, 1, 73, 0), ERR_NONE, "MonitorWait (wave 63)");
        assert_eq!(set_mode(env, 0, 73, 0), ERR_NONE);
        assert_eq!(set_mode(env, 1, 200, 0), ERR_INVALID_EVENT_TYPE);
        assert_eq!(
            set_mode(env, 1, 89, 0),
            ERR_INVALID_EVENT_TYPE,
            "past JDK 25's last"
        );
        assert_eq!(set_mode(env, 1, EVENT_BREAKPOINT, 0), ERR_NONE);

        assert_eq!(set_bp(env, jmid, 9), ERR_NONE);
        assert_eq!(set_bp(env, jmid, 9), rt::JvmtiError::Duplicate as JInt);
        assert_eq!(
            set_bp(env, jmid, 1000),
            rt::JvmtiError::InvalidLocation as JInt
        );
        assert_eq!(set_bp(env, jmid | 7, 0), ERR_INVALID_METHODID);

        let (mut mname, mut sig, mut generic) = (
            std::ptr::null_mut::<u8>(),
            std::ptr::null_mut::<u8>(),
            std::ptr::NonNull::<u8>::dangling().as_ptr(),
        );
        assert_eq!(
            method_name(env, jmid, &mut mname, &mut sig, &mut generic),
            ERR_NONE
        );
        // SAFETY: NUL-terminated strings this table allocated.
        let (n, s) = unsafe {
            (
                std::ffi::CStr::from_ptr(mname.cast()).to_owned(),
                std::ffi::CStr::from_ptr(sig.cast()).to_owned(),
            )
        };
        assert_eq!((n.to_str(), s.to_str()), (Ok("jvmtiBpLoop"), Ok("()I")));
        assert!(generic.is_null());
        assert_eq!(dealloc(env, mname), ERR_NONE);
        assert_eq!(dealloc(env, sig), ERR_NONE);
        let mut err = std::ptr::null_mut::<u8>();
        assert_eq!(error_name(env, 40, &mut err), ERR_NONE);
        // SAFETY: a NUL-terminated string this table allocated.
        let text = unsafe { std::ffi::CStr::from_ptr(err.cast()).to_owned() };
        assert_eq!(text.to_str(), Ok("JVMTI_ERROR_DUPLICATE"));
        assert_eq!(dealloc(env, err), ERR_NONE);
        assert_eq!(error_name(env, 57, &mut err), ERR_ILLEGAL_ARGUMENT);

        // The loop: eleven executions of pc 9, one C callback each.
        run_loop(&shared, cid, name, 0);
        assert_eq!(
            HITS_AT_9.load(Ordering::SeqCst),
            11,
            "one callback per execution"
        );
        assert_eq!(HITS.load(Ordering::SeqCst), 11);
        assert_eq!(
            HITS_WITH_JNI_ENV.load(Ordering::SeqCst),
            11,
            "a JNIEnv* each"
        );

        assert_eq!(clear_bp(env, jmid, 9), ERR_NONE);
        assert_eq!(clear_bp(env, jmid, 9), rt::JvmtiError::NotFound as JInt);
        assert_eq!(set_bp(env, jmid, 3), ERR_NONE);
        assert_eq!(dispose(env), ERR_NONE);
        assert_eq!(set_bp(env, jmid, 9), ERR_INVALID_ENVIRONMENT, "disposed");
        assert!(
            !shared
                .debug
                .debugger_gates
                .concerns_method(cid.as_u32(), "jvmtiBpLoop", "()I"),
            "disposing cleared the env's breakpoints"
        );
        assert!(
            shared.debug.jvmti_env.lock().c_table.native_envs.is_empty(),
            "disposing left the VM's list"
        );
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }

    static VM_INITS: AtomicUsize = AtomicUsize::new(0);
    static PREPARED: AtomicU64 = AtomicU64::new(0);
    static EXCEPTIONS: AtomicUsize = AtomicUsize::new(0);
    static EXCEPTION_SEEN: std::sync::Mutex<Vec<(u64, i64, bool, u64, i64)>> =
        std::sync::Mutex::new(Vec::new());
    static STARTUP_BP_HITS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn on_vm_init(_env: *mut JvmtiNativeEnv, jni_env: JNIEnv, _thread: JObject) {
        if !jni_env.is_null() {
            VM_INITS.fetch_add(1, Ordering::SeqCst);
        }
    }

    static VM_DEATHS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn on_vm_death(_env: *mut JvmtiNativeEnv, jni_env: JNIEnv) {
        if !jni_env.is_null() {
            VM_DEATHS.fetch_add(1, Ordering::SeqCst);
        }
    }

    unsafe extern "C" fn on_class_prepare(
        _env: *mut JvmtiNativeEnv,
        _jni_env: JNIEnv,
        _thread: JObject,
        klass: JObject,
    ) {
        PREPARED.store(klass, Ordering::SeqCst);
    }

    #[allow(clippy::too_many_arguments)]
    unsafe extern "C" fn on_exception(
        _env: *mut JvmtiNativeEnv,
        _jni_env: JNIEnv,
        _thread: JObject,
        method: u64,
        location: i64,
        exception: JObject,
        catch_method: u64,
        catch_location: i64,
    ) {
        EXCEPTIONS.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut seen) = EXCEPTION_SEEN.lock() {
            seen.push((
                method,
                location,
                exception != 0,
                catch_method,
                catch_location,
            ));
        }
    }

    unsafe extern "C" fn on_startup_breakpoint(
        _env: *mut JvmtiNativeEnv,
        _jni_env: JNIEnv,
        _thread: JObject,
        _method: u64,
        _location: i64,
    ) {
        STARTUP_BP_HITS.fetch_add(1, Ordering::SeqCst);
    }

    /// A startup agent (wave 13): the env it obtains in `Agent_OnLoad` stays
    /// unbound — `SetBreakpoint` is `WRONG_PHASE` although the thread can
    /// reach a VM — while it negotiates capabilities and enables `VMInit`,
    /// `ClassPrepare`, `Exception` and `Breakpoint`. Bound by `Vm::new`
    /// (`bind_startup_envs`), it is in the start phase (wave 14): no
    /// `VMInit` yet, `Exception` not armed on the VM, `SetBreakpoint` and
    /// `SetEventNotificationMode` `WRONG_PHASE`, while `ClassPrepare`
    /// arrives with the class's `jclass`, whose `GetClassSignature` and
    /// `GetClassMethods` answer. Entering the live phase
    /// (`enter_live_phase`, `set_init_level(3)`) sends exactly one `VMInit`
    /// and arms the events; a breakpoint set on the method id
    /// `GetClassMethods` answered fires; `Exception` arrives with the
    /// exception as a reference and no known catch as method NULL /
    /// location 0.
    #[test]
    fn a_startup_agent_gets_vm_init_class_prepare_and_exception_through_the_c_table() {
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        let vm = shared.vm_identity;
        jni::set_jni_context_arc(Arc::clone(&shared));
        VM_INITS.store(0, Ordering::SeqCst);
        VM_DEATHS.store(0, Ordering::SeqCst);
        PREPARED.store(0, Ordering::SeqCst);
        EXCEPTIONS.store(0, Ordering::SeqCst);
        STARTUP_BP_HITS.store(0, Ordering::SeqCst);
        if let Ok(mut seen) = EXCEPTION_SEEN.lock() {
            seen.clear();
        }

        // `Agent_OnLoad`.
        let mut slot: Option<*mut c_void> = None;
        let envs = collect_startup_envs(|| {
            let (code, env) = get_env(0x3001_0200);
            assert_eq!(code, jni::JNI_OK);
            // SAFETY (every transmute below): the slot's `jvmti.h` signature.
            let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
                unsafe { std::mem::transmute(function(env, 142)) };
            let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
                unsafe { std::mem::transmute(function(env, 2)) };
            let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
                unsafe { std::mem::transmute(function(env, 122)) };
            let set_bp: extern "C" fn(*mut c_void, u64, i64) -> JInt =
                unsafe { std::mem::transmute(function(env, 38)) };
            assert_eq!(add(env, &POTENTIAL), ERR_NONE);
            let callbacks = callbacks_with(&[
                (EVENT_VM_INIT, on_vm_init as *const () as usize),
                (EVENT_VM_DEATH, on_vm_death as *const () as usize),
                (EVENT_CLASS_PREPARE, on_class_prepare as *const () as usize),
                (EVENT_EXCEPTION, on_exception as *const () as usize),
                (
                    EVENT_BREAKPOINT,
                    on_startup_breakpoint as *const () as usize,
                ),
            ]);
            let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
            assert_eq!(
                set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
                ERR_NONE
            );
            for event in [
                EVENT_VM_INIT,
                EVENT_VM_DEATH,
                EVENT_CLASS_PREPARE,
                EVENT_EXCEPTION,
                EVENT_BREAKPOINT,
            ] {
                assert_eq!(
                    set_mode(env, 1, event, 0),
                    ERR_NONE,
                    "event {event} at OnLoad"
                );
            }
            assert_eq!(set_bp(env, 1 << 32, 0), ERR_WRONG_PHASE, "no VM yet");
            slot = Some(env);
        });
        let env = slot.expect("the agent obtained an env");
        assert_eq!(envs, vec![env as usize]);
        assert!(
            shared.debug.jvmti_env.lock().c_table.native_envs.is_empty(),
            "not bound to the VM the thread reaches"
        );
        assert!(!shared.debug.debugger_gates.native_class_prepare_armed());

        // `Vm::new`: bound, in the start phase (past `System.initPhase1`, so
        // the start phase with `DebuggerGates::c_jvmti_phases` too, wave 55;
        // its primordial phase is
        // `a_startup_agent_gets_vm_start_after_the_primordial_phase`).
        shared.raise_vm_init_level(2);
        shared.debug.jvmti_env.lock().c_table.startup_native_envs = envs;
        bind_startup_envs(&shared);
        assert_eq!(VM_INITS.load(Ordering::SeqCst), 0, "no VMInit before live");
        assert!(shared.debug.debugger_gates.jvmti_start_phase());
        assert_eq!(
            shared.debug.jvmti_env.lock().c_table.native_envs,
            vec![env as usize]
        );
        assert!(shared.debug.debugger_gates.native_class_prepare_armed());
        assert!(
            !rt::exception_event_enabled_for_vm(vm, 3),
            "Exception is not posted in the start phase"
        );
        // SAFETY (both transmutes): the slot's `jvmti.h` signature.
        let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
            unsafe { std::mem::transmute(function(env, 2)) };
        let set_bp: extern "C" fn(*mut c_void, u64, i64) -> JInt =
            unsafe { std::mem::transmute(function(env, 38)) };
        assert_eq!(
            set_mode(env, 0, EVENT_EXCEPTION, 0),
            ERR_WRONG_PHASE,
            "OnLoad or live only"
        );

        // `ClassPrepare` (a start-phase event), then the class functions
        // (start or live) on its `jclass`.
        let name = "cratonvm/test/W13StartupAgent";
        let cid = loop_class(&shared, name);
        // Prepared, as the VM marks a class before it reports it (wave 52:
        // `GetClassMethods` refuses one that is not).
        if let Some(class) = shared.classes.class_manager.write().get_class_mut(cid) {
            class.state = crate::classloading::ClassState::Prepared;
        }
        crate::debug::class_prepared_on_thread(&shared, 0, cid);
        let klass = PREPARED.load(Ordering::SeqCst);
        assert_eq!(klass, jni::class_id_to_jclass(cid));
        // Once per class (wave 15): a class prepared again (an
        // initialization retried after a supertype's link failure) is not
        // reported again.
        PREPARED.store(0, Ordering::SeqCst);
        crate::debug::class_prepared_on_thread(&shared, 0, cid);
        assert_eq!(PREPARED.load(Ordering::SeqCst), 0, "reported once");
        // SAFETY (every transmute below): the slot's `jvmti.h` signature.
        let class_sig: extern "C" fn(*mut c_void, JObject, *mut *mut u8, *mut *mut u8) -> JInt =
            unsafe { std::mem::transmute(function(env, 48)) };
        let class_methods: extern "C" fn(*mut c_void, JObject, *mut JInt, *mut *mut u64) -> JInt =
            unsafe { std::mem::transmute(function(env, 52)) };
        let dealloc: extern "C" fn(*mut c_void, *mut u8) -> JInt =
            unsafe { std::mem::transmute(function(env, 47)) };
        let dispose: extern "C" fn(*mut c_void) -> JInt =
            unsafe { std::mem::transmute(function(env, 127)) };
        let mut sig = std::ptr::null_mut::<u8>();
        assert_eq!(
            class_sig(env, klass, &mut sig, std::ptr::null_mut()),
            ERR_NONE
        );
        // SAFETY: a NUL-terminated string this table allocated.
        let text = unsafe { std::ffi::CStr::from_ptr(sig.cast()).to_owned() };
        assert_eq!(text.to_str(), Ok("Lcratonvm/test/W13StartupAgent;"));
        assert_eq!(dealloc(env, sig), ERR_NONE);
        assert_eq!(
            class_sig(env, 0, &mut sig, std::ptr::null_mut()),
            ERR_INVALID_CLASS
        );
        let (mut count, mut methods) = (0, std::ptr::null_mut::<u64>());
        assert_eq!(
            class_methods(env, klass, &mut count, &mut methods),
            ERR_NONE
        );
        assert_eq!(count, 1);
        // SAFETY: a block of `count` ids this table allocated.
        let jmid = unsafe { *methods };
        assert_eq!(jmid, u64::from(cid.as_u32()) << 32);
        assert_eq!(dealloc(env, methods.cast::<u8>()), ERR_NONE);
        assert_eq!(set_bp(env, jmid, 9), ERR_WRONG_PHASE, "live phase only");

        // `set_init_level(3)`: the live phase, one `VMInit`, however often
        // the level is raised.
        enter_live_phase(&shared);
        enter_live_phase(&shared);
        assert_eq!(
            VM_INITS.load(Ordering::SeqCst),
            1,
            "one VMInit with a JNIEnv*"
        );
        assert!(!shared.debug.debugger_gates.jvmti_start_phase());
        assert!(
            rt::exception_event_enabled_for_vm(vm, 3),
            "Exception armed on the VM's manager for the unwinder"
        );
        assert_eq!(set_bp(env, jmid, 9), ERR_NONE);
        run_loop(&shared, cid, name, 0);
        assert_eq!(STARTUP_BP_HITS.load(Ordering::SeqCst), 11);

        // `Exception`, as the unwinder posts it (uncaught: 0 / -1).
        let thrown = shared.mem.heap.alloc_object(cid, 0);
        rt::fire_exception_for_vm(
            vm,
            0,
            jmid,
            12,
            // Cast: the object's address, as the unwinder passes it
            thrown.as_ptr() as usize as u64,
            0,
            -1,
        );
        assert_eq!(EXCEPTIONS.load(Ordering::SeqCst), 1);
        assert_eq!(
            EXCEPTION_SEEN
                .lock()
                .map(|seen| seen.clone())
                .unwrap_or_default(),
            vec![(jmid, 12, true, 0, 0)]
        );

        // `Drop for Vm`: one `VMDeath`, the agents unloaded once.
        shut_down_agents(&shared, 0);
        assert_eq!(VM_DEATHS.load(Ordering::SeqCst), 1);
        assert!(shared
            .debug
            .jvmti_env
            .lock()
            .c_table
            .agent_registry
            .agents()
            .is_empty());

        // Disposed: no more deliveries, the gate is down.
        assert_eq!(dispose(env), ERR_NONE);
        assert!(!shared.debug.debugger_gates.native_class_prepare_armed());
        assert!(
            !rt::exception_event_enabled_for_vm(vm, 0),
            "disposing took Exception off the VM (wave 14)"
        );
        crate::debug::class_prepared_on_thread(&shared, 0, cid);
        // Cast: the object's address, as the unwinder passes it
        let address = thrown.as_ptr() as usize as u64;
        rt::fire_exception_for_vm(vm, 0, jmid, 12, address, 0, -1);
        assert_eq!(EXCEPTIONS.load(Ordering::SeqCst), 1);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }

    /// The counter a phase test's env keeps in its environment-local
    /// storage (wave 55): no test static.
    fn phase_counter<'a>(env: *mut JvmtiNativeEnv) -> &'a std::sync::atomic::AtomicU64 {
        // SAFETY: an env this table handed out, whose local storage the
        // test set to a counter that outlives every delivery.
        let storage = unsafe { env_at(env as usize) }.state.lock().local_storage;
        // SAFETY: as above.
        unsafe { &*(storage as *const std::sync::atomic::AtomicU64) }
    }

    /// `VMStart`: one count, plus the phase it sees times 256.
    unsafe extern "C" fn on_counted_vm_start(env: *mut JvmtiNativeEnv, _jni_env: JNIEnv) {
        let mut phase: JInt = 0;
        assert_eq!(get_phase(env, &mut phase), ERR_NONE);
        let phase = u64::try_from(phase).unwrap_or(0);
        phase_counter(env).fetch_add(1 + (phase << 8), Ordering::SeqCst);
    }

    /// `ClassPrepare`: `1 << 32`.
    unsafe extern "C" fn on_counted_class_prepare(
        env: *mut JvmtiNativeEnv,
        _jni_env: JNIEnv,
        _thread: JObject,
        _class: JObject,
    ) {
        phase_counter(env).fetch_add(1 << 32, Ordering::SeqCst);
    }

    /// Interpreter round i2 wave 55, lane L1, with
    /// `DebuggerGates::c_jvmti_phases` (the test asserts nothing without
    /// it): a startup agent's env bound by `Vm::new` is in the primordial
    /// phase (`GetPhase` 2) and gets no `ClassPrepare`; init level 2 (after
    /// `System.initPhase1`) enters the start phase and sends one `VMStart`,
    /// which sees phase 6, however often the level is raised; a class
    /// prepared then is reported; level 3 is the live phase; after the
    /// `VMDeath` callbacks, the dead phase.
    #[test]
    fn a_startup_agent_gets_vm_start_after_the_primordial_phase() {
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        if !shared.debug.debugger_gates.c_jvmti_phases() {
            return;
        }
        let vm = shared.vm_identity;
        jni::set_jni_context_arc(Arc::clone(&shared));
        let counter = std::sync::atomic::AtomicU64::new(0);
        let mut slot: Option<*mut c_void> = None;
        let envs = collect_startup_envs(|| {
            let (code, env) = get_env(0x3001_0200);
            assert_eq!(code, jni::JNI_OK);
            // SAFETY (every transmute below): the slot's `jvmti.h` signature.
            let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
                unsafe { std::mem::transmute(function(env, 2)) };
            let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
                unsafe { std::mem::transmute(function(env, 122)) };
            let set_storage: extern "C" fn(*mut c_void, usize) -> JInt =
                unsafe { std::mem::transmute(function(env, 148)) };
            let callbacks = callbacks_with(&[
                (EVENT_VM_START, on_counted_vm_start as *const () as usize),
                (
                    EVENT_CLASS_PREPARE,
                    on_counted_class_prepare as *const () as usize,
                ),
            ]);
            let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
            assert_eq!(
                set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
                ERR_NONE
            );
            // Cast: the counter's address, the env's local storage
            let address = &counter as *const std::sync::atomic::AtomicU64 as usize;
            assert_eq!(set_storage(env, address), ERR_NONE);
            assert_eq!(
                set_mode(env, 1, EVENT_VM_START, 0),
                ERR_NONE,
                "VMStart at OnLoad"
            );
            assert_eq!(set_mode(env, 1, EVENT_CLASS_PREPARE, 0), ERR_NONE);
            slot = Some(env);
        });
        let env = slot.expect("the agent obtained an env");
        let phase_of = |env: *mut c_void| {
            let mut phase: JInt = 0;
            assert_eq!(get_phase(env.cast(), &mut phase), ERR_NONE);
            phase
        };

        // `Vm::new`: bound, primordial; a class prepared now is not reported.
        shared.debug.jvmti_env.lock().c_table.startup_native_envs = envs;
        bind_startup_envs(&shared);
        assert_eq!(phase_of(env), 2, "primordial");
        assert!(!shared.debug.debugger_gates.native_class_prepare_armed());
        let early = loop_class(&shared, "cratonvm/test/W55Primordial");
        crate::debug::class_prepared_on_thread(&shared, 0, early);
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "nothing in the primordial phase"
        );
        // SAFETY: the slot's `jvmti.h` signature.
        let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
            unsafe { std::mem::transmute(function(env, 2)) };
        assert_eq!(
            set_mode(env, 0, EVENT_CLASS_PREPARE, 0),
            ERR_WRONG_PHASE,
            "OnLoad or live only"
        );

        // Level 2: the start phase, one `VMStart` seeing it.
        shared.raise_vm_init_level(2);
        shared.raise_vm_init_level(2);
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1 + (6 << 8),
            "one VMStart, in phase 6"
        );
        assert_eq!(phase_of(env), 6, "start");
        assert!(shared.debug.debugger_gates.native_class_prepare_armed());
        let later = loop_class(&shared, "cratonvm/test/W55Start");
        crate::debug::class_prepared_on_thread(&shared, 0, later);
        assert_eq!(
            counter.load(Ordering::SeqCst) >> 32,
            1,
            "ClassPrepare in the start phase"
        );

        // Level 3: live.
        shared.raise_vm_init_level(3);
        assert_eq!(phase_of(env), 4, "live");
        assert_eq!(set_mode(env, 1, EVENT_VM_START, 0), ERR_NONE);

        // `Drop for Vm`: dead after `VMDeath`.
        shut_down_agents(&shared, 0);
        assert_eq!(phase_of(env), 8, "dead");
        assert_eq!(
            counter.load(Ordering::SeqCst) & 0xFF,
            1,
            "one VMStart in all"
        );
        // SAFETY: the slot's `jvmti.h` signature.
        let dispose: extern "C" fn(*mut c_void) -> JInt =
            unsafe { std::mem::transmute(function(env, 127)) };
        assert_eq!(dispose(env), ERR_NONE);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }

    /// Interpreter round i2 wave 55, lane L1, with
    /// `DebuggerGates::c_lambda_classes` (the test asserts nothing without
    /// it): a lambda proxy class the VM minted outside the class store is
    /// listed by `GetLoadedClasses` and answers HotSpot's facts of a lambda's
    /// hidden class (measured, `L1W55JvmtiLambdaClass`): its signature after
    /// its host, `0x1030`, status 7, its functional interface, neither an
    /// interface nor an array, version 69, no source file.
    #[test]
    fn a_lambda_proxy_class_answers_the_class_functions() {
        use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        if !shared.debug.debugger_gates.c_lambda_classes() {
            return;
        }
        let vm = shared.vm_identity;
        jni::set_jni_context_arc(Arc::clone(&shared));
        let host = loop_class(&shared, "cratonvm/test/W55Host");
        let iface = loop_class(&shared, "cratonvm/test/W55Iface");
        let proxy = shared.alloc_lambda_proxy_id();
        shared.classes.lambda_proxies.write().insert(
            proxy,
            Arc::new(LambdaCallSite {
                functional_interface_id: Some(iface),
                functional_interface: "cratonvm/test/W55Iface".into(),
                sam_method_name: "run".into(),
                sam_descriptor: "()V".into(),
                impl_handle: MethodHandle {
                    kind: MethodHandleKind::InvokeStatic,
                    class_name: "cratonvm/test/W55Host".into(),
                    member_name: "lambda$main$0".into(),
                    descriptor: "()V".into(),
                },
                instantiated_descriptor: "()V".into(),
                capture_types: Vec::new(),
                proxy_class_id: proxy,
                serializable_flag: false,
            }),
        );
        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        let env = env.cast::<JvmtiNativeEnv>();
        let klass = jni::class_id_to_jclass(proxy);

        let (mut count, mut classes) = (0, std::ptr::null_mut::<JObject>());
        assert_eq!(get_loaded_classes(env, &mut count, &mut classes), ERR_NONE);
        // SAFETY: a block of `count` handles this table allocated.
        let listed =
            unsafe { std::slice::from_raw_parts(classes, usize::try_from(count).unwrap_or(0)) }
                .contains(&klass);
        assert!(listed, "GetLoadedClasses lists the proxy");
        // SAFETY: a block this table allocated.
        unsafe { free_block(classes.cast()) };

        let (mut sig, mut generic) = (std::ptr::null_mut::<u8>(), std::ptr::null_mut::<u8>());
        assert_eq!(
            get_class_signature(env, klass, &mut sig, &mut generic),
            ERR_NONE
        );
        // SAFETY: a NUL-terminated string this table allocated.
        let text = unsafe { std::ffi::CStr::from_ptr(sig.cast()).to_owned() };
        let expected = format!("Lcratonvm/test/W55Host$$Lambda.0x{:016x};", proxy.as_u32());
        assert_eq!(text.to_str(), Ok(expected.as_str()));
        assert!(generic.is_null(), "no generic signature");
        // SAFETY: a block this table allocated.
        unsafe { free_block(sig) };

        let mut value: JInt = 0;
        assert_eq!(get_class_modifiers(env, klass, &mut value), ERR_NONE);
        assert_eq!(value, 0x1030);
        assert_eq!(get_class_status(env, klass, &mut value), ERR_NONE);
        assert_eq!(value, 7);
        let mut flag: u8 = 1;
        assert_eq!(is_interface(env, klass, &mut flag), ERR_NONE);
        assert_eq!(flag, 0);
        flag = 1;
        assert_eq!(is_array_class(env, klass, &mut flag), ERR_NONE);
        assert_eq!(flag, 0);
        let (mut minor, mut major) = (-1, -1);
        assert_eq!(
            get_class_version_numbers(env, klass, &mut minor, &mut major),
            ERR_NONE
        );
        assert_eq!((minor, major), (0, 69));
        let mut interfaces = std::ptr::null_mut::<JObject>();
        assert_eq!(
            get_implemented_interfaces(env, klass, &mut count, &mut interfaces),
            ERR_NONE
        );
        assert_eq!(count, 1);
        // SAFETY: a block of one handle this table allocated.
        assert_eq!(unsafe { *interfaces }, jni::class_id_to_jclass(iface));
        // SAFETY: a block this table allocated.
        unsafe { free_block(interfaces.cast()) };
        let _ = host;
        assert_eq!(dispose_environment(env), ERR_NONE);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }

    /// Interpreter round i2 wave 56, lane L1, with
    /// `DebuggerGates::c_lambda_methods` (the test asserts nothing without
    /// it): `GetClassMethods` of a capturing lambda proxy lists its
    /// constructor and its functional method, and the method functions answer
    /// HotSpot's facts of each (measured, `L1W56JvmtiLambdaMethods`).
    #[test]
    fn a_lambda_proxy_class_answers_its_methods() {
        use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        if !shared.debug.debugger_gates.c_lambda_methods() {
            return;
        }
        let vm = shared.vm_identity;
        jni::set_jni_context_arc(Arc::clone(&shared));
        let iface = loop_class(&shared, "cratonvm/test/W56Function");
        let _host = loop_class(&shared, "cratonvm/test/W56Host");
        let proxy = shared.alloc_lambda_proxy_id();
        shared.classes.lambda_proxies.write().insert(
            proxy,
            Arc::new(LambdaCallSite {
                functional_interface_id: Some(iface),
                functional_interface: "cratonvm/test/W56Function".into(),
                sam_method_name: "apply".into(),
                sam_descriptor: "(Ljava/lang/Object;)Ljava/lang/Object;".into(),
                impl_handle: MethodHandle {
                    kind: MethodHandleKind::InvokeStatic,
                    class_name: "cratonvm/test/W56Host".into(),
                    member_name: "lambda$main$1".into(),
                    descriptor: "(Ljava/lang/String;JLjava/lang/Integer;)Ljava/lang/String;".into(),
                },
                instantiated_descriptor: "(Ljava/lang/Integer;)Ljava/lang/String;".into(),
                capture_types: vec!['L', 'J'],
                proxy_class_id: proxy,
                serializable_flag: false,
            }),
        );
        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        let env = env.cast::<JvmtiNativeEnv>();
        let klass = jni::class_id_to_jclass(proxy);

        let (mut count, mut methods) = (0, std::ptr::null_mut::<u64>());
        assert_eq!(
            get_class_methods(env, klass, &mut count, &mut methods),
            ERR_NONE
        );
        assert_eq!(count, 2, "the constructor and the functional method");
        // SAFETY: a block of `count` ids this table allocated.
        let ids = unsafe { std::slice::from_raw_parts(methods, 2) }.to_vec();
        // SAFETY: a block this table allocated.
        unsafe { free_block(methods.cast()) };
        let expected = [
            ("<init>", "(Ljava/lang/String;J)V", 0x2, 4),
            ("apply", "(Ljava/lang/Object;)Ljava/lang/Object;", 0x1, 2),
        ];
        for (&id, (name, descriptor, modifiers, slots)) in ids.iter().zip(expected) {
            let (mut n, mut s, mut g) = (
                std::ptr::null_mut::<u8>(),
                std::ptr::null_mut::<u8>(),
                std::ptr::null_mut::<u8>(),
            );
            assert_eq!(get_method_name(env, id, &mut n, &mut s, &mut g), ERR_NONE);
            // SAFETY: NUL-terminated strings this table allocated.
            let (n_text, s_text) = unsafe {
                (
                    std::ffi::CStr::from_ptr(n.cast()).to_owned(),
                    std::ffi::CStr::from_ptr(s.cast()).to_owned(),
                )
            };
            assert_eq!(n_text.to_str(), Ok(name));
            assert_eq!(s_text.to_str(), Ok(descriptor));
            assert!(g.is_null(), "no generic signature");
            // SAFETY: blocks this table allocated.
            unsafe {
                free_block(n);
                free_block(s);
            }
            let mut declaring: JObject = 0;
            assert_eq!(
                get_method_declaring_class(env, id, &mut declaring),
                ERR_NONE
            );
            assert_eq!(declaring, klass);
            let mut value: JInt = -1;
            assert_eq!(get_method_modifiers(env, id, &mut value), ERR_NONE);
            assert_eq!(value, modifiers, "{name}");
            assert_eq!(get_arguments_size(env, id, &mut value), ERR_NONE);
            assert_eq!(value, slots, "{name}");
            assert_eq!(get_max_locals(env, id, &mut value), ERR_NONE);
            assert_eq!(value, slots, "{name}");
            let mut flag: u8 = 1;
            assert_eq!(is_method_native(env, id, &mut flag), ERR_NONE);
            assert_eq!(flag, 0);
            // Wave 57 (`CRATONVM_JVMTI_C_LAMBDA_CODE`, on): the spun code's
            // range (it was `JVMTI_ERROR_ABSENT_INFORMATION`).
            let (mut start, mut end) = (-1i64, -1i64);
            assert_eq!(
                get_method_location(env, id, &mut start, &mut end),
                ERR_NONE,
                "{name}"
            );
            assert_eq!(start, 0, "{name}");
            assert!(end > 0, "{name}: the code ends at {end}");
        }
        let past = (u64::from(proxy.as_u32()) << 32) | 2;
        let mut value: JInt = -1;
        assert_eq!(
            get_method_modifiers(env, past, &mut value),
            ERR_INVALID_METHODID,
            "no third method"
        );
        assert_eq!(dispose_environment(env), ERR_NONE);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }

    /// Interpreter round i2 wave 56, lane L1: the CPU-time functions answer
    /// HotSpot's (measured, `L1W56JvmtiThreadTimes`): 99 without the
    /// capability, then the total-CPU timer and the calling thread's clock;
    /// `IsMethodObsolete` refuses an id that names no method.
    #[test]
    fn the_cpu_time_functions_answer_with_their_capabilities() {
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        let vm = shared.vm_identity;
        jni::set_jni_context_arc(Arc::clone(&shared));
        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        let env = env.cast::<JvmtiNativeEnv>();
        let mut nanos: i64 = -1;
        assert_eq!(
            get_current_thread_cpu_time(env, &mut nanos),
            ERR_MUST_POSSESS_CAPABILITY
        );
        assert_eq!(
            get_thread_cpu_time(env, 0, &mut nanos),
            ERR_MUST_POSSESS_CAPABILITY
        );
        let wanted: [u32; CAPABILITY_WORDS] = [
            CAN_GET_CURRENT_THREAD_CPU_TIME | CAN_GET_THREAD_CPU_TIME,
            0,
            0,
            0,
        ];
        assert_eq!(add_capabilities(env, &wanted), ERR_NONE);
        let mut info = TimerInfoRow {
            max_value: 0,
            may_skip_forward: 1,
            may_skip_backward: 1,
            kind: 0,
            reserved1: 0,
            reserved2: 0,
        };
        assert_eq!(get_thread_cpu_timer_info(env, &mut info), ERR_NONE);
        assert_eq!(
            (
                info.max_value,
                info.may_skip_forward,
                info.may_skip_backward,
                info.kind
            ),
            (-1, 0, 0, TIMER_TOTAL_CPU)
        );
        assert_eq!(
            get_current_thread_cpu_timer_info(env, std::ptr::null_mut()),
            ERR_NULL_POINTER
        );
        if current_thread_cpu_nanos().is_some() {
            assert_eq!(get_current_thread_cpu_time(env, &mut nanos), ERR_NONE);
            assert!(nanos >= 0);
            assert_eq!(get_thread_cpu_time(env, 0, &mut nanos), ERR_NONE);
        }
        assert_eq!(
            get_thread_cpu_time(env, 0, std::ptr::null_mut()),
            ERR_NULL_POINTER
        );
        let mut flag: u8 = 2;
        assert_eq!(
            is_method_obsolete(env, u64::from(u32::MAX - 7) << 32, &mut flag),
            ERR_INVALID_METHODID
        );
        assert_eq!(dispose_environment(env), ERR_NONE);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }

    /// Interpreter round i2 wave 61, lane L1: the startup functions answer
    /// HotSpot's (MEASURED, `L1W61JvmtiAgentStartup`): the VM's own
    /// properties only, `WRONG_PHASE` for `SetSystemProperty` in the live
    /// phase, the elapsed timer, `ILLEGAL_ARGUMENT` for a search segment
    /// that is no jar; and frame control answers `NOT_AVAILABLE` while its
    /// switch is off.
    #[test]
    fn the_wave_61_startup_functions_answer_through_the_c_table() {
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        let vm = shared.vm_identity;
        jni::set_jni_context_arc(Arc::clone(&shared));
        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        let env = env.cast::<JvmtiNativeEnv>();
        let read = |key: &str| -> Result<String, JInt> {
            let key = std::ffi::CString::new(key).unwrap();
            let mut value: *mut u8 = std::ptr::null_mut();
            let code = get_system_property(env, key.as_ptr(), &mut value);
            if code != ERR_NONE {
                return Err(code);
            }
            // SAFETY: a NUL-terminated `Allocate` block of this table.
            let text = unsafe { std::ffi::CStr::from_ptr(value.cast()) }
                .to_string_lossy()
                .into_owned();
            assert_eq!(deallocate(env, value), ERR_NONE);
            Ok(text)
        };
        assert_eq!(read("java.vm.specification.version"), Ok("25".to_string()));
        assert_eq!(read("user.dir"), Err(ERR_NOT_AVAILABLE));
        assert_eq!(read("l1w61.missing"), Err(ERR_NOT_AVAILABLE));
        assert_eq!(
            get_system_property(env, std::ptr::null(), &mut std::ptr::null_mut()),
            ERR_NULL_POINTER
        );
        let mut count: JInt = -1;
        let mut keys: *mut *mut u8 = std::ptr::null_mut();
        assert_eq!(get_system_properties(env, &mut count, &mut keys), ERR_NONE);
        let listed: Vec<String> = (0..usize::try_from(count).unwrap())
            .map(|i| {
                // SAFETY: `count` blocks the call allocated.
                let key = unsafe { *keys.add(i) };
                let text = unsafe { std::ffi::CStr::from_ptr(key.cast()) }
                    .to_string_lossy()
                    .into_owned();
                assert_eq!(deallocate(env, key), ERR_NONE);
                text
            })
            .collect();
        assert_eq!(deallocate(env, keys.cast()), ERR_NONE);
        assert!(listed.iter().any(|k| k == "java.home"), "{listed:?}");
        assert!(
            !listed.iter().any(|k| k == "user.dir" || k == "os.name"),
            "{listed:?}"
        );
        let x = std::ffi::CString::new("x").unwrap();
        assert_eq!(
            set_system_property(env, std::ptr::null(), x.as_ptr()),
            ERR_WRONG_PHASE
        );
        let mut info = TimerInfoRow {
            max_value: 0,
            may_skip_forward: 1,
            may_skip_backward: 1,
            kind: 0,
            reserved1: 0,
            reserved2: 0,
        };
        assert_eq!(get_timer_info(env, &mut info), ERR_NONE);
        assert_eq!(
            (
                info.max_value,
                info.may_skip_forward,
                info.may_skip_backward,
                info.kind
            ),
            (-1, 0, 0, TIMER_ELAPSED)
        );
        assert_eq!(get_timer_info(env, std::ptr::null_mut()), ERR_NULL_POINTER);
        // `SetEventNotificationMode` as HotSpot reads its arguments.
        assert_eq!(
            set_event_notification_mode(env, 7, EVENT_THREAD_START, 0),
            ERR_NONE,
            "mode 7 disables"
        );
        assert_eq!(
            set_event_notification_mode(env, 1, 49, 0),
            ERR_NONE,
            "an extension event"
        );
        assert_eq!(
            set_event_notification_mode(env, 1, 72, 0),
            ERR_NONE,
            "a reserved event"
        );
        assert_eq!(
            set_event_notification_mode(env, 1, 46, 0),
            ERR_INVALID_EVENT_TYPE
        );
        let wanted: [u32; CAPABILITY_WORDS] = [CAN_GENERATE_BREAKPOINT_EVENTS, 0, 0, 0];
        assert_eq!(add_capabilities(env, &wanted), ERR_NONE);
        assert_eq!(set_breakpoint(env, 0, 0), ERR_INVALID_METHODID);
        assert_eq!(clear_breakpoint(env, 0, 0), ERR_INVALID_METHODID);
        let plain =
            std::env::temp_dir().join(format!("l1w61-not-a-jar-{}.txt", std::process::id()));
        std::fs::write(&plain, b"not a jar").unwrap();
        let plain_c = std::ffi::CString::new(plain.to_string_lossy().into_owned()).unwrap();
        assert_eq!(
            add_to_bootstrap_class_loader_search(env, plain_c.as_ptr()),
            ERR_ILLEGAL_ARGUMENT
        );
        assert_eq!(
            add_to_system_class_loader_search(env, plain_c.as_ptr()),
            ERR_ILLEGAL_ARGUMENT
        );
        assert_eq!(
            add_to_system_class_loader_search(env, std::ptr::null()),
            ERR_NULL_POINTER
        );
        let _ = std::fs::remove_file(&plain);
        let mut hash: JInt = 0;
        assert_eq!(get_object_hash_code(env, 0, &mut hash), ERR_INVALID_OBJECT);
        let mut size: i64 = 0;
        assert_eq!(get_object_size(env, 0, &mut size), ERR_INVALID_OBJECT);
        if !shared.debug.debugger_gates.c_frame_control() {
            assert_eq!(pop_frame(env, 0), ERR_NOT_AVAILABLE);
            assert_eq!(force_early_return_int(env, 0, 1), ERR_NOT_AVAILABLE);
            assert_eq!(clear_all_frame_pops(env, 0), ERR_NOT_AVAILABLE);
            let mut caps = [0u32; CAPABILITY_WORDS];
            assert_eq!(get_potential_capabilities(env, &mut caps), ERR_NONE);
            assert_eq!(
                (caps[0] & CAN_POP_FRAME, caps[1] & CAN_FORCE_EARLY_RETURN),
                (0, 0)
            );
        }
        assert_eq!(dispose_environment(env), ERR_NONE);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }

    /// Wave 61: the OnLoad phase's VM properties: the identity and path keys
    /// with `SharedVm::new`'s initial values, the `-D` ones writable, the
    /// internal ones left out.
    #[test]
    fn the_onload_vm_properties_are_hotspots_list() {
        let mut config = crate::config::VmConfig::default();
        config.system_properties = vec![
            ("l1w61.user".to_string(), "u".to_string()),
            ("jdk.boot.class.path.append".to_string(), "b".to_string()),
        ];
        let props = onload_vm_properties(&config);
        let find = |key: &str| props.iter().find(|(k, _, _)| k == key).cloned();
        assert_eq!(
            find("java.vm.specification.version"),
            Some((
                "java.vm.specification.version".to_string(),
                "25".to_string(),
                false
            ))
        );
        assert_eq!(find("java.vm.version").map(|p| p.2), Some(false));
        assert_eq!(find("java.library.path").map(|p| p.2), Some(true));
        assert_eq!(find("java.home").map(|p| p.2), Some(true));
        assert_eq!(
            find("l1w61.user"),
            Some(("l1w61.user".to_string(), "u".to_string(), true))
        );
        assert_eq!(find("jdk.boot.class.path.append"), None);
        assert_eq!(find("java.vm.compressedOopsMode"), None);
        assert_eq!(find("user.dir"), None);
    }

    /// The i53 residual: an explicit `-Dsun.boot.library.path` is appended
    /// to the default, as HotSpot does; `-Djava.library.path` replaces it.
    #[test]
    fn i53r_an_explicit_boot_library_path_is_appended() {
        let mut config = crate::config::VmConfig::default();
        config.system_properties = vec![
            ("sun.boot.library.path".to_string(), "i53r-boot".to_string()),
            ("java.library.path".to_string(), "i53r-lib".to_string()),
        ];
        let props = onload_vm_properties(&config);
        let find = |key: &str| props.iter().find(|(k, _, _)| k == key).map(|p| p.1.clone());
        let home = crate::vm::vm_init::initial_java_home(&config);
        let sep = if cfg!(windows) { ';' } else { ':' };
        assert_eq!(
            find("sun.boot.library.path"),
            Some(format!(
                "{}{sep}i53r-boot",
                crate::vm::vm_init::initial_sun_boot_library_path(&home)
            ))
        );
        assert_eq!(find("java.library.path"), Some("i53r-lib".to_string()));
    }

    /// Interpreter round i7 wave 135, lane L1b (`CRATONVM_VM_PROPERTY_LIST_ORDER`):
    /// the OnLoad list is in HotSpot's measured order, a `-D` of a list key
    /// keeps the key's place, and the other `-D` ones follow the list.
    #[test]
    fn i135_l1b_the_onload_vm_properties_are_in_hotspots_order() {
        let mut config = crate::config::VmConfig::default();
        config.vm_property_list_order = true;
        config.system_properties = vec![
            ("l1b.user".to_string(), "u".to_string()),
            ("java.class.path".to_string(), "cp".to_string()),
        ];
        let props = onload_vm_properties(&config);
        let keys: Vec<&str> = props.iter().map(|(k, _, _)| k.as_str()).collect();
        let list: Vec<&str> = cratonvm_types::vm_properties::HOTSPOT_LIST_ORDER
            .iter()
            .copied()
            .filter(|k| *k != "java.vm.compressedOopsMode")
            .collect();
        assert_eq!(&keys[..list.len()], &list[..]);
        assert_eq!(&keys[list.len()..], &["l1b.user"]);
        config.vm_property_list_order = false;
        let old = onload_vm_properties(&config);
        assert_eq!(old.last().map(|p| p.0.as_str()), Some("java.class.path"));
    }

    static THREAD_BP_HITS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn on_thread_breakpoint(
        _env: *mut JvmtiNativeEnv,
        _jni_env: JNIEnv,
        _thread: JObject,
        _method: u64,
        _location: i64,
    ) {
        THREAD_BP_HITS.fetch_add(1, Ordering::SeqCst);
    }

    /// Per-thread enabling (wave 13): `Breakpoint` enabled for one
    /// `jthread` reaches that thread only; `VMInit` is not controllable per
    /// thread; a handle that names no VM thread is `INVALID_THREAD`.
    #[test]
    fn an_event_enabled_for_one_thread_reaches_that_thread_only() {
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        let vm = shared.vm_identity;
        jni::set_jni_context_arc(Arc::clone(&shared));
        THREAD_BP_HITS.store(0, Ordering::SeqCst);

        let name = "cratonvm/test/W13ThreadAgent";
        let cid = loop_class(&shared, name);
        let mirror = shared.mem.heap.alloc_object(cid, 0);
        shared
            .threads
            .thread_registry
            .register(ThreadId(5), "w13-t5", Some(mirror));
        let jthread = jni::obj_to_jobject(mirror);

        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        // SAFETY (every transmute below): the slot's `jvmti.h` signature.
        let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
            unsafe { std::mem::transmute(function(env, 142)) };
        let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
            unsafe { std::mem::transmute(function(env, 2)) };
        let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
            unsafe { std::mem::transmute(function(env, 122)) };
        let set_bp: extern "C" fn(*mut c_void, u64, i64) -> JInt =
            unsafe { std::mem::transmute(function(env, 38)) };
        let dispose: extern "C" fn(*mut c_void) -> JInt =
            unsafe { std::mem::transmute(function(env, 127)) };
        assert_eq!(add(env, &POTENTIAL), ERR_NONE);
        let callbacks =
            callbacks_with(&[(EVENT_BREAKPOINT, on_thread_breakpoint as *const () as usize)]);
        let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
        assert_eq!(
            set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
            ERR_NONE
        );

        assert_eq!(
            set_mode(env, 1, EVENT_VM_INIT, jthread),
            ERR_ILLEGAL_ARGUMENT
        );
        // Wave 53 (round i2, measured): `ThreadStart` is global on HotSpot,
        // `ThreadEnd` is not.
        assert_eq!(
            set_mode(env, 1, EVENT_THREAD_START, jthread),
            ERR_ILLEGAL_ARGUMENT
        );
        assert_eq!(set_mode(env, 1, EVENT_THREAD_END, jthread), ERR_NONE);
        assert_eq!(set_mode(env, 0, EVENT_THREAD_END, jthread), ERR_NONE);
        let stranger = jni::obj_to_jobject(shared.mem.heap.alloc_object(cid, 0));
        assert_eq!(
            set_mode(env, 1, EVENT_BREAKPOINT, stranger),
            ERR_INVALID_THREAD
        );
        assert_eq!(set_mode(env, 1, EVENT_BREAKPOINT, jthread), ERR_NONE);
        let jmid = u64::from(cid.as_u32()) << 32;
        assert_eq!(set_bp(env, jmid, 9), ERR_NONE);

        run_loop(&shared, cid, name, 6);
        assert_eq!(THREAD_BP_HITS.load(Ordering::SeqCst), 0, "another thread");
        run_loop(&shared, cid, name, 5);
        assert_eq!(
            THREAD_BP_HITS.load(Ordering::SeqCst),
            11,
            "the enabled thread"
        );
        assert_eq!(set_mode(env, 0, EVENT_BREAKPOINT, jthread), ERR_NONE);
        run_loop(&shared, cid, name, 5);
        assert_eq!(THREAD_BP_HITS.load(Ordering::SeqCst), 11, "disabled again");

        assert_eq!(dispose(env), ERR_NONE);
        shared.threads.thread_registry.mark_dead(ThreadId(5));
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }

    /// Wave 19: the handle checks answer what HotSpot's do. A terminated
    /// thread is `THREAD_NOT_ALIVE` (it was accepted, and the event enabled
    /// for a thread that could never post it); a handle that is neither a
    /// `jclass` nor a `Class` mirror is `INVALID_CLASS` (JNI's decode fell back
    /// to the handle's low 32 bits, so this one, a class id without the
    /// `jclass` tag, named that class).
    #[test]
    fn a_dead_thread_and_a_non_class_handle_are_refused_as_hotspot_refuses_them() {
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        let vm = shared.vm_identity;
        jni::set_jni_context_arc(Arc::clone(&shared));

        let name = "cratonvm/test/W19HandleChecks";
        let cid = loop_class(&shared, name);
        let mirror = shared.mem.heap.alloc_object(cid, 0);
        shared
            .threads
            .thread_registry
            .register(ThreadId(7), "w19-t7", Some(mirror));
        let jthread = jni::obj_to_jobject(mirror);

        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        // SAFETY (every transmute below): the slot's `jvmti.h` signature.
        let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
            unsafe { std::mem::transmute(function(env, 2)) };
        let signature: extern "C" fn(*mut c_void, JObject, *mut *mut u8, *mut *mut u8) -> JInt =
            unsafe { std::mem::transmute(function(env, 48)) };
        let status: extern "C" fn(*mut c_void, JObject, *mut JInt) -> JInt =
            unsafe { std::mem::transmute(function(env, 49)) };
        let dealloc: extern "C" fn(*mut c_void, *mut u8) -> JInt =
            unsafe { std::mem::transmute(function(env, 47)) };
        let dispose: extern "C" fn(*mut c_void) -> JInt =
            unsafe { std::mem::transmute(function(env, 127)) };

        // `ClassPrepare` needs no capability.
        assert_eq!(set_mode(env, 1, EVENT_CLASS_PREPARE, jthread), ERR_NONE);
        assert_eq!(set_mode(env, 0, EVENT_CLASS_PREPARE, jthread), ERR_NONE);
        shared.threads.thread_registry.mark_dead(ThreadId(7));
        assert_eq!(
            set_mode(env, 1, EVENT_CLASS_PREPARE, jthread),
            ERR_THREAD_NOT_ALIVE
        );

        let mut sig = std::ptr::null_mut::<u8>();
        let klass = jni::class_id_to_jclass(cid);
        assert_eq!(
            signature(env, klass, &mut sig, std::ptr::null_mut()),
            ERR_NONE
        );
        // SAFETY: a NUL-terminated string this table allocated.
        let text = unsafe { std::ffi::CStr::from_ptr(sig.cast()).to_owned() };
        assert_eq!(text.to_str(), Ok("Lcratonvm/test/W19HandleChecks;"));
        assert_eq!(dealloc(env, sig), ERR_NONE);
        let untagged = u64::from(cid.as_u32());
        assert_eq!(
            signature(env, untagged, &mut sig, std::ptr::null_mut()),
            ERR_INVALID_CLASS
        );
        let mut st = 0;
        assert_eq!(status(env, untagged, &mut st), ERR_INVALID_CLASS);
        assert_eq!(
            status(env, jthread, &mut st),
            ERR_INVALID_CLASS,
            "an object"
        );

        assert_eq!(dispose(env), ERR_NONE);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }

    static ENV_A_EXCEPTIONS: AtomicUsize = AtomicUsize::new(0);
    static ENV_B_EXCEPTIONS: AtomicUsize = AtomicUsize::new(0);

    #[allow(clippy::too_many_arguments)]
    unsafe extern "C" fn on_exception_a(
        _env: *mut JvmtiNativeEnv,
        _jni_env: JNIEnv,
        _thread: JObject,
        _method: u64,
        _location: i64,
        _exception: JObject,
        _catch_method: u64,
        _catch_location: i64,
    ) {
        ENV_A_EXCEPTIONS.fetch_add(1, Ordering::SeqCst);
    }

    #[allow(clippy::too_many_arguments)]
    unsafe extern "C" fn on_exception_b(
        _env: *mut JvmtiNativeEnv,
        _jni_env: JNIEnv,
        _thread: JObject,
        _method: u64,
        _location: i64,
        _exception: JObject,
        _catch_method: u64,
        _catch_location: i64,
    ) {
        ENV_B_EXCEPTIONS.fetch_add(1, Ordering::SeqCst);
    }

    /// Wave 14: enabling is per env. Of two C envs with an `Exception`
    /// callback, only the one that enabled the event receives it, and a Rust
    /// callback on the VM manager's own table that never enabled it receives
    /// nothing. The env's disable, its relinquished capability and its
    /// dispose each take the event off the VM: the unwinder's exact check
    /// (`exception_event_enabled_for_vm`) answers no again. Until wave 14
    /// the enable stayed on the manager for the rest of the run.
    #[test]
    fn a_c_env_gets_only_what_it_enabled_and_its_disable_reaches_the_vm() {
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        let vm = shared.vm_identity;
        jni::set_jni_context_arc(Arc::clone(&shared));
        ENV_A_EXCEPTIONS.store(0, Ordering::SeqCst);
        ENV_B_EXCEPTIONS.store(0, Ordering::SeqCst);
        let name = "cratonvm/test/W14TwoEnvs";
        let cid = loop_class(&shared, name);
        // Widening: the class id's half of the jmethodID.
        let jmid = u64::from(cid.as_u32()) << 32;
        let thrown = shared.mem.heap.alloc_object(cid, 0);
        // Cast: the object's address, as the unwinder passes it
        let address = thrown.as_ptr() as usize as u64;

        let manager = rt::manager_for_vm(vm).expect("the fixture's manager");
        let own = Arc::new(AtomicUsize::new(0));
        let own_seen = Arc::clone(&own);
        manager
            .set_event_callbacks(EventCallbacks {
                exception: Some(Box::new(move |_, _, _, _, _, _| {
                    own_seen.fetch_add(1, Ordering::SeqCst);
                })),
                ..Default::default()
            })
            .expect("installed");

        // SAFETY (every transmute below): the slot's `jvmti.h` signature;
        // both envs share the one table.
        let (_, probe) = get_env(0x3001_0200);
        let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
            unsafe { std::mem::transmute(function(probe, 142)) };
        let relinquish: extern "C" fn(*mut c_void, *const Caps) -> JInt =
            unsafe { std::mem::transmute(function(probe, 143)) };
        let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
            unsafe { std::mem::transmute(function(probe, 2)) };
        let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
            unsafe { std::mem::transmute(function(probe, 122)) };
        let dispose: extern "C" fn(*mut c_void) -> JInt =
            unsafe { std::mem::transmute(function(probe, 127)) };
        assert_eq!(dispose(probe), ERR_NONE);
        let mut envs = Vec::new();
        for callback in [
            on_exception_a as *const () as usize,
            on_exception_b as *const () as usize,
        ] {
            let (code, env) = get_env(0x3001_0200);
            assert_eq!(code, jni::JNI_OK);
            // A solo capability goes to one env at a time (HotSpot; wave 63).
            let wanted = if envs.is_empty() {
                POTENTIAL
            } else {
                without(POTENTIAL, SOLO)
            };
            assert_eq!(add(env, &wanted), ERR_NONE);
            let callbacks = callbacks_with(&[(EVENT_EXCEPTION, callback)]);
            let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
            assert_eq!(
                set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
                ERR_NONE
            );
            envs.push(env);
        }
        let (a, b) = (envs[0], envs[1]);

        // A enables `Exception`; B enables only `FramePop` (so it has a
        // delivery env on the manager too; not `Breakpoint`, an OnLoad-solo
        // capability A holds since wave 63).
        assert_eq!(set_mode(a, 1, EVENT_EXCEPTION, 0), ERR_NONE);
        assert_eq!(set_mode(b, 1, EVENT_FRAME_POP, 0), ERR_NONE);
        assert!(rt::exception_event_enabled_for_vm(vm, 0));
        rt::fire_exception_for_vm(vm, 0, jmid, 12, address, 0, -1);
        assert_eq!(ENV_A_EXCEPTIONS.load(Ordering::SeqCst), 1);
        assert_eq!(
            ENV_B_EXCEPTIONS.load(Ordering::SeqCst),
            0,
            "B never enabled it"
        );
        assert_eq!(
            own.load(Ordering::SeqCst),
            0,
            "the manager never enabled it"
        );

        // A disables it: nothing on the VM has it enabled any more.
        assert_eq!(set_mode(a, 0, EVENT_EXCEPTION, 0), ERR_NONE);
        assert!(
            !rt::exception_event_enabled_for_vm(vm, 0),
            "the unwinder's slow path is off again"
        );
        assert!(!manager.has_exception_listener());
        rt::fire_exception_for_vm(vm, 0, jmid, 12, address, 0, -1);
        assert_eq!(ENV_A_EXCEPTIONS.load(Ordering::SeqCst), 1);

        // Enabled again, then the capability relinquished.
        assert_eq!(set_mode(a, 1, EVENT_EXCEPTION, 0), ERR_NONE);
        assert!(rt::exception_event_enabled_for_vm(vm, 0));
        let exception_capability: Caps = [CAN_GENERATE_EXCEPTION_EVENTS, 0, 0, 0];
        assert_eq!(relinquish(a, &exception_capability), ERR_NONE);
        assert!(!rt::exception_event_enabled_for_vm(vm, 0));

        // Enabled again, then the env disposed.
        assert_eq!(add(a, &POTENTIAL), ERR_NONE);
        assert_eq!(set_mode(a, 1, EVENT_EXCEPTION, 0), ERR_NONE);
        assert!(rt::exception_event_enabled_for_vm(vm, 0));
        assert_eq!(dispose(a), ERR_NONE);
        assert!(!rt::exception_event_enabled_for_vm(vm, 0));
        assert!(manager.is_event_enabled(JvmtiEventKind::FramePop, Some(0)));
        assert_eq!(dispose(b), ERR_NONE);
        assert!(
            !manager.is_event_enabled(JvmtiEventKind::FramePop, Some(0)),
            "B's dispose took its FramePop off"
        );
        assert_eq!(ENV_B_EXCEPTIONS.load(Ordering::SeqCst), 0);
        assert_eq!(own.load(Ordering::SeqCst), 0);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }

    /// Wave 41 (lane L1): `GetClassSignature` and `GetMethodName` answer the
    /// `Signature` attribute as the generic signature, and NULL for a class
    /// or method without one (both answered NULL always).
    #[test]
    fn generic_signatures_are_the_signature_attributes() {
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        let vm = shared.vm_identity;
        jni::set_jni_context_arc(Arc::clone(&shared));
        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        let generic = loop_class(&shared, "cratonvm/test/W41Generic");
        let plain = loop_class(&shared, "cratonvm/test/W41Plain");
        {
            let mut cm = shared.classes.class_manager.write();
            let class = cm.get_class_mut(generic).expect("fabricated");
            class.signature = Some("<T:Ljava/lang/Object;>Ljava/lang/Object;".to_string());
            class.methods[0]
                .attributes
                .push(LazyAttribute::Decoded(Attribute::Signature(Arc::from(
                    "<U:Ljava/lang/Object;>()I",
                ))));
        }
        // Widening: the class id's half of the jmethodID.
        let generic_jmid = u64::from(generic.as_u32()) << 32;
        let plain_jmid = u64::from(plain.as_u32()) << 32;

        // SAFETY (every transmute below): the slot's `jvmti.h` signature.
        let class_sig: extern "C" fn(*mut c_void, JObject, *mut *mut u8, *mut *mut u8) -> JInt =
            unsafe { std::mem::transmute(function(env, 48)) };
        let method_name: extern "C" fn(
            *mut c_void,
            u64,
            *mut *mut u8,
            *mut *mut u8,
            *mut *mut u8,
        ) -> JInt = unsafe { std::mem::transmute(function(env, 64)) };
        let dealloc: extern "C" fn(*mut c_void, *mut u8) -> JInt =
            unsafe { std::mem::transmute(function(env, 47)) };
        let dispose: extern "C" fn(*mut c_void) -> JInt =
            unsafe { std::mem::transmute(function(env, 127)) };
        // The text of an answered block, freed; `None` for NULL.
        let text = |block: *mut u8| -> Option<String> {
            if block.is_null() {
                return None;
            }
            // SAFETY: a NUL-terminated block the table allocated.
            let s = unsafe { std::ffi::CStr::from_ptr(block.cast()) }
                .to_string_lossy()
                .into_owned();
            assert_eq!(dealloc(env, block), ERR_NONE);
            Some(s)
        };

        // A non-NULL value the table must overwrite with NULL.
        let marker = std::ptr::NonNull::<u8>::dangling().as_ptr();
        let (mut sig, mut generic_out) = (std::ptr::null_mut::<u8>(), marker);
        assert_eq!(
            class_sig(
                env,
                jni::class_id_to_jclass(generic),
                &mut sig,
                &mut generic_out
            ),
            ERR_NONE
        );
        assert_eq!(text(sig).as_deref(), Some("Lcratonvm/test/W41Generic;"));
        assert_eq!(
            text(generic_out).as_deref(),
            Some("<T:Ljava/lang/Object;>Ljava/lang/Object;")
        );
        let (mut sig, mut generic_out) = (std::ptr::null_mut::<u8>(), marker);
        assert_eq!(
            class_sig(
                env,
                jni::class_id_to_jclass(plain),
                &mut sig,
                &mut generic_out
            ),
            ERR_NONE
        );
        assert_eq!(text(sig).as_deref(), Some("Lcratonvm/test/W41Plain;"));
        assert!(generic_out.is_null(), "no Signature attribute: NULL");

        let (mut name, mut desc, mut generic_out) = (
            std::ptr::null_mut::<u8>(),
            std::ptr::null_mut::<u8>(),
            marker,
        );
        assert_eq!(
            method_name(env, generic_jmid, &mut name, &mut desc, &mut generic_out),
            ERR_NONE
        );
        assert_eq!(text(name).as_deref(), Some("jvmtiBpLoop"));
        assert_eq!(text(desc).as_deref(), Some("()I"));
        assert_eq!(
            text(generic_out).as_deref(),
            Some("<U:Ljava/lang/Object;>()I")
        );
        let mut generic_out = marker;
        assert_eq!(
            method_name(
                env,
                plain_jmid,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut generic_out
            ),
            ERR_NONE
        );
        assert!(generic_out.is_null(), "no Signature attribute: NULL");

        assert_eq!(dispose(env), ERR_NONE);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }

    /// JVM type signatures of a class, an array and a primitive.
    #[test]
    fn class_signatures_follow_the_jvm_type_grammar() {
        assert_eq!(class_signature("java/lang/String"), "Ljava/lang/String;");
        assert_eq!(
            class_signature("[Ljava/lang/String;"),
            "[Ljava/lang/String;"
        );
        assert_eq!(class_signature("[I"), "[I");
        assert_eq!(class_signature("int"), "I");
        assert_eq!(class_signature("void"), "V");
    }

    /// Modified UTF-8, as JVMTI strings are: NUL is `C0 80`, a supplementary
    /// character its two surrogates of three bytes each.
    #[test]
    fn strings_are_modified_utf8() {
        let mem = allocate_modified_utf8("a\u{0}\u{1F600}");
        assert!(!mem.is_null());
        // SAFETY: a NUL-terminated block allocated just above.
        let bytes = unsafe { std::ffi::CStr::from_ptr(mem.cast()).to_bytes().to_vec() };
        assert_eq!(
            bytes,
            vec![b'a', 0xC0, 0x80, 0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x80]
        );
        // SAFETY: allocated above, freed once.
        unsafe { free_block(mem) };
    }

    static METHOD_WATCHED: AtomicU64 = AtomicU64::new(0);
    static METHOD_ENTRIES: AtomicUsize = AtomicUsize::new(0);
    static METHOD_EXITS: AtomicUsize = AtomicUsize::new(0);
    static METHOD_EXIT_VALUE: AtomicU64 = AtomicU64::new(0);
    static METHOD_EXIT_POPPED: AtomicUsize = AtomicUsize::new(0);

    // No assertion in here: a panic cannot leave an `extern "C"` function.
    unsafe extern "C" fn on_method_entry(
        _env: *mut JvmtiNativeEnv,
        _jni_env: JNIEnv,
        _thread: JObject,
        method: u64,
    ) {
        if method == METHOD_WATCHED.load(Ordering::SeqCst) {
            METHOD_ENTRIES.fetch_add(1, Ordering::SeqCst);
        }
    }

    // No assertion in here: a panic cannot leave an `extern "C"` function.
    unsafe extern "C" fn on_method_exit(
        _env: *mut JvmtiNativeEnv,
        _jni_env: JNIEnv,
        _thread: JObject,
        method: u64,
        was_popped_by_exception: u8,
        value: u64,
    ) {
        if method == METHOD_WATCHED.load(Ordering::SeqCst) {
            METHOD_EXITS.fetch_add(1, Ordering::SeqCst);
            METHOD_EXIT_VALUE.store(value, Ordering::SeqCst);
            METHOD_EXIT_POPPED.store(usize::from(was_popped_by_exception), Ordering::SeqCst);
        }
    }

    /// Wave 17 (proposal `interpreter-L1-proposal-c-jvmti-table-interpreter-events-through-the-delivery-env-FIXED-20261016`,
    /// stage 1): a C env with `can_generate_method_entry_events` /
    /// `can_generate_method_exit_events` gets `MethodEntry` and `MethodExit`
    /// (the `jint` result in the `jvalue`, not popped by an exception)
    /// through `jvmti.h`'s members 15 and 16, the VM runs interpreted while
    /// they are enabled, and a disable or a dispose takes each off.
    #[test]
    fn a_c_env_gets_method_entry_and_exit_with_the_return_value() {
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        let vm = shared.vm_identity;
        // `jvmti_method_id` resolves real ids through the VM's bridge.
        rt::install_real_agent_env_bridge(&shared);
        jni::set_jni_context_arc(Arc::clone(&shared));
        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        let name = "cratonvm/test/W17MethodEvents";
        let cid = loop_class(&shared, name);
        // Widening: the class id's half of the jmethodID.
        METHOD_WATCHED.store(u64::from(cid.as_u32()) << 32, Ordering::SeqCst);
        METHOD_ENTRIES.store(0, Ordering::SeqCst);
        METHOD_EXITS.store(0, Ordering::SeqCst);
        METHOD_EXIT_VALUE.store(0, Ordering::SeqCst);
        METHOD_EXIT_POPPED.store(7, Ordering::SeqCst);

        // SAFETY (every transmute below): the slot's `jvmti.h` signature.
        let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
            unsafe { std::mem::transmute(function(env, 2)) };
        let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
            unsafe { std::mem::transmute(function(env, 122)) };
        let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
            unsafe { std::mem::transmute(function(env, 142)) };
        let dispose: extern "C" fn(*mut c_void) -> JInt =
            unsafe { std::mem::transmute(function(env, 127)) };

        assert_eq!(
            set_mode(env, 1, EVENT_METHOD_ENTRY, 0),
            ERR_MUST_POSSESS_CAPABILITY
        );
        let method_events: Caps = [
            CAN_GENERATE_METHOD_ENTRY_EVENTS | CAN_GENERATE_METHOD_EXIT_EVENTS,
            0,
            0,
            0,
        ];
        assert_eq!(add(env, &method_events), ERR_NONE);
        let callbacks = callbacks_with(&[
            (EVENT_METHOD_ENTRY, on_method_entry as *const () as usize),
            (EVENT_METHOD_EXIT, on_method_exit as *const () as usize),
        ]);
        // The agent's structure, as long as `jvmti.h` makes it up to
        // `MethodExit` (member 16).
        let size = JInt::try_from(17 * std::mem::size_of::<usize>()).expect("small");
        assert_eq!(
            set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
            ERR_NONE
        );
        let gates = &shared.debug.debugger_gates;
        assert!(!gates.jvmti_method_events_armed());
        assert_eq!(set_mode(env, 1, EVENT_METHOD_ENTRY, 0), ERR_NONE);
        assert_eq!(set_mode(env, 1, EVENT_METHOD_EXIT, 0), ERR_NONE);
        assert!(
            rt::interp_only_events_active_for_vm(vm),
            "the VM runs interpreted while they are enabled"
        );
        // Wave 27: the native-call funnel's gate follows them.
        assert!(gates.jvmti_method_events_armed() && gates.method_events_armed());
        assert!(!gates.jdwp_method_events_armed());

        run_loop(&shared, cid, name, 0);
        assert_eq!(METHOD_ENTRIES.load(Ordering::SeqCst), 1);
        assert_eq!(METHOD_EXITS.load(Ordering::SeqCst), 1);
        assert_eq!(
            METHOD_EXIT_VALUE.load(Ordering::SeqCst),
            45,
            "the jint result"
        );
        assert_eq!(METHOD_EXIT_POPPED.load(Ordering::SeqCst), 0);

        assert_eq!(set_mode(env, 0, EVENT_METHOD_ENTRY, 0), ERR_NONE);
        assert!(
            gates.jvmti_method_events_armed(),
            "MethodExit is still enabled"
        );
        run_loop(&shared, cid, name, 0);
        assert_eq!(METHOD_ENTRIES.load(Ordering::SeqCst), 1, "disabled");
        assert_eq!(METHOD_EXITS.load(Ordering::SeqCst), 2);

        assert_eq!(dispose(env), ERR_NONE);
        assert!(
            !rt::interp_only_events_active_for_vm(vm),
            "dispose takes it off"
        );
        assert!(!gates.method_events_armed(), "and the funnel's gate");
        run_loop(&shared, cid, name, 0);
        assert_eq!(METHOD_EXITS.load(Ordering::SeqCst), 2, "disposed");
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }

    /// Wave 17 (proposal `interpreter-L1-proposal-jvmti-c-table-stack-and-line-functions-RETIRED-20261015`,
    /// stage 1): the class and method facts through the C table —
    /// `GetClassStatus`, `GetMethodModifiers`, `IsMethodNative`,
    /// `GetMethodLocation` and, with `can_get_line_numbers`,
    /// `GetLineNumberTable` — on a Java method with a line table and a native
    /// method, with HotSpot's errors for a native method, a method without a
    /// table, a bad id and a NULL out-parameter.
    #[test]
    fn the_c_table_answers_class_status_method_facts_and_line_numbers() {
        use cratonvm_reader::attribute::LineNumberEntry;
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        let vm = shared.vm_identity;
        jni::set_jni_context_arc(Arc::clone(&shared));
        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        let name = "cratonvm/test/W17LineAgent";
        let cid = loop_class(&shared, name);
        let bare = loop_class(&shared, "cratonvm/test/W17NoLines");
        {
            let mut cm = shared.classes.class_manager.write();
            let class = cm.get_class_mut(cid).expect("fabricated");
            if let Some(LazyAttribute::Decoded(Attribute::Code(code))) =
                class.methods[0].attributes.get_mut(0)
            {
                code.attributes.push(Attribute::LineNumberTable(vec![
                    LineNumberEntry {
                        start_pc: 7,
                        line_number: 4,
                    },
                    LineNumberEntry {
                        start_pc: 0,
                        line_number: 3,
                    },
                    LineNumberEntry {
                        start_pc: 22,
                        line_number: 6,
                    },
                ]));
            }
            class
                .methods
                .push(cratonvm_reader::method::ClassFileMethod {
                    access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
                    name: Arc::from("nat"),
                    descriptor: Arc::from("()V"),
                    attributes: vec![],
                });
        }
        // Widening: the class id's half of the jmethodID.
        let jmid = u64::from(cid.as_u32()) << 32;
        let native_jmid = jmid | 1;
        let bare_jmid = u64::from(bare.as_u32()) << 32;

        // SAFETY (every transmute below): the slot's `jvmti.h` signature.
        let status: extern "C" fn(*mut c_void, JObject, *mut JInt) -> JInt =
            unsafe { std::mem::transmute(function(env, 49)) };
        let modifiers: extern "C" fn(*mut c_void, u64, *mut JInt) -> JInt =
            unsafe { std::mem::transmute(function(env, 66)) };
        let lines: extern "C" fn(*mut c_void, u64, *mut JInt, *mut *mut LineNumberRow) -> JInt =
            unsafe { std::mem::transmute(function(env, 70)) };
        let location: extern "C" fn(*mut c_void, u64, *mut i64, *mut i64) -> JInt =
            unsafe { std::mem::transmute(function(env, 71)) };
        let native: extern "C" fn(*mut c_void, u64, *mut u8) -> JInt =
            unsafe { std::mem::transmute(function(env, 76)) };
        let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
            unsafe { std::mem::transmute(function(env, 142)) };
        let dealloc: extern "C" fn(*mut c_void, *mut u8) -> JInt =
            unsafe { std::mem::transmute(function(env, 47)) };
        let dispose: extern "C" fn(*mut c_void) -> JInt =
            unsafe { std::mem::transmute(function(env, 127)) };

        // A fabricated class is initialized: VERIFIED | PREPARED | INITIALIZED.
        let mut s = -1;
        assert_eq!(status(env, jni::class_id_to_jclass(cid), &mut s), ERR_NONE);
        assert_eq!(s, 7);
        assert_eq!(status(env, 0, &mut s), ERR_INVALID_CLASS);
        assert_eq!(
            status(env, jni::class_id_to_jclass(cid), std::ptr::null_mut()),
            ERR_NULL_POINTER
        );

        let mut m = 0;
        assert_eq!(modifiers(env, jmid, &mut m), ERR_NONE);
        assert_eq!(m, 0x0008, "static");
        assert_eq!(modifiers(env, native_jmid, &mut m), ERR_NONE);
        assert_eq!(m, 0x0101, "public native");
        assert_eq!(modifiers(env, jmid | 7, &mut m), ERR_INVALID_METHODID);

        let mut n = 2u8;
        assert_eq!(native(env, jmid, &mut n), ERR_NONE);
        assert_eq!(n, 0);
        assert_eq!(native(env, native_jmid, &mut n), ERR_NONE);
        assert_eq!(n, 1);

        let (mut start, mut end) = (7i64, 7i64);
        assert_eq!(location(env, jmid, &mut start, &mut end), ERR_NONE);
        assert_eq!((start, end), (0, 23), "the loop's code is 24 bytes");
        assert_eq!(
            location(env, native_jmid, &mut start, &mut end),
            ERR_NATIVE_METHOD
        );

        let (mut count, mut table) = (0, std::ptr::null_mut::<LineNumberRow>());
        assert_eq!(
            lines(env, jmid, &mut count, &mut table),
            ERR_MUST_POSSESS_CAPABILITY
        );
        let line_numbers: Caps = [CAN_GET_LINE_NUMBERS, 0, 0, 0];
        assert_eq!(add(env, &line_numbers), ERR_NONE);
        assert_eq!(lines(env, jmid, &mut count, &mut table), ERR_NONE);
        assert_eq!(count, 3);
        // SAFETY: a block of `count` rows this table allocated.
        let rows = unsafe { std::slice::from_raw_parts(table, 3) }.to_vec();
        assert_eq!(
            rows,
            vec![
                LineNumberRow {
                    start_location: 0,
                    line_number: 3
                },
                LineNumberRow {
                    start_location: 7,
                    line_number: 4
                },
                LineNumberRow {
                    start_location: 22,
                    line_number: 6
                },
            ],
            "by start location"
        );
        assert_eq!(dealloc(env, table.cast::<u8>()), ERR_NONE);
        assert_eq!(
            lines(env, bare_jmid, &mut count, &mut table),
            ERR_ABSENT_INFORMATION
        );
        assert_eq!(
            lines(env, native_jmid, &mut count, &mut table),
            ERR_NATIVE_METHOD
        );
        assert_eq!(
            lines(env, jmid, std::ptr::null_mut(), &mut table),
            ERR_NULL_POINTER
        );

        assert_eq!(dispose(env), ERR_NONE);
        assert_eq!(modifiers(env, jmid, &mut m), ERR_INVALID_ENVIRONMENT);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }

    static W18_COLLECTIONS: AtomicUsize = AtomicUsize::new(0);
    static W18_SECOND_CLASS: AtomicU64 = AtomicU64::new(0);

    /// The first env of the wave-18 test: its callback collects, as one that
    /// allocates through JNI may.
    #[allow(clippy::too_many_arguments)]
    unsafe extern "C" fn on_exception_collecting(
        _env: *mut JvmtiNativeEnv,
        _jni_env: JNIEnv,
        _thread: JObject,
        _method: u64,
        _location: i64,
        _exception: JObject,
        _catch_method: u64,
        _catch_location: i64,
    ) {
        if let Some(vm) = jni::calling_thread_vm() {
            let mut t = JvmThread::new(ThreadId(0), "w18-collecting-agent");
            crate::runtime::interpreter::maybe_gc_forced_pub_at(&vm, &mut t, "w18-jvmti-c");
            W18_COLLECTIONS.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The second env: the class of the object its `exception` names.
    #[allow(clippy::too_many_arguments)]
    unsafe extern "C" fn on_exception_checking(
        _env: *mut JvmtiNativeEnv,
        _jni_env: JNIEnv,
        _thread: JObject,
        _method: u64,
        _location: i64,
        exception: JObject,
        _catch_method: u64,
        _catch_location: i64,
    ) {
        let class = jni::jobject_to_obj(exception)
            .zip(jni::calling_thread_vm())
            .map_or(0, |(obj, vm)| {
                u64::from(vm.mem.heap.class_id_of(obj).as_u32())
            });
        W18_SECOND_CLASS.store(class, Ordering::SeqCst);
    }

    /// Interpreter round i1 wave 18, lane L1 (page
    /// `interpreter-L2-jvmti-event-object-address-stale-for-the-next-listener-FIXED-20260925`): of
    /// two C envs enabled for `Exception`, the first one's callback collects,
    /// and the second one's `exception` local reference still names the
    /// thrown object. It was made from the address read before the first
    /// callback ran; the manager now holds the object in a global reference
    /// for the delivery and hands each env its current address.
    #[test]
    fn a_second_c_env_gets_the_exception_after_the_first_one_collected() {
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        rt::install_real_agent_env_bridge(&shared);
        let vm = shared.vm_identity;
        jni::set_jni_context_arc(Arc::clone(&shared));
        W18_COLLECTIONS.store(0, Ordering::SeqCst);
        W18_SECOND_CLASS.store(0, Ordering::SeqCst);
        let name = "cratonvm/test/W18CollectingEnvs";
        let cid = loop_class(&shared, name);
        // Widening: the class id's half of the jmethodID.
        let jmid = u64::from(cid.as_u32()) << 32;
        let thrown = shared.mem.heap.alloc_object(cid, 0);
        // The producer's own root (the unwinder pins the exception).
        let pin = shared.natives.jni_global_refs.lock().add(thrown);

        // SAFETY (every transmute below): the slot's `jvmti.h` signature;
        // both envs share the one table.
        let (_, probe) = get_env(0x3001_0200);
        let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
            unsafe { std::mem::transmute(function(probe, 142)) };
        let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
            unsafe { std::mem::transmute(function(probe, 2)) };
        let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
            unsafe { std::mem::transmute(function(probe, 122)) };
        let dispose: extern "C" fn(*mut c_void) -> JInt =
            unsafe { std::mem::transmute(function(probe, 127)) };
        assert_eq!(dispose(probe), ERR_NONE);
        let mut envs = Vec::new();
        for callback in [
            on_exception_collecting as *const () as usize,
            on_exception_checking as *const () as usize,
        ] {
            let (code, env) = get_env(0x3001_0200);
            assert_eq!(code, jni::JNI_OK);
            // A solo capability goes to one env at a time (HotSpot; wave 63).
            let wanted = if envs.is_empty() {
                POTENTIAL
            } else {
                without(POTENTIAL, SOLO)
            };
            assert_eq!(add(env, &wanted), ERR_NONE);
            let callbacks = callbacks_with(&[(EVENT_EXCEPTION, callback)]);
            let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
            assert_eq!(
                set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
                ERR_NONE
            );
            // The first env's delivery registers first, so it runs first.
            assert_eq!(set_mode(env, 1, EVENT_EXCEPTION, 0), ERR_NONE);
            envs.push(env);
        }

        // Cast: the object's address, as the unwinder passes it
        let address = thrown.as_ptr() as usize as u64;
        rt::fire_exception_for_vm(vm, 0, jmid, 12, address, 0, -1);
        assert_eq!(
            W18_COLLECTIONS.load(Ordering::SeqCst),
            1,
            "the first env collected"
        );
        assert_eq!(
            W18_SECOND_CLASS.load(Ordering::SeqCst),
            u64::from(cid.as_u32()),
            "the second env's reference names the thrown object"
        );

        for env in envs {
            assert_eq!(dispose(env), ERR_NONE);
        }
        let _ = shared.natives.jni_global_refs.lock().remove(pin);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }

    /// The locations the `SingleStep` callback of
    /// [`a_c_env_gets_single_step_for_every_bytecode`] saw.
    static W22_STEPS: std::sync::Mutex<Vec<i64>> = std::sync::Mutex::new(Vec::new());

    // No assertion in here: a panic cannot leave an `extern "C"` function.
    unsafe extern "C" fn on_single_step(
        _env: *mut JvmtiNativeEnv,
        _jni_env: JNIEnv,
        _thread: JObject,
        _method: u64,
        location: i64,
    ) {
        if let Ok(mut steps) = W22_STEPS.lock() {
            steps.push(location);
        }
    }

    /// Interpreter round i1 wave 22, lane L1
    /// (`interpreter-L1-proposal-c-jvmti-table-interpreter-events-through-the-delivery-env-FIXED-20261016`,
    /// stage 2): `SingleStep` is delivered through the C table — its
    /// capability is granted, the event (member 11 of `jvmtiEventCallbacks`)
    /// reaches the agent before each bytecode of the ten-trip loop, starting
    /// at 0 and ending at the `ireturn`, and a disable takes it off. Until
    /// wave 22 the interpreter gated the event on a per-thread flag nothing
    /// raised, so no agent received one.
    #[test]
    fn a_c_env_gets_single_step_for_every_bytecode() {
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        let vm = shared.vm_identity;
        jni::set_jni_context_arc(Arc::clone(&shared));
        if let Ok(mut steps) = W22_STEPS.lock() {
            steps.clear();
        }
        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        // SAFETY (every transmute below): the slot's `jvmti.h` signature.
        let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
            unsafe { std::mem::transmute(function(env, 142)) };
        let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
            unsafe { std::mem::transmute(function(env, 2)) };
        let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
            unsafe { std::mem::transmute(function(env, 122)) };
        let dispose: extern "C" fn(*mut c_void) -> JInt =
            unsafe { std::mem::transmute(function(env, 127)) };
        assert_eq!(
            set_mode(env, 1, EVENT_SINGLE_STEP, 0),
            ERR_MUST_POSSESS_CAPABILITY
        );
        let single_step: Caps = [CAN_GENERATE_SINGLE_STEP_EVENTS, 0, 0, 0];
        assert_eq!(add(env, &single_step), ERR_NONE);
        let callbacks =
            callbacks_with(&[(EVENT_SINGLE_STEP, on_single_step as *const () as usize)]);
        let size = JInt::try_from(13 * std::mem::size_of::<usize>()).expect("small");
        assert_eq!(
            set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
            ERR_NONE
        );
        let gates = &shared.debug.debugger_gates;
        assert!(!gates.jvmti_frames_armed());
        assert_eq!(set_mode(env, 1, EVENT_SINGLE_STEP, 0), ERR_NONE);
        assert!(
            rt::interp_only_events_active_for_vm(vm),
            "the VM runs interpreted while an agent steps"
        );
        // Wave 29: the native-call funnel's gate follows the step, so a
        // stood-in Java method runs its bytecode and the agent steps into it
        // (`jvmti_events::run_stood_in_java_method`); no native method event
        // is armed by it.
        assert!(gates.jvmti_frames_armed() && gates.method_events_armed());
        assert!(!gates.jvmti_method_events_armed() && !gates.jdwp_method_events_armed());

        let name = "cratonvm/test/W22StepAgent";
        let cid = loop_class(&shared, name);
        run_loop(&shared, cid, name, 0x0F22);
        let steps = W22_STEPS.lock().expect("the callback's record").clone();
        // The instruction starts of `loop_code`.
        let starts = [0, 2, 3, 4, 5, 6, 7, 8, 9, 12, 13, 14, 15, 16, 19, 22, 23];
        assert_eq!(steps.first(), Some(&0), "the first bytecode: {steps:?}");
        assert_eq!(steps.last(), Some(&23), "the ireturn: {steps:?}");
        assert!(
            steps.iter().all(|at| starts.contains(at)),
            "only instruction starts: {steps:?}"
        );
        assert!(
            steps.iter().filter(|&&at| at == 9).count() >= 11,
            "the loop test runs eleven times: {steps:?}"
        );

        assert_eq!(set_mode(env, 0, EVENT_SINGLE_STEP, 0), ERR_NONE);
        assert!(
            !gates.method_events_armed(),
            "the funnel's gate follows the disable"
        );
        run_loop(&shared, cid, name, 0x0F22);
        assert_eq!(
            W22_STEPS.lock().expect("the callback's record").len(),
            steps.len(),
            "disabled: no more steps"
        );
        assert_eq!(dispose(env), ERR_NONE);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }

    /// What the stack functions answered inside the first `Breakpoint`
    /// callback of [`the_c_table_answers_the_current_threads_stack_in_a_callback`]:
    /// `(error, value)` pairs in the order the callback asks them.
    static W22_STACK: std::sync::Mutex<Vec<(JInt, i64)>> = std::sync::Mutex::new(Vec::new());

    // No assertion in here: a panic cannot leave an `extern "C"` function.
    unsafe extern "C" fn on_breakpoint_stack(
        env: *mut JvmtiNativeEnv,
        _jni_env: JNIEnv,
        _thread: JObject,
        _method: u64,
        _location: i64,
    ) {
        let Ok(mut seen) = W22_STACK.lock() else {
            return;
        };
        if !seen.is_empty() {
            return;
        }
        let mut count: JInt = -1;
        let code = get_frame_count(env, 0, &mut count);
        seen.push((code, i64::from(count)));
        let (mut method, mut location) = (0u64, -2i64);
        let code = get_frame_location(env, 0, 0, &mut method, &mut location);
        // Cast: the jmethodID's bits, to share the vector.
        seen.push((code, method as i64));
        seen.push((code, location));
        let code = get_frame_location(env, 0, 1, &mut method, &mut location);
        seen.push((code, 0));
        let mut buffer = [FrameInfo {
            method: 0,
            location: -2,
        }; 4];
        let mut n: JInt = -1;
        let code = get_stack_trace(env, 0, 0, 4, buffer.as_mut_ptr(), &mut n);
        seen.push((code, i64::from(n)));
        seen.push((code, buffer[0].location));
        let code = get_stack_trace(env, 0, -1, 4, buffer.as_mut_ptr(), &mut n);
        seen.push((code, i64::from(n)));
        let code = get_stack_trace(env, 0, 1, 4, buffer.as_mut_ptr(), &mut n);
        seen.push((code, 0));
        let code = get_stack_trace(env, 0, -2, 4, buffer.as_mut_ptr(), &mut n);
        seen.push((code, 0));
        let code = get_stack_trace(env, 0, 0, -1, buffer.as_mut_ptr(), &mut n);
        seen.push((code, 0));
        let code = get_frame_location(env, 0, -1, &mut method, &mut location);
        seen.push((code, 0));
        let mut thread: JObject = 7;
        let code = get_current_thread(env, &mut thread);
        // Cast: the handle's bits (NULL: this thread has no mirror).
        seen.push((code, thread as i64));
    }

    /// Interpreter round i1 wave 22, lane L1
    /// (`interpreter-L1-proposal-jvmti-c-table-stack-and-line-functions-RETIRED-20261015`, stage 2):
    /// inside a `Breakpoint` callback the current thread's stack is answered
    /// through the table's slots 16, 18, 19 and 104 — one frame, the loop
    /// method at the breakpoint's bytecode index — with the specification's
    /// errors past the bottom, for a bad start depth or count, and a thread
    /// the VM does not know is `UNATTACHED_THREAD`.
    #[test]
    fn the_c_table_answers_the_current_threads_stack_in_a_callback() {
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        let vm = shared.vm_identity;
        jni::set_jni_context_arc(Arc::clone(&shared));
        if let Ok(mut seen) = W22_STACK.lock() {
            seen.clear();
        }
        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        assert_eq!(function(env, 16), get_frame_count as *const () as usize);
        assert_eq!(function(env, 18), get_current_thread as *const () as usize);
        assert_eq!(function(env, 19), get_frame_location as *const () as usize);
        assert_eq!(function(env, 104), get_stack_trace as *const () as usize);
        // SAFETY (every transmute below): the slot's `jvmti.h` signature.
        let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
            unsafe { std::mem::transmute(function(env, 142)) };
        let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
            unsafe { std::mem::transmute(function(env, 2)) };
        let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
            unsafe { std::mem::transmute(function(env, 122)) };
        let set_bp: extern "C" fn(*mut c_void, u64, i64) -> JInt =
            unsafe { std::mem::transmute(function(env, 38)) };
        let dispose: extern "C" fn(*mut c_void) -> JInt =
            unsafe { std::mem::transmute(function(env, 127)) };
        assert_eq!(add(env, &POTENTIAL), ERR_NONE, "binds the env to the VM");

        // An OS thread the VM does not know.
        let env_addr = env as usize; // Cast: the env's address, to cross threads
        let unattached = std::thread::spawn(move || {
            let mut count: JInt = -1;
            let env = env_addr as *mut JvmtiNativeEnv; // Cast: back to the env
            (get_frame_count(env, 0, &mut count), count)
        })
        .join()
        .expect("the probe thread");
        assert_eq!(unattached, (ERR_UNATTACHED_THREAD, -1));

        let name = "cratonvm/test/W22StackAgent";
        let cid = loop_class(&shared, name);
        // Widening: the class id's half of the jmethodID.
        let jmid = u64::from(cid.as_u32()) << 32;
        let callbacks =
            callbacks_with(&[(EVENT_BREAKPOINT, on_breakpoint_stack as *const () as usize)]);
        let size = JInt::try_from(13 * std::mem::size_of::<usize>()).expect("small");
        assert_eq!(
            set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
            ERR_NONE
        );
        assert_eq!(set_mode(env, 1, EVENT_BREAKPOINT, 0), ERR_NONE);
        assert_eq!(set_bp(env, jmid, 9), ERR_NONE);

        // The loop runs on a registered thread whose `JvmThread` is published,
        // as every Java thread's is, so the callback's context names it.
        let registry = &shared.threads.thread_registry;
        let tid = registry.next_thread_id();
        registry.register(tid, "w22-stack", None);
        let mut thread = JvmThread::new(tid, "w22-stack");
        registry.set_jvm_thread_addr(tid, &thread as *const JvmThread as usize);
        let frame = Frame::new(
            cid,
            name.to_string(),
            "jvmtiBpLoop".to_string(),
            "()I".to_string(),
            None,
            loop_code(),
            Vec::new(),
            2,
            3,
            &[],
        );
        let r = crate::runtime::interpreter::execute_prebuilt_frame(&shared, &mut thread, frame);
        assert!(matches!(r, Ok(Some(Value::Int(45)))), "answer: {r:?}");
        registry.mark_dead(tid);

        let seen = W22_STACK.lock().expect("the callback's record").clone();
        // Cast: the jmethodID's bits, as the callback stored them.
        let jmid_bits = jmid as i64;
        assert_eq!(
            seen,
            vec![
                (ERR_NONE, 1),
                (ERR_NONE, jmid_bits),
                (ERR_NONE, 9),
                (ERR_NO_MORE_FRAMES, 0),
                (ERR_NONE, 1),
                (ERR_NONE, 9),
                (ERR_NONE, 1),
                (ERR_ILLEGAL_ARGUMENT, 0),
                (ERR_ILLEGAL_ARGUMENT, 0),
                (ERR_ILLEGAL_ARGUMENT, 0),
                (ERR_ILLEGAL_ARGUMENT, 0),
                (ERR_NONE, 0),
            ],
            "count, top location, past the bottom, the trace, the bottom one, \
             start 1 of 1, start -2 of 1, a negative count, a negative depth, \
             the current thread (no mirror)"
        );
        assert_eq!(
            stack_trace_start(0, 0),
            Some(0),
            "an empty stack from the top"
        );
        assert_eq!(stack_trace_start(0, -1), None);
        assert_eq!(stack_trace_start(3, -3), Some(0));
        assert_eq!(stack_trace_start(3, 2), Some(2));
        assert_eq!(stack_trace_start(3, 3), None);

        assert_eq!(dispose(env), ERR_NONE);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }

    /// Interpreter round i1 wave 42, lane L1: a running native's row goes
    /// right before the position the capture gave it (wave 44, lane L3: the
    /// first frame its call put on the stack), or on top; natives at one
    /// position keep their order.
    #[test]
    fn native_rows_are_spliced_below_the_first_frame_their_call_did_not_have() {
        let entry = |name: &str| {
            crate::runtime::stackwalker::synthetic_entry(Arc::from("C"), Arc::from(name))
        };
        let names = |rows: &[FrameRow<'_>]| -> Vec<String> {
            rows.iter()
                .map(|row| match row {
                    FrameRow::Java(_, e) | FrameRow::Spliced(e) => e.method_name.to_string(),
                    FrameRow::Native(_, hash) => format!("N{hash}"),
                    FrameRow::Inferred(class, _) => format!("P{}", class.as_u32()),
                })
                .collect()
        };
        let c = ClassId::new(7);
        // main, a, b: interpreter frames 0..3, one row each.
        let trace = vec![entry("main"), entry("a"), entry("b")];
        let rows = splice_native_rows(&trace, &[3], &[(3, c, 1, 0)]);
        assert_eq!(
            names(&rows),
            ["main", "a", "b", "N1"],
            "called by b: on top"
        );
        let rows = splice_native_rows(&trace, &[usize::MAX], &[(3, c, 1, 0)]);
        assert_eq!(
            names(&rows),
            ["main", "a", "b", "N1"],
            "past the end: on top"
        );
        let rows = splice_native_rows(&trace, &[2], &[(2, c, 1, 0)]);
        assert_eq!(
            names(&rows),
            ["main", "a", "N1", "b"],
            "called by a, b is its upcall"
        );
        let rows = splice_native_rows(
            &trace,
            &[1, 1, 3],
            &[(1, c, 1, 0), (1, c, 2, 0), (3, c, 3, 0)],
        );
        assert_eq!(
            names(&rows),
            ["main", "N1", "N2", "a", "b", "N3"],
            "nested natives at one position keep their order"
        );
        // main, a compiled caller, a compiled method the native's own upcall
        // entered, b: the row goes between the two compiled activations.
        let trace = vec![
            entry("main"),
            entry("compiled"),
            entry("upcalled"),
            entry("b"),
        ];
        let rows = splice_native_rows(&trace, &[2], &[(1, c, 1, 1)]);
        assert_eq!(
            names(&rows),
            ["main", "compiled", "N1", "upcalled", "b"],
            "above the compiled caller, below the upcall's compiled target"
        );
        // Wave 46 (lane L1): `trace` is the four-row one here; this row
        // expected wave 42's first trace's names since wave 44 renamed it.
        let rows = splice_native_rows(&trace, &[], &[]);
        assert_eq!(
            names(&rows),
            ["main", "compiled", "upcalled", "b"],
            "no native: the trace"
        );
    }

    /// Wave 42: a row lives exactly as long as its guard, and nests; a JNI
    /// arm's row is its function pointer, untouched (no hash on the call
    /// path); a report's row for the native already innermost at this depth
    /// adds none.
    #[test]
    fn a_native_frame_row_is_popped_with_its_guard_and_not_doubled() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(42), "w42-native-rows");
        let c = ClassId::new(9);
        let hash = native_row_method_hash("frames", "()Ljava/lang/String;");
        let ptr: *mut JvmThread = &mut thread;
        {
            // SAFETY: `thread` outlives every row of this block.
            let _jni = unsafe { NativeFrameRow::enter_jni(ptr, c, 0x1000) };
            // SAFETY: as above.
            assert_eq!(
                unsafe { (*ptr).jni_native_frames.clone() },
                vec![(0, c, 0x1000, 0)]
            );
            // SAFETY: as above.
            let _outer = unsafe { NativeFrameRow::enter_hashed(&shared, ptr, c, hash) };
            // SAFETY: as above.
            let _same = unsafe { NativeFrameRow::enter_hashed(&shared, ptr, c, hash) };
            // SAFETY: as above.
            assert_eq!(
                unsafe { (*ptr).jni_native_frames.clone() },
                vec![(0, c, 0x1000, 0), (0, c, hash | ROW_KEY_IS_HASH, 0)],
                "class 9 names no method bound to 0x1000: a row of its own; then none"
            );
            {
                // SAFETY: as above.
                let _inner = unsafe { NativeFrameRow::enter_hashed(&shared, ptr, c, hash ^ 1) };
                // SAFETY: as above.
                assert_eq!(unsafe { (*ptr).jni_native_frames.len() }, 3);
            }
            // SAFETY: as above.
            assert_eq!(unsafe { (*ptr).jni_native_frames.len() }, 2);
        }
        assert!(thread.jni_native_frames.is_empty());
        #[cfg(feature = "experimental-debug")]
        assert_eq!(
            crate::debug::jdwp_method_id("frames", "()Ljava/lang/String;"),
            hash,
            "one key for a row and a JDWP method id"
        );
    }

    /// Interpreter round i2 wave 60, lane L1: a lambda proxy method's row
    /// names the proxy and the forwarding method's index (the functional
    /// method for the SAM's own descriptor), is told apart from a native's
    /// row by its key, goes right below the body's frame, and is popped with
    /// its guard.
    #[test]
    fn a_lambda_proxy_row_names_its_method_and_sits_below_the_body() {
        use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(60), "w60-proxy-rows");
        let proxy = ClassId::new(0x8000_0060);
        let site = LambdaCallSite {
            functional_interface_id: None,
            functional_interface: Arc::from("java/lang/Runnable"),
            sam_method_name: Arc::from("run"),
            sam_descriptor: Arc::from("()V"),
            impl_handle: MethodHandle {
                kind: MethodHandleKind::InvokeStatic,
                class_name: Arc::from("p/Host"),
                member_name: Arc::from("lambda$main$0"),
                descriptor: Arc::from("()V"),
            },
            instantiated_descriptor: Arc::from("()V"),
            capture_types: Vec::new(),
            proxy_class_id: proxy,
            serializable_flag: false,
        };
        let ptr: *mut JvmThread = &mut thread;
        {
            let index = crate::runtime::lambda_proxy::lambda_proxy_forwarding_index(
                &shared, proxy, &site, "()V",
            );
            // SAFETY: `thread` outlives the row.
            let _row = unsafe { NativeFrameRow::enter_lambda_proxy(ptr, proxy, index) };
            // SAFETY: as above.
            let rows = unsafe { (*ptr).jni_native_frames.clone() };
            assert_eq!(rows, vec![(0, proxy, ROW_KEY_IS_PROXY | 1, 0)]);
            assert_eq!(proxy_row_index(rows[0].2), Some(1));
            let trace = vec![
                crate::runtime::stackwalker::synthetic_entry(
                    Arc::from("p/Host"),
                    Arc::from("main"),
                ),
                crate::runtime::stackwalker::synthetic_entry(
                    Arc::from("p/Host"),
                    Arc::from("lambda$main$0"),
                ),
            ];
            let spliced = splice_native_rows(&trace, &[1], &rows);
            assert!(
                matches!(spliced.as_slice(), [FrameRow::Java(0, _), FrameRow::Native(c, _), FrameRow::Java(1, _)] if *c == proxy),
                "between the caller and the body"
            );
        }
        assert!(thread.jni_native_frames.is_empty());
        assert_eq!(
            proxy_row_index(0x1000),
            None,
            "a JNI arm's function pointer"
        );
        assert_eq!(
            proxy_row_index(ROW_KEY_IS_HASH | ROW_KEY_IS_PROXY | 1),
            None,
            "a hashed key"
        );
    }

    /// The i54-L1 retirement: a lambda proxy method's frame records its
    /// locals -- the proxy in slot 0, then the arguments as passed, a
    /// `long`'s second slot marked, every reference pinned (a null one by the
    /// proxy) -- which read back by kind, answer `INVALID_SLOT` past the
    /// frame and `TYPE_MISMATCH` for another kind (MEASURED on HotSpot,
    /// `L1K54JvmtiLambdaProxyLocalsAndFields`), take writes, and go with
    /// their guard, pins included.
    #[test]
    fn a_lambda_proxy_frame_keeps_its_locals() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(61), "k54-proxy-locals");
        let proxy = ClassId::new(0x8000_0061);
        let receiver = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let arg = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let pins_before = thread.native_pin_roots.len();
        let ptr: *mut JvmThread = &mut thread;
        {
            let args = [
                Value::Int(7),
                Value::Long(9),
                Value::Object(Some(arg)),
                Value::Object(None),
            ];
            // SAFETY: `thread` outlives the frame.
            let frame = unsafe { LambdaProxyFrame::enter(ptr, proxy, 1, receiver, &args) };
            // SAFETY: as above; the frame only reads through its pointer.
            let t = unsafe { &mut *ptr };
            let key = t.jni_native_frames[0].2;
            assert_eq!(
                (proxy_row_index(key), proxy_row_locals(key)),
                (Some(1), Some(0))
            );
            let read = |t: &JvmThread, slot: JInt, kind| proxy_local_read(t, 0, slot, kind);
            assert_eq!(
                read(t, 0, LocalKind::Object),
                Ok(Value::Object(Some(receiver)))
            );
            assert_eq!(read(t, 1, LocalKind::Int), Ok(Value::Int(7)));
            assert_eq!(read(t, 2, LocalKind::Long), Ok(Value::Long(9)));
            assert_eq!(
                read(t, 3, LocalKind::Int),
                Err(ERR_TYPE_MISMATCH),
                "a long's second slot"
            );
            assert_eq!(read(t, 4, LocalKind::Object), Ok(Value::Object(Some(arg))));
            assert_eq!(read(t, 5, LocalKind::Object), Ok(Value::Object(None)));
            assert_eq!(read(t, 6, LocalKind::Object), Err(ERR_INVALID_SLOT));
            assert_eq!(read(t, -1, LocalKind::Object), Err(ERR_INVALID_SLOT));
            assert_eq!(read(t, 1, LocalKind::Object), Err(ERR_TYPE_MISMATCH));
            assert_eq!(
                proxy_local_write(t, 0, 5, LocalKind::Object, Value::Object(Some(arg))),
                Ok(())
            );
            assert_eq!(
                proxy_local_write(t, 0, 1, LocalKind::Int, Value::Int(8)),
                Ok(())
            );
            assert_eq!(
                proxy_local_write(t, 0, 2, LocalKind::Int, Value::Int(8)),
                Err(ERR_TYPE_MISMATCH)
            );
            assert_eq!(read(t, 5, LocalKind::Object), Ok(Value::Object(Some(arg))));
            assert_eq!(
                frame.current(),
                Some((
                    receiver,
                    vec![
                        Value::Int(8),
                        Value::Long(9),
                        Value::Object(Some(arg)),
                        Value::Object(Some(arg))
                    ]
                ))
            );
            assert_eq!(
                proxy_frame_values(&t.lambda_proxy_locals, &t.native_pin_roots, key)
                    .map(|v| v.len()),
                Some(6)
            );
        }
        assert!(thread.jni_native_frames.is_empty());
        assert!(thread.lambda_proxy_locals.is_empty());
        assert_eq!(thread.native_pin_roots.len(), pins_before);
    }

    /// Interpreter round i1 wave 44: the slot checks of the local-variable
    /// functions, in HotSpot 25.0.3's order (measured,
    /// `tools/probes/interp/L1/L1W44JvmtiLocalsAndStack.java`): the slot's
    /// range, then whether it holds a reference, then the
    /// `LocalVariableTable`.
    #[test]
    fn a_local_is_checked_as_hotspot_checks_it() {
        use LocalKind::{Int, Long, Object};
        let mut f = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            4,
            6,
            &[],
        );
        f.set_local(0, Value::Int(3));
        f.set_local(1, Value::Long(4));
        f.set_local(3, Value::Object(None));
        // Slots 4 and 5 are never written.
        assert_eq!(checked_local(Declared::Kind(Int), &f, 0, Int), Ok(0));
        assert_eq!(checked_local(Declared::NoTable, &f, 1, Long), Ok(1));
        assert_eq!(checked_local(Declared::Kind(Object), &f, 3, Object), Ok(3));
        assert_eq!(
            checked_local(Declared::Kind(Int), &f, 0, Long),
            Err(ERR_TYPE_MISMATCH),
            "declared another kind"
        );
        assert_eq!(
            checked_local(Declared::Kind(Int), &f, 0, Object),
            Err(ERR_TYPE_MISMATCH),
            "no reference there"
        );
        assert_eq!(
            checked_local(Declared::Kind(Object), &f, 3, Int),
            Err(ERR_TYPE_MISMATCH),
            "a reference there"
        );
        assert_eq!(
            checked_local(Declared::NotLive, &f, 3, Object),
            Err(ERR_INVALID_SLOT),
            "a reference no entry covers (javac's lock copy)"
        );
        assert_eq!(
            checked_local(Declared::NotLive, &f, 4, Object),
            Err(ERR_TYPE_MISMATCH),
            "an Object read of a variable not assigned yet"
        );
        assert_eq!(
            checked_local(Declared::NotLive, &f, 4, Int),
            Err(ERR_INVALID_SLOT),
            "an int read of a variable not assigned yet"
        );
        assert_eq!(
            checked_local(Declared::NoTable, &f, 5, Long),
            Err(ERR_INVALID_SLOT),
            "the second slot is past the locals"
        );
        assert_eq!(
            checked_local(Declared::NoTable, &f, 99, Int),
            Err(ERR_INVALID_SLOT)
        );
        assert_eq!(
            checked_local(Declared::NoTable, &f, -1, Int),
            Err(ERR_INVALID_SLOT)
        );
        assert_eq!(
            std::mem::size_of::<MonitorDepthRow>(),
            16,
            "jvmtiMonitorStackDepthInfo"
        );
    }

    /// Interpreter round i1 wave 52, lane L1
    /// (`interpreter-L1-the-c-jvmti-table-lacks-the-functions-and-events-agents-call-first-FIXED-20261007`):
    /// the phase, the env's local storage, a raw monitor's reentrant enter,
    /// timed wait and owner checks, and the class, field and method facts of
    /// a fabricated class through the C table. A static field's id carries
    /// the static mark, so it does not name the instance field of the same
    /// slot number.
    #[test]
    fn the_wave_52_facts_answer_through_the_c_table() {
        use cratonvm_reader::class_access_flags::FieldAccessFlags;
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        let vm = shared.vm_identity;
        jni::set_jni_context_arc(Arc::clone(&shared));
        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        // SAFETY (every transmute below): the slot's `jvmti.h` signature.
        let phase: extern "C" fn(*mut c_void, *mut JInt) -> JInt =
            unsafe { std::mem::transmute(function(env, 133)) };
        let set_storage: extern "C" fn(*mut c_void, usize) -> JInt =
            unsafe { std::mem::transmute(function(env, 148)) };
        let get_storage: extern "C" fn(*mut c_void, *mut usize) -> JInt =
            unsafe { std::mem::transmute(function(env, 147)) };
        let create: extern "C" fn(*mut c_void, *const u8, *mut usize) -> JInt =
            unsafe { std::mem::transmute(function(env, 31)) };
        type MonitorOp = extern "C" fn(*mut c_void, usize) -> JInt;
        let destroy: MonitorOp = unsafe { std::mem::transmute(function(env, 32)) };
        let enter: MonitorOp = unsafe { std::mem::transmute(function(env, 33)) };
        let exit: MonitorOp = unsafe { std::mem::transmute(function(env, 34)) };
        let notify: MonitorOp = unsafe { std::mem::transmute(function(env, 36)) };
        let notify_all: MonitorOp = unsafe { std::mem::transmute(function(env, 37)) };
        let wait: extern "C" fn(*mut c_void, usize, i64) -> JInt =
            unsafe { std::mem::transmute(function(env, 35)) };
        let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
            unsafe { std::mem::transmute(function(env, 142)) };
        let modifiers: extern "C" fn(*mut c_void, JObject, *mut JInt) -> JInt =
            unsafe { std::mem::transmute(function(env, 51)) };
        type ClassBool = extern "C" fn(*mut c_void, JObject, *mut u8) -> JInt;
        let is_interface: ClassBool = unsafe { std::mem::transmute(function(env, 55)) };
        let is_array: ClassBool = unsafe { std::mem::transmute(function(env, 56)) };
        let source: extern "C" fn(*mut c_void, JObject, *mut *mut u8) -> JInt =
            unsafe { std::mem::transmute(function(env, 50)) };
        let fields: extern "C" fn(*mut c_void, JObject, *mut JInt, *mut *mut u64) -> JInt =
            unsafe { std::mem::transmute(function(env, 53)) };
        let field_name: extern "C" fn(
            *mut c_void,
            JObject,
            u64,
            *mut *mut u8,
            *mut *mut u8,
            *mut *mut u8,
        ) -> JInt = unsafe { std::mem::transmute(function(env, 60)) };
        let field_mods: extern "C" fn(*mut c_void, JObject, u64, *mut JInt) -> JInt =
            unsafe { std::mem::transmute(function(env, 62)) };
        let field_synthetic: extern "C" fn(*mut c_void, JObject, u64, *mut u8) -> JInt =
            unsafe { std::mem::transmute(function(env, 63)) };
        type MethodJint = extern "C" fn(*mut c_void, u64, *mut JInt) -> JInt;
        let max_locals: MethodJint = unsafe { std::mem::transmute(function(env, 68)) };
        let args_size: MethodJint = unsafe { std::mem::transmute(function(env, 69)) };
        let version: extern "C" fn(*mut c_void, JObject, *mut JInt, *mut JInt) -> JInt =
            unsafe { std::mem::transmute(function(env, 145)) };
        let dealloc: extern "C" fn(*mut c_void, *mut u8) -> JInt =
            unsafe { std::mem::transmute(function(env, 47)) };
        let dispose: extern "C" fn(*mut c_void) -> JInt =
            unsafe { std::mem::transmute(function(env, 127)) };
        let text = |block: *mut u8| -> Option<String> {
            if block.is_null() {
                return None;
            }
            // SAFETY: a NUL-terminated block the table allocated.
            let s = unsafe { std::ffi::CStr::from_ptr(block.cast()) }
                .to_string_lossy()
                .into_owned();
            assert_eq!(dealloc(env, block), ERR_NONE);
            Some(s)
        };

        let mut p = 0;
        assert_eq!(phase(env, &mut p), ERR_NONE);
        assert_eq!(p, JInt::from(crate::debug::JVMTI_PHASE_LIVE));
        let mut storage = 1usize;
        assert_eq!(get_storage(env, &mut storage), ERR_NONE);
        assert_eq!(storage, 0, "nothing stored yet");
        assert_eq!(set_storage(env, 0x5200), ERR_NONE);
        assert_eq!(get_storage(env, &mut storage), ERR_NONE);
        assert_eq!(storage, 0x5200);

        let mut monitor = 0usize;
        assert_eq!(create(env, b"w52\0".as_ptr(), &mut monitor), ERR_NONE);
        assert_eq!(exit(env, monitor), ERR_NOT_MONITOR_OWNER);
        assert_eq!(notify(env, monitor), ERR_NOT_MONITOR_OWNER);
        assert_eq!(wait(env, monitor, 1), ERR_NOT_MONITOR_OWNER);
        assert_eq!(enter(env, monitor), ERR_NONE);
        assert_eq!(enter(env, monitor), ERR_NONE, "reentrant");
        assert_eq!(wait(env, monitor, 1), ERR_NONE, "timed out, owned again");
        assert_eq!(notify(env, monitor), ERR_NONE);
        assert_eq!(notify_all(env, monitor), ERR_NONE);
        assert_eq!(exit(env, monitor), ERR_NONE);
        assert_eq!(exit(env, monitor), ERR_NONE, "the wait kept both entries");
        assert_eq!(exit(env, monitor), ERR_NOT_MONITOR_OWNER);
        // Another thread takes it once this one lets it go.
        assert_eq!(enter(env, monitor), ERR_NONE);
        let env_addr = env as usize; // Cast: the env's address, for the other thread
        let other = std::thread::spawn(move || {
            let env = env_addr as *mut c_void; // Cast: the address back
            (enter(env, monitor), exit(env, monitor))
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(exit(env, monitor), ERR_NONE);
        assert_eq!(other.join().expect("joined"), (ERR_NONE, ERR_NONE));
        assert_eq!(destroy(env, monitor), ERR_NONE);
        assert_eq!(enter(env, 0), ERR_INVALID_MONITOR);

        let cid = loop_class(&shared, "cratonvm/test/W52Facts");
        {
            let mut cm = shared.classes.class_manager.write();
            let class = cm.get_class_mut(cid).expect("fabricated");
            let field = |flags: FieldAccessFlags, name: &str, descriptor: &str| {
                cratonvm_reader::field::ClassFileField {
                    access_flags: flags,
                    name: Arc::from(name),
                    descriptor: Arc::from(descriptor),
                    attributes: Vec::new(),
                }
            };
            class.fields = vec![
                field(
                    FieldAccessFlags::PUBLIC | FieldAccessFlags::STATIC | FieldAccessFlags::FINAL,
                    "LABEL",
                    "Ljava/lang/String;",
                ),
                field(FieldAccessFlags::PRIVATE, "count", "I"),
                field(FieldAccessFlags::STATIC, "created", "I"),
            ];
            class.fields[1]
                .attributes
                .push(LazyAttribute::Decoded(Attribute::Synthetic));
            class.first_field_index = 0;
            class.source_file = Some("W52Facts.java".to_string());
            class.state = crate::classloading::ClassState::Loaded;
        }
        let klass = jni::class_id_to_jclass(cid);
        let mut answer: JInt = -1;
        assert_eq!(modifiers(env, klass, &mut answer), ERR_NONE);
        let mut flag = 2u8;
        assert_eq!(is_interface(env, klass, &mut flag), ERR_NONE);
        assert_eq!(flag, 0, "not an interface");
        assert_eq!(is_array(env, klass, &mut flag), ERR_NONE);
        assert_eq!(flag, 0, "not an array");
        assert_eq!(is_array(env, 0, &mut flag), ERR_INVALID_CLASS);
        let mut src = std::ptr::null_mut::<u8>();
        assert_eq!(
            source(env, klass, &mut src),
            ERR_MUST_POSSESS_CAPABILITY,
            "can_get_source_file_name first"
        );
        let wanted: Caps = [
            CAN_GET_SOURCE_FILE_NAME | CAN_GET_SYNTHETIC_ATTRIBUTE,
            0,
            0,
            0,
        ];
        assert_eq!(add(env, &wanted), ERR_NONE);
        assert_eq!(source(env, klass, &mut src), ERR_NONE);
        assert_eq!(text(src).as_deref(), Some("W52Facts.java"));
        let (mut minor, mut major) = (-1, -1);
        assert_eq!(version(env, klass, &mut minor, &mut major), ERR_NONE);

        let mut count: JInt = -1;
        let mut ids = std::ptr::null_mut::<u64>();
        assert_eq!(
            fields(env, klass, &mut count, &mut ids),
            ERR_CLASS_NOT_PREPARED,
            "loaded, not linked: HotSpot's answer"
        );
        shared
            .classes
            .class_manager
            .write()
            .get_class_mut(cid)
            .expect("fabricated")
            .state = crate::classloading::ClassState::Prepared;
        assert_eq!(fields(env, klass, &mut count, &mut ids), ERR_NONE);
        assert_eq!(count, 3);
        // SAFETY: the three ids the table allocated.
        let list = unsafe { std::slice::from_raw_parts(ids, 3) }.to_vec();
        assert_eq!(dealloc(env, ids.cast()), ERR_NONE);
        let class_half = u64::from(cid.as_u32()) << 32;
        assert_eq!(
            list,
            vec![
                STATIC_FIELD_ID | class_half,
                class_half,
                STATIC_FIELD_ID | class_half | 1
            ],
            "class-file order; the statics marked"
        );
        let named: Vec<(Option<String>, Option<String>, JInt, u8)> = list
            .iter()
            .map(|&id| {
                let (mut n, mut s) = (std::ptr::null_mut(), std::ptr::null_mut());
                assert_eq!(
                    field_name(env, klass, id, &mut n, &mut s, std::ptr::null_mut()),
                    ERR_NONE
                );
                let mut mods = -1;
                assert_eq!(field_mods(env, klass, id, &mut mods), ERR_NONE);
                let mut synthetic = 2u8;
                assert_eq!(field_synthetic(env, klass, id, &mut synthetic), ERR_NONE);
                (text(n), text(s), mods, synthetic)
            })
            .collect();
        assert_eq!(
            named,
            vec![
                (
                    Some("LABEL".into()),
                    Some("Ljava/lang/String;".into()),
                    0x19,
                    0
                ),
                (Some("count".into()), Some("I".into()), 0x2, 1),
                (Some("created".into()), Some("I".into()), 0x8, 0),
            ]
        );
        let mut n = std::ptr::null_mut();
        assert_eq!(
            field_name(
                env,
                klass,
                class_half | 7,
                &mut n,
                std::ptr::null_mut(),
                std::ptr::null_mut()
            ),
            ERR_INVALID_FIELDID
        );

        // Widening: the class id's half of the jmethodID.
        let jmid = u64::from(cid.as_u32()) << 32;
        let mut slots = -1;
        assert_eq!(max_locals(env, jmid, &mut slots), ERR_NONE);
        assert_eq!(slots, 3, "max_locals");
        assert_eq!(args_size(env, jmid, &mut slots), ERR_NONE);
        assert_eq!(slots, 0, "a static ()I takes no argument slot");

        assert_eq!(dispose(env), ERR_NONE);
        assert_eq!(phase(env, &mut p), ERR_INVALID_ENVIRONMENT);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }

    /// The `'static` tally a test leaks and hands its callback through the
    /// env's local storage (slot 148), so the tests declare no static (the
    /// per-VM statics ratchet counts test modules too).
    fn leak_into_env_storage<T>(env: *mut c_void, value: T) -> &'static T {
        let leaked: &'static T = Box::leak(Box::new(value));
        assert_eq!(
            set_environment_local_storage(env.cast(), std::ptr::from_ref(leaked) as usize),
            ERR_NONE
        );
        leaked
    }

    /// What [`leak_into_env_storage`] stored; `None` before any.
    fn env_storage<T>(env: *mut JvmtiNativeEnv) -> Option<&'static T> {
        let mut data = 0usize;
        if get_environment_local_storage(env, &mut data) != ERR_NONE || data == 0 {
            return None;
        }
        // SAFETY: a leaked `T` the test stored with `leak_into_env_storage`.
        Some(unsafe { &*(data as *const T) })
    }

    // No assertion in here: a panic cannot leave an `extern "C"` function.
    unsafe extern "C" fn on_thread_event(env: *mut JvmtiNativeEnv, _jni: JNIEnv, _thread: JObject) {
        if let Some(events) = env_storage::<AtomicUsize>(env) {
            events.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Interpreter round i1 wave 52, lane L1: `ThreadStart` / `ThreadEnd`
    /// through the C table. Enabling either raises the VM's gate
    /// (`DebuggerGates::native_thread_events_armed`), the thread's post
    /// reaches the agent's member 2 / 3, and disabling both lowers the gate.
    #[test]
    fn a_c_env_gets_thread_start_and_end() {
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        let vm = shared.vm_identity;
        jni::set_jni_context_arc(Arc::clone(&shared));
        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        let events = leak_into_env_storage(env, AtomicUsize::new(0));
        // SAFETY (every transmute below): the slot's `jvmti.h` signature.
        let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
            unsafe { std::mem::transmute(function(env, 2)) };
        let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
            unsafe { std::mem::transmute(function(env, 122)) };
        let dispose: extern "C" fn(*mut c_void) -> JInt =
            unsafe { std::mem::transmute(function(env, 127)) };
        let callback = on_thread_event as *const () as usize;
        let callbacks =
            callbacks_with(&[(EVENT_THREAD_START, callback), (EVENT_THREAD_END, callback)]);
        let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
        assert_eq!(
            set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
            ERR_NONE
        );
        let gates = &shared.debug.debugger_gates;
        assert!(!gates.native_thread_events_armed());
        post_thread_event(&shared, 7, true);
        assert_eq!(events.load(Ordering::SeqCst), 0, "not enabled");
        assert_eq!(set_mode(env, 1, EVENT_THREAD_START, 0), ERR_NONE);
        assert!(gates.native_thread_events_armed());
        post_thread_event(&shared, 7, true);
        post_thread_event(&shared, 7, false);
        assert_eq!(events.load(Ordering::SeqCst), 1, "ThreadEnd is not enabled");
        assert_eq!(set_mode(env, 1, EVENT_THREAD_END, 0), ERR_NONE);
        post_thread_event(&shared, 7, false);
        assert_eq!(events.load(Ordering::SeqCst), 2);
        assert_eq!(set_mode(env, 0, EVENT_THREAD_START, 0), ERR_NONE);
        assert!(
            gates.native_thread_events_armed(),
            "ThreadEnd still listens"
        );
        assert_eq!(set_mode(env, 0, EVENT_THREAD_END, 0), ERR_NONE);
        assert!(!gates.native_thread_events_armed());
        assert_eq!(dispose(env), ERR_NONE);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }

    // gci i12/ev: the JNI binding the callback runs under (0: none). No
    // assertion in here: a panic cannot leave an `extern "C"` function.
    unsafe extern "C" fn on_thread_event_binding(
        env: *mut JvmtiNativeEnv,
        _jni: JNIEnv,
        _thread: JObject,
    ) {
        if let Some(seen) = env_storage::<AtomicUsize>(env) {
            seen.store(
                jni::installed_jni_thread_addr().unwrap_or(0),
                Ordering::SeqCst,
            );
        }
    }

    /// gci i12/ev (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
    /// item 1c): a `ThreadStart` / `ThreadEnd` posted the way the thread
    /// start path now posts it (under `post_on_own_thread`) reaches the
    /// agent's callback with the poster's own handle installed, and the
    /// binding is put back afterwards; posted bare, the callback runs with no
    /// handle of an unpublished thread (the registry has none for it).
    #[test]
    fn gci_i12ev_a_thread_event_callback_runs_on_the_posters_handle() {
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        let vm = shared.vm_identity;
        jni::set_jni_context_arc(Arc::clone(&shared));
        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        let seen = leak_into_env_storage(env, AtomicUsize::new(usize::MAX));
        // SAFETY (every transmute below): the slot's `jvmti.h` signature.
        let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
            unsafe { std::mem::transmute(function(env, 2)) };
        let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
            unsafe { std::mem::transmute(function(env, 122)) };
        let dispose: extern "C" fn(*mut c_void) -> JInt =
            unsafe { std::mem::transmute(function(env, 127)) };
        let callback = on_thread_event_binding as *const () as usize;
        let callbacks =
            callbacks_with(&[(EVENT_THREAD_START, callback), (EVENT_THREAD_END, callback)]);
        let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
        assert_eq!(
            set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
            ERR_NONE
        );
        assert_eq!(set_mode(env, 1, EVENT_THREAD_START, 0), ERR_NONE);
        assert_eq!(set_mode(env, 1, EVENT_THREAD_END, 0), ERR_NONE);
        let before = jni::installed_jni_thread_addr();
        let mut poster = JvmThread::new(ThreadId(7), "gci-i12ev-poster");
        let own = &mut poster as *mut JvmThread as usize;
        for start in [true, false] {
            seen.store(usize::MAX, Ordering::SeqCst);
            crate::runtime::interpreter::post_on_own_thread(&mut poster, || {
                post_thread_event(&shared, 7, start)
            });
            assert_eq!(
                seen.load(Ordering::SeqCst),
                own,
                "start={start}: the poster's handle"
            );
            assert_eq!(
                jni::installed_jni_thread_addr(),
                before,
                "start={start}: put back"
            );
        }
        if before.is_none() {
            seen.store(usize::MAX, Ordering::SeqCst);
            post_thread_event(&shared, 7, true);
            assert_eq!(seen.load(Ordering::SeqCst), 0, "bare: no handle installed");
        }
        assert_eq!(set_mode(env, 0, EVENT_THREAD_START, 0), ERR_NONE);
        assert_eq!(set_mode(env, 0, EVENT_THREAD_END, 0), ERR_NONE);
        assert_eq!(dispose(env), ERR_NONE);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }

    /// The `(method, location, class id)` rows `on_exception_catch` saw.
    type Catches = std::sync::Mutex<Vec<(u64, i64, u64)>>;

    // No assertion in here: a panic cannot leave an `extern "C"` function.
    unsafe extern "C" fn on_exception_catch(
        env: *mut JvmtiNativeEnv,
        _jni_env: JNIEnv,
        _thread: JObject,
        method: u64,
        location: i64,
        exception: JObject,
    ) {
        let class = jni::jobject_to_obj(exception)
            .zip(jni::calling_thread_vm())
            .map_or(0, |(obj, vm)| {
                u64::from(vm.mem.heap.class_id_of(obj).as_u32())
            });
        if let Some(Ok(mut seen)) = env_storage::<Catches>(env).map(std::sync::Mutex::lock) {
            seen.push((method, location, class));
        }
    }

    /// Interpreter round i1 wave 52, lane L1 (the last item of
    /// `interpreter-L1-proposal-c-jvmti-table-interpreter-events-through-the-delivery-env-FIXED-20261016`):
    /// `ExceptionCatch` through the C table — its capability is
    /// `can_generate_exception_events`, the event (member 9 of
    /// `jvmtiEventCallbacks`) reaches the agent with the handler's method and
    /// location and the caught object as a local reference, and a disable
    /// takes it off the VM's manager. With `CRATONVM_JVMTI_C_EXCEPTION_CATCH`
    /// off an enable answers `JVMTI_ERROR_NOT_AVAILABLE`, as before.
    #[test]
    fn a_c_env_gets_exception_catch_with_the_caught_object() {
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        let vm = shared.vm_identity;
        jni::set_jni_context_arc(Arc::clone(&shared));
        let cid = loop_class(&shared, "cratonvm/test/W52Catch");
        // Widening: the class id's half of the jmethodID.
        let jmid = u64::from(cid.as_u32()) << 32;
        let caught = shared.mem.heap.alloc_object(cid, 0);
        let pin = shared.natives.jni_global_refs.lock().add(caught);
        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        let catches = leak_into_env_storage(env, Catches::default());
        // SAFETY (every transmute below): the slot's `jvmti.h` signature.
        let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
            unsafe { std::mem::transmute(function(env, 142)) };
        let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
            unsafe { std::mem::transmute(function(env, 2)) };
        let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
            unsafe { std::mem::transmute(function(env, 122)) };
        let dispose: extern "C" fn(*mut c_void) -> JInt =
            unsafe { std::mem::transmute(function(env, 127)) };
        let callbacks = callbacks_with(&[(
            EVENT_EXCEPTION_CATCH,
            on_exception_catch as *const () as usize,
        )]);
        let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
        assert_eq!(
            set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
            ERR_NONE
        );
        if !shared.debug.debugger_gates.c_exception_catch() {
            assert_eq!(add(env, &POTENTIAL), ERR_NONE);
            assert_eq!(
                set_mode(env, 1, EVENT_EXCEPTION_CATCH, 0),
                ERR_NOT_AVAILABLE
            );
            assert_eq!(dispose(env), ERR_NONE);
            let _ = shared.natives.jni_global_refs.lock().remove(pin);
            rt::forget_vm_jvmti_state(vm);
            jni::clear_jni_context();
            return;
        }
        assert_eq!(
            set_mode(env, 1, EVENT_EXCEPTION_CATCH, 0),
            ERR_MUST_POSSESS_CAPABILITY
        );
        let exceptions: Caps = [CAN_GENERATE_EXCEPTION_EVENTS, 0, 0, 0];
        assert_eq!(add(env, &exceptions), ERR_NONE);
        assert!(!rt::exception_catch_enabled_for_vm(vm, 5));
        assert_eq!(set_mode(env, 1, EVENT_EXCEPTION_CATCH, 0), ERR_NONE);
        assert!(
            rt::exception_catch_enabled_for_vm(vm, 5),
            "the delivery env raised the manager's ExceptionCatch"
        );

        // Cast: the object's address, as the catch hook passes it
        let address = caught.as_ptr() as usize as u64;
        rt::fire_exception_catch_for_vm(vm, 5, jmid, 22, address);
        assert_eq!(
            *catches.lock().expect("catches"),
            vec![(jmid, 22, u64::from(cid.as_u32()))],
            "the handler's method and location, and the caught object"
        );

        assert_eq!(set_mode(env, 0, EVENT_EXCEPTION_CATCH, 0), ERR_NONE);
        assert!(
            !rt::exception_catch_enabled_for_vm(vm, 5),
            "a disable reaches the VM"
        );
        rt::fire_exception_catch_for_vm(vm, 5, jmid, 22, address);
        assert_eq!(catches.lock().expect("catches").len(), 1);
        assert_eq!(dispose(env), ERR_NONE);
        let _ = shared.natives.jni_global_refs.lock().remove(pin);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }

    /// The `(name, class data length, loader is NULL)` rows
    /// `on_class_file_load_hook` saw.
    type Hooked = std::sync::Mutex<Vec<(String, usize, bool)>>;

    // No assertion in here: a panic cannot leave an `extern "C"` function.
    #[allow(clippy::too_many_arguments)]
    unsafe extern "C" fn on_class_file_load_hook(
        env: *mut JvmtiNativeEnv,
        _jni_env: JNIEnv,
        _class_being_redefined: JObject,
        loader: JObject,
        name: *const u8,
        _domain: JObject,
        len: JInt,
        data: *const u8,
        new_len: *mut JInt,
        new_data: *mut *mut u8,
    ) {
        let name = std::ffi::CStr::from_ptr(name.cast())
            .to_string_lossy()
            .into_owned();
        let Ok(size) = usize::try_from(len) else {
            return;
        };
        if name == "w53/Patched" {
            let mut copy = std::ptr::null_mut::<u8>();
            if allocate(env, i64::from(len), &mut copy) == ERR_NONE && !copy.is_null() {
                std::ptr::copy_nonoverlapping(data, copy, size);
                *copy.add(size - 1) = b'!';
                *new_len = len;
                *new_data = copy;
            }
        }
        if let Some(Ok(mut seen)) = env_storage::<Hooked>(env).map(std::sync::Mutex::lock) {
            seen.push((name, size, loader == 0));
        }
    }

    /// Interpreter round i2 wave 53, lane L1: `ClassFileLoadHook` through the
    /// C table. Enabling it (no capability) raises the VM's gate
    /// (`DebuggerGates::native_class_file_load_hook_armed`), which the
    /// transformer chain's walk asks through `class_file_load_hook_vm`; the
    /// callback (member 4) gets the name and the class file, and its
    /// replacement, an `Allocate` block, is the answer. A class it declines
    /// answers `None`; a disable lowers the gate. With
    /// `CRATONVM_JVMTI_C_CLASS_HOOKS` off an enable answers
    /// `JVMTI_ERROR_NOT_AVAILABLE`, as before.
    #[test]
    fn a_c_class_file_load_hook_rewrites_the_class_file() {
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        let vm = shared.vm_identity;
        jni::set_jni_context_arc(Arc::clone(&shared));
        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        let hooked = leak_into_env_storage(env, Hooked::default());
        // SAFETY (every transmute below): the slot's `jvmti.h` signature.
        let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
            unsafe { std::mem::transmute(function(env, 2)) };
        let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
            unsafe { std::mem::transmute(function(env, 122)) };
        let dispose: extern "C" fn(*mut c_void) -> JInt =
            unsafe { std::mem::transmute(function(env, 127)) };
        let callbacks = callbacks_with(&[(
            EVENT_CLASS_FILE_LOAD_HOOK,
            on_class_file_load_hook as *const () as usize,
        )]);
        let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
        assert_eq!(
            set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
            ERR_NONE
        );
        let gates = &shared.debug.debugger_gates;
        if !gates.c_class_hooks() {
            assert_eq!(
                set_mode(env, 1, EVENT_CLASS_FILE_LOAD_HOOK, 0),
                ERR_NOT_AVAILABLE
            );
            assert!(!gates.native_class_file_load_hook_armed());
            assert_eq!(dispose(env), ERR_NONE);
            rt::forget_vm_jvmti_state(vm);
            jni::clear_jni_context();
            return;
        }
        assert!(class_file_load_hook_vm(vm).is_none(), "not enabled");
        assert_eq!(set_mode(env, 1, EVENT_CLASS_FILE_LOAD_HOOK, 0), ERR_NONE);
        assert!(gates.native_class_file_load_hook_armed());
        assert!(class_file_load_hook_vm(vm).is_some());

        let bytes = b"\xCA\xFE\xBA\xBE-w53";
        let rewritten =
            post_class_file_load_hook(&shared, 0, "w53/Patched", [None; 3], bytes, false);
        assert_eq!(rewritten.as_deref(), Some(&b"\xCA\xFE\xBA\xBE-w5!"[..]));
        let declined = post_class_file_load_hook(&shared, 0, "w53/Other", [None; 3], bytes, false);
        assert_eq!(declined, None, "a declined class file is not replaced");
        // Wave 54: an env without `can_retransform_classes` is not in the
        // retransform-capable group.
        let other_group =
            post_class_file_load_hook(&shared, 0, "w53/Patched", [None; 3], bytes, true);
        assert_eq!(other_group, None);
        assert_eq!(
            *hooked.lock().expect("hooked"),
            vec![
                ("w53/Patched".to_string(), bytes.len(), true),
                ("w53/Other".to_string(), bytes.len(), true),
            ]
        );

        assert_eq!(set_mode(env, 0, EVENT_CLASS_FILE_LOAD_HOOK, 0), ERR_NONE);
        assert!(!gates.native_class_file_load_hook_armed());
        assert!(class_file_load_hook_vm(vm).is_none());
        assert_eq!(dispose(env), ERR_NONE);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }

    /// Interpreter round i2 wave 54, lane L1: `can_retransform_classes` and
    /// `RetransformClasses` (152). While `CRATONVM_JVMTI_C_RETRANSFORM` is on
    /// the capability is potential, an env holding it is in the
    /// retransform-capable group of `ClassFileLoadHook` (and only there), and
    /// `RetransformClasses` checks its arguments in HotSpot's order; off, the
    /// capability is not potential and the call answers
    /// `JVMTI_ERROR_MUST_POSSESS_CAPABILITY`.
    #[test]
    fn a_c_env_with_can_retransform_classes_takes_the_retransform_group() {
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        let vm = shared.vm_identity;
        jni::set_jni_context_arc(Arc::clone(&shared));
        let cid = loop_class(&shared, "cratonvm/test/W54Retransformed");
        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        let hooked = leak_into_env_storage(env, Hooked::default());
        // SAFETY (every transmute below): the slot's `jvmti.h` signature.
        let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
            unsafe { std::mem::transmute(function(env, 2)) };
        let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
            unsafe { std::mem::transmute(function(env, 122)) };
        let potential: extern "C" fn(*mut c_void, *mut [u32; CAPABILITY_WORDS]) -> JInt =
            unsafe { std::mem::transmute(function(env, 140)) };
        let add: extern "C" fn(*mut c_void, *const [u32; CAPABILITY_WORDS]) -> JInt =
            unsafe { std::mem::transmute(function(env, 142)) };
        let retransform: extern "C" fn(*mut c_void, JInt, *const JObject) -> JInt =
            unsafe { std::mem::transmute(function(env, 152)) };
        let dispose: extern "C" fn(*mut c_void) -> JInt =
            unsafe { std::mem::transmute(function(env, 127)) };
        let klass = jni::class_id_to_jclass(cid);
        assert_eq!(
            retransform(env, 1, &klass),
            ERR_MUST_POSSESS_CAPABILITY,
            "not held yet"
        );
        let mut caps = [0u32; CAPABILITY_WORDS];
        assert_eq!(potential(env, &mut caps), ERR_NONE);
        let wanted = [0, CAN_RETRANSFORM_CLASSES, 0, 0];
        let gates = &shared.debug.debugger_gates;
        if !gates.c_retransform() {
            assert_eq!(caps[1] & CAN_RETRANSFORM_CLASSES, 0);
            assert_eq!(add(env, &wanted), ERR_NOT_AVAILABLE);
            assert_eq!(dispose(env), ERR_NONE);
            rt::forget_vm_jvmti_state(vm);
            jni::clear_jni_context();
            return;
        }
        assert_ne!(
            caps[1] & CAN_RETRANSFORM_CLASSES,
            0,
            "potential while the switch is on"
        );
        assert_eq!(add(env, &wanted), ERR_NONE);
        assert_eq!(retransform(env, -1, &klass), ERR_ILLEGAL_ARGUMENT);
        assert_eq!(retransform(env, 1, std::ptr::null()), ERR_NULL_POINTER);
        let bogus: JObject = 0;
        assert_eq!(retransform(env, 1, &bogus), ERR_INVALID_CLASS);
        if gates.c_class_hooks() {
            let callbacks = callbacks_with(&[(
                EVENT_CLASS_FILE_LOAD_HOOK,
                on_class_file_load_hook as *const () as usize,
            )]);
            let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
            assert_eq!(
                set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
                ERR_NONE
            );
            assert_eq!(set_mode(env, 1, EVENT_CLASS_FILE_LOAD_HOOK, 0), ERR_NONE);
            let bytes = b"\xCA\xFE\xBA\xBE-w54";
            let first =
                post_class_file_load_hook(&shared, 0, "w53/Patched", [None; 3], bytes, false);
            assert_eq!(first, None, "not in the non-retransformable group");
            let second =
                post_class_file_load_hook(&shared, 0, "w53/Patched", [None; 3], bytes, true);
            assert_eq!(second.as_deref(), Some(&b"\xCA\xFE\xBA\xBE-w5!"[..]));
            assert_eq!(hooked.lock().expect("hooked").len(), 1, "offered once");
            assert_eq!(set_mode(env, 0, EVENT_CLASS_FILE_LOAD_HOOK, 0), ERR_NONE);
        }
        assert_eq!(dispose(env), ERR_NONE);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }

    // No assertion in here: a panic cannot leave an `extern "C"` function.
    unsafe extern "C" fn on_class_load(
        env: *mut JvmtiNativeEnv,
        _jni: JNIEnv,
        _thread: JObject,
        klass: JObject,
    ) {
        if let Some(loaded) = env_storage::<AtomicU64>(env) {
            let id =
                jni::jclass_class_id_exact(klass).map_or(u64::MAX, |id| u64::from(id.as_u32()));
            loaded.store(id, Ordering::SeqCst);
        }
    }

    /// Interpreter round i2 wave 53, lane L1: `ClassLoad` through the C table
    /// (member 5, `ClassPrepare`'s signature), behind its own gate
    /// (`DebuggerGates::native_class_load_armed`).
    #[test]
    fn a_c_env_gets_class_load() {
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        let vm = shared.vm_identity;
        jni::set_jni_context_arc(Arc::clone(&shared));
        let cid = loop_class(&shared, "cratonvm/test/W53Loaded");
        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        let loaded = leak_into_env_storage(env, AtomicU64::new(0));
        // SAFETY (every transmute below): the slot's `jvmti.h` signature.
        let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
            unsafe { std::mem::transmute(function(env, 2)) };
        let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
            unsafe { std::mem::transmute(function(env, 122)) };
        let dispose: extern "C" fn(*mut c_void) -> JInt =
            unsafe { std::mem::transmute(function(env, 127)) };
        let callbacks = callbacks_with(&[(EVENT_CLASS_LOAD, on_class_load as *const () as usize)]);
        let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
        assert_eq!(
            set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
            ERR_NONE
        );
        let gates = &shared.debug.debugger_gates;
        if !gates.c_class_hooks() {
            assert_eq!(set_mode(env, 1, EVENT_CLASS_LOAD, 0), ERR_NOT_AVAILABLE);
            assert_eq!(dispose(env), ERR_NONE);
            rt::forget_vm_jvmti_state(vm);
            jni::clear_jni_context();
            return;
        }
        post_class_load(&shared, 0, cid);
        assert_eq!(loaded.load(Ordering::SeqCst), 0, "not enabled");
        assert_eq!(set_mode(env, 1, EVENT_CLASS_LOAD, 0), ERR_NONE);
        assert!(gates.native_class_load_armed());
        post_class_load(&shared, 0, cid);
        assert_eq!(loaded.load(Ordering::SeqCst), u64::from(cid.as_u32()));
        assert_eq!(set_mode(env, 0, EVENT_CLASS_LOAD, 0), ERR_NONE);
        assert!(!gates.native_class_load_armed());
        assert_eq!(dispose(env), ERR_NONE);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }

    /// gci i12/ev (`CRATONVM_JVMTI_DEFER_UNOWNED_CLASS_LOAD`): the decisions.
    /// A `ClassLoad` waits only on a published thread whose own handle is not
    /// installed, and only with the switch on (asked last); the queue hands
    /// each thread its own entries in order.
    #[test]
    fn gci_i12ev_a_class_load_waits_only_without_the_threads_own_handle() {
        let asked = std::cell::Cell::new(0);
        let on = || {
            asked.set(asked.get() + 1);
            true
        };
        assert!(own_handle_installed(Some(0x1000), Some(0x1000)));
        assert!(!own_handle_installed(Some(0x1000), Some(0x2000)));
        assert!(!own_handle_installed(Some(0x1000), None));
        assert!(!own_handle_installed(None, None));
        assert!(
            !class_load_deferred(Some(0x1000), Some(0x1000), on),
            "own handle: now"
        );
        assert!(
            !class_load_deferred(None, None, on),
            "unpublished thread: now"
        );
        assert_eq!(asked.get(), 0, "the switch is asked last");
        assert!(class_load_deferred(Some(0x1000), None, on));
        assert!(class_load_deferred(Some(0x1000), Some(0x2000), on));
        assert!(
            !class_load_deferred(Some(0x1000), None, || false),
            "switch off: now"
        );
        let (a, b, c) = (ClassId::new(1), ClassId::new(2), ClassId::new(3));
        let mut queue = vec![(7, a), (8, b), (7, c)];
        assert_eq!(take_deferred_class_loads(&mut queue, 7), vec![a, c]);
        assert_eq!(queue, vec![(8, b)]);
        assert!(take_deferred_class_loads(&mut queue, 9).is_empty());
        assert_eq!(take_deferred_class_loads(&mut queue, 8), vec![b]);
        assert!(queue.is_empty());
    }

    /// gci i12/ev: a deferred `ClassLoad` reaches the agent at its thread's
    /// flush, run under that thread's own handle; another thread's entry
    /// waits, and the gate drops with the queue empty.
    #[test]
    fn gci_i12ev_a_deferred_class_load_is_delivered_at_its_threads_flush() {
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        let vm = shared.vm_identity;
        jni::set_jni_context_arc(Arc::clone(&shared));
        let cid = loop_class(&shared, "cratonvm/test/I12EvDeferred");
        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        let loaded = leak_into_env_storage(env, AtomicU64::new(0));
        // SAFETY (every transmute below): the slot's `jvmti.h` signature.
        let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
            unsafe { std::mem::transmute(function(env, 2)) };
        let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
            unsafe { std::mem::transmute(function(env, 122)) };
        let dispose: extern "C" fn(*mut c_void) -> JInt =
            unsafe { std::mem::transmute(function(env, 127)) };
        let callbacks = callbacks_with(&[(EVENT_CLASS_LOAD, on_class_load as *const () as usize)]);
        let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
        assert_eq!(
            set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
            ERR_NONE
        );
        if !shared.debug.debugger_gates.c_class_hooks() {
            assert_eq!(dispose(env), ERR_NONE);
            rt::forget_vm_jvmti_state(vm);
            jni::clear_jni_context();
            return;
        }
        assert_eq!(set_mode(env, 1, EVENT_CLASS_LOAD, 0), ERR_NONE);
        let pending = &shared.debug.deferred_class_loads_pending;
        // As `post_class_load` defers them: queued under the lock, gate up.
        {
            let mut state = shared.debug.jvmti_env.lock();
            state.deferred_class_loads.push((7, cid));
            state.deferred_class_loads.push((8, cid));
            pending.store(true, Ordering::Release);
        }
        let mut seven = JvmThread::new(ThreadId(7), "gci-i12ev-seven");
        crate::runtime::interpreter::post_on_own_thread(&mut seven, || {
            flush_deferred_class_loads(&shared, 7)
        });
        assert_eq!(
            loaded.load(Ordering::SeqCst),
            u64::from(cid.as_u32()),
            "thread 7's load"
        );
        assert_eq!(
            shared.debug.jvmti_env.lock().deferred_class_loads,
            vec![(8, cid)]
        );
        assert!(
            pending.load(Ordering::Acquire),
            "thread 8's entry still waits"
        );
        loaded.store(0, Ordering::SeqCst);
        let mut eight = JvmThread::new(ThreadId(8), "gci-i12ev-eight");
        crate::runtime::interpreter::post_on_own_thread(&mut eight, || {
            flush_deferred_class_loads(&shared, 8)
        });
        assert_eq!(
            loaded.load(Ordering::SeqCst),
            u64::from(cid.as_u32()),
            "thread 8's load"
        );
        assert!(shared
            .debug
            .jvmti_env
            .lock()
            .deferred_class_loads
            .is_empty());
        assert!(
            !pending.load(Ordering::Acquire),
            "the gate drops with the queue empty"
        );
        // Nothing waits: a flush is one load.
        loaded.store(0, Ordering::SeqCst);
        flush_deferred_class_loads(&shared, 7);
        assert_eq!(loaded.load(Ordering::SeqCst), 0);
        assert_eq!(set_mode(env, 0, EVENT_CLASS_LOAD, 0), ERR_NONE);
        assert_eq!(dispose(env), ERR_NONE);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }
}

#[cfg(test)]
mod gc8_window_rows_tests {
    //! JIT round 19 proposal wave 8, lane gc8 (GC719-2): a blocked thread's
    //! window rows are matched to its published trace entry by entry, with at
    //! most a native heading, or not at all.
    use super::*;
    use crate::native::registry::StackTraceEntry;

    fn row(location: i64, frame: Option<usize>) -> OtherRow {
        OtherRow {
            info: FrameInfo {
                method: 0,
                location,
            },
            raw: BlockedRow {
                class_id: 0,
                method_id: 0,
                location,
                frame,
                runs_compiled: false,
                chain: None,
            },
        }
    }

    fn entry(class: &str, method: &str) -> StackTraceEntry {
        StackTraceEntry {
            class_name: Arc::from(class),
            method_name: Arc::from(method),
            method_descriptor: None,
            source_file: None,
            line_number: -1,
            byte_code_index: 0,
            class_id: None,
            method_index: None,
        }
    }

    fn name(class: &str, method: &str) -> Option<(String, Arc<str>)> {
        Some((class.to_string(), Arc::from(method)))
    }

    /// `hold` (compiled, holding a block) under `run` (interpreted), blocked
    /// in the native `sleep0` the listing heads with.
    #[test]
    fn rows_match_the_trace_under_a_native_heading() {
        let rows = vec![row(-1, None), row(3, None), row(7, Some(0))];
        let names = vec![
            name("java.lang.Thread", "sleep0"),
            name("p.T", "hold"),
            name("p.T", "run"),
        ];
        let trace = vec![entry("p/T", "hold"), entry("p.T", "run")];
        assert_eq!(
            window_rows_for_trace(&names, &rows, &trace),
            Some(vec![1, 2])
        );
        // Equal lengths: one to one.
        let trace3 = vec![
            entry("java.lang.Thread", "sleep0"),
            entry("p.T", "hold"),
            entry("p.T", "run"),
        ];
        assert_eq!(
            window_rows_for_trace(&names, &rows, &trace3),
            Some(vec![0, 1, 2])
        );
    }

    #[test]
    fn any_disagreement_refuses() {
        let rows = vec![row(-1, None), row(3, None), row(7, Some(0))];
        let names = vec![
            name("java.lang.Thread", "sleep0"),
            name("p.T", "hold"),
            name("p.T", "run"),
        ];
        // A method name differs.
        let trace = vec![entry("p.T", "other"), entry("p.T", "run")];
        assert_eq!(window_rows_for_trace(&names, &rows, &trace), None);
        // A class name differs.
        let trace = vec![entry("p.U", "hold"), entry("p.T", "run")];
        assert_eq!(window_rows_for_trace(&names, &rows, &trace), None);
        // The extra row is not a native heading.
        let rows2 = vec![row(5, Some(1)), row(3, None), row(7, Some(0))];
        let trace = vec![entry("p.T", "hold"), entry("p.T", "run")];
        assert_eq!(window_rows_for_trace(&names, &rows2, &trace), None);
        // Two rows more, or the trace longer than the listing.
        assert_eq!(window_rows_for_trace(&names, &rows, &trace[..1]), None);
        let long = vec![
            entry("x.X", "a"),
            entry("java.lang.Thread", "sleep0"),
            entry("p.T", "hold"),
            entry("p.T", "run"),
        ];
        assert_eq!(window_rows_for_trace(&names, &rows, &long), None);
        // A row that names no loaded method.
        let unnamed = vec![None, name("p.T", "hold"), name("p.T", "run")];
        let trace3 = vec![
            entry("java.lang.Thread", "sleep0"),
            entry("p.T", "hold"),
            entry("p.T", "run"),
        ];
        assert_eq!(window_rows_for_trace(&unnamed, &rows, &trace3), None);
    }
}

#[cfg(test)]
mod gci_i11ja_event_thread_tests {
    //! gci i11/ja (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
    //! item 1): an agent callback keeps a pointer the poster installed from its
    //! own live `&mut`, and gets the registry's address only when none (or
    //! another thread's) is installed.
    use super::*;

    #[test]
    fn gci_i11ja_event_context_keeps_the_posters_pointer() {
        // Installed and published at the same address: kept, nothing replaced.
        assert_eq!(event_thread_to_install(Some(0x1000), Some(0x1000)), None);
        // Nothing installed: the registry's address, as before.
        assert_eq!(event_thread_to_install(Some(0x1000), None), Some(0x1000));
        // Another thread's pointer installed: replaced by this thread's.
        assert_eq!(
            event_thread_to_install(Some(0x1000), Some(0x2000)),
            Some(0x1000)
        );
        // This thread not published: install nothing (the poster's, if any, stays).
        assert_eq!(event_thread_to_install(None, Some(0x1000)), None);
        assert_eq!(event_thread_to_install(None, None), None);
    }

    /// Source pin: `in_event_context` decides through the helper with the raw
    /// installed address, and restores only what it replaced.
    #[test]
    fn gci_i11ja_in_event_context_asks_for_the_installed_address() {
        let src = include_str!("native_env.rs").replace("\r\n", "\n");
        let start = src
            .find("\nfn in_event_context(")
            .expect("in_event_context");
        let rest = &src[start + 1..];
        let body = &rest[..rest.find("\n}\n").expect("its end")];
        let ask = body.find("event_thread_to_install(").expect("the decision");
        assert!(body[ask..].contains("jni::installed_jni_thread_addr()"));
        let call = body
            .find("call(jni::get_jni_env(), thread)")
            .expect("the callback");
        let restore = body
            .find("jni::restore_jni_thread(prev)")
            .expect("the restore");
        assert!(ask < call && call < restore);
        assert!(
            body.contains("if let Some(prev) = prev_thread"),
            "restore only what was replaced"
        );
    }
}

#[cfg(test)]
mod gci_i16sn_native_transition_tests {
    //! gci i16/sn (`gci-i15rb-vm-default-path-changes-review-findings-FIXED-20261017`
    //! finding 6, `CRATONVM_DEBUG_SUSPEND_AT_NATIVE_TRANSITION`): the C
    //! functions behind `live_phase_vm` stop a suspended caller in native at
    //! their entry, as the JNIEnv functions do (`jni::ForeignJniEntry`; the
    //! behaviour is `jni.rs`'s
    //! `gci_i16sn_a_suspended_thread_in_native_stops_at_its_next_jni_entry`).

    /// Source pin: the hold is the prologue's first statement, before the env
    /// is read.
    #[test]
    fn gci_i16sn_live_phase_vm_holds_a_suspended_caller_first() {
        let src = include_str!("native_env.rs").replace("\r\n", "\n");
        let start = src.find("\nfn live_phase_vm(").expect("live_phase_vm");
        let rest = &src[start + 1..];
        let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
        let hold = body
            .find("jni::hold_in_native_while_suspended();")
            .expect("the hold");
        let env = body.find("let env = live_env(env)?;").expect("the env");
        assert!(hold < env, "held before anything is read");
    }
}

#[cfg(test)]
mod gci_i47w_oz47_raw_monitor_lock_tests {
    //! gci i47w lane oz47 (`vi12-ev.list` T, every row a `[stw-stall]` hang
    //! on the i13 and i47z experimental-debug builds): a raw monitor's inner
    //! lock is re-taken by a woken waiter inside its GC-safe region and held
    //! while that region's end waits for a pause, so a contended entry must
    //! itself be GC-safe ([`super::lock_raw_monitor_state`]).

    /// Source pin: the helper tries the lock first and blocks GC-safe only
    /// when it is contended.
    #[test]
    fn gci_i47w_oz47_contended_raw_monitor_lock_is_taken_gc_safe() {
        let src = include_str!("native_env.rs").replace("\r\n", "\n");
        let start = src
            .find("\nfn lock_raw_monitor_state<")
            .expect("lock_raw_monitor_state");
        let rest = &src[start + 1..];
        let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
        let fast = body.find("raw.state.try_lock()").expect("the fast path");
        let slow = body
            .find("raw_monitor_block(env, || raw.state.lock())")
            .expect("the GC-safe slow path");
        assert!(fast < slow);
    }

    /// Source pin: every raw monitor function takes the inner lock through
    /// the helper; no plain `raw.state.lock()` is left outside it.
    #[test]
    fn gci_i47w_oz47_every_raw_monitor_function_uses_the_helper() {
        let src = include_str!("native_env.rs").replace("\r\n", "\n");
        let helper = src
            .find("\nfn lock_raw_monitor_state<")
            .expect("lock_raw_monitor_state");
        let plain = concat!("raw.state", ".lock();");
        assert!(!src.contains(plain), "a plain lock outside the helper");
        for f in [
            "extern \"C\" fn destroy_raw_monitor(",
            "extern \"C\" fn raw_monitor_enter(",
            "extern \"C\" fn raw_monitor_exit(",
            "extern \"C\" fn raw_monitor_wait(",
            "fn raw_monitor_notify(",
        ] {
            let at = src.find(f).expect(f);
            assert!(at > helper, "{f}");
            let rest = &src[at..];
            let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
            assert!(body.contains("lock_raw_monitor_state(env, raw)"), "{f}");
        }
    }
}

#[cfg(test)]
mod i134_l1_blocked_entry_frame_control_tests {
    //! Interpreter round i6 wave 134, lane L1 (i133-L1,
    //! `CRATONVM_JVMTI_BLOCKED_ENTRY_FRAME_CONTROL`): which blocked top frames
    //! the window serves, and the error numbers it hands on.
    use super::*;

    #[test]
    fn only_a_monitor_entry_top_frame_is_served() {
        // A `monitorenter` the thread is blocked in.
        assert!(blocked_at_monitor_entry(Some(0xc2), false));
        // A `synchronized` method's frame pushed before its monitor.
        assert!(blocked_at_monitor_entry(Some(0x2a), true));
        assert!(blocked_at_monitor_entry(None, true));
        // An invoke (the caller of a synchronized method whose monitor is
        // entered before the push), anything else, or no code.
        assert!(!blocked_at_monitor_entry(Some(0xb6), false));
        assert!(!blocked_at_monitor_entry(Some(0xc3), false));
        assert!(!blocked_at_monitor_entry(None, false));
    }

    #[test]
    fn jvmti_codes_keep_their_numbers() {
        assert_eq!(jdwp_numbered(ERR_OPAQUE_FRAME), 32);
        assert_eq!(jdwp_numbered(ERR_NO_MORE_FRAMES), 31);
        assert_eq!(jdwp_numbered(-1), 113);
    }

    /// Source pin: both frame-control slots hand a suspended thread's request
    /// to the window route, and that route is taken only with the switch on
    /// and only after the parked route answered `NotParked`.
    #[test]
    fn the_window_route_follows_the_parked_one() {
        let src = include_str!("native_env.rs").replace("\r\n", "\n");
        let start = src.find("\nfn frame_op_on_parked<").expect("frame_op_on_parked");
        let rest = &src[start + 1..];
        let body = &rest[..rest.find("\n}\n").expect("its end")];
        let parked = body.find("run_on_parked_thread(").expect("parked");
        let gate = body.find("blocked_entry_frame_control_on(shared)").expect("gate");
        let window = body.find("with_blocked_frames(shared, tid, window)").expect("window");
        assert!(parked < gate && gate < window);
        assert!(src.contains("|held| pop_through_window(&shared, held)"));
        assert!(src.contains("|held| forced_return_through_window(&shared, held, kind, forced)"));
    }
}
