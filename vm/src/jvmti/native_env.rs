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
//! `SetHeapSamplingInterval`); every slot answers
//! `JVMTI_ERROR_NOT_AVAILABLE` except the ones this module implements:
//!
//! | slot | function |
//! |---|---|
//! | 2 | `SetEventNotificationMode` (`VMInit`, `VMDeath`, `ClassPrepare`, `Exception`, `SingleStep` (wave 22), `Breakpoint`, `MethodEntry`, `MethodExit`; globally or for one thread) |
//! | 4 | `GetAllThreads` (wave 46) |
//! | 11 | `GetCurrentContendedMonitor`, of any thread (wave 44) |
//! | 16 / 19 / 104 | `GetFrameCount` / `GetFrameLocation` / `GetStackTrace`, of the current thread (wave 22) and of another (wave 45) |
//! | 18 | `GetCurrentThread` (wave 22) |
//! | 21-30 / 155 | `GetLocal*` / `SetLocal*` / `GetLocalInstance`, of the current thread's interpreter frames (wave 44) and of another suspended thread's (wave 46) |
//! | 153 | `GetOwnedMonitorStackDepthInfo`, of the current thread (wave 44) and of another (wave 46) |
//! | 38 / 39 | `SetBreakpoint` / `ClearBreakpoint` |
//! | 46 / 47 | `Allocate` / `Deallocate` |
//! | 48 | `GetClassSignature` |
//! | 49 | `GetClassStatus` (wave 17) |
//! | 52 | `GetClassMethods` |
//! | 64 / 65 | `GetMethodName` / `GetMethodDeclaringClass` |
//! | 66 / 76 | `GetMethodModifiers` / `IsMethodNative` (wave 17) |
//! | 70 / 71 | `GetLineNumberTable` / `GetMethodLocation` (wave 17) |
//! | 88 | `GetVersionNumber` |
//! | 100 / 101 | `GetAllStackTraces` / `GetThreadListStackTraces` (wave 46) |
//! | 89 / 140 / 142 / 143 | `GetCapabilities` / `GetPotentialCapabilities` / `AddCapabilities` / `RelinquishCapabilities` |
//! | 122 | `SetEventCallbacks` |
//! | 127 | `DisposeEnvironment` |
//! | 128 | `GetErrorName` |
//!
//! The capabilities this table grants are `can_generate_breakpoint_events`,
//! `can_generate_exception_events`, (wave 22) `can_generate_single_step_events`
//! and (wave 17)
//! `can_generate_method_entry_events` / `can_generate_method_exit_events`,
//! because those are the events it delivers that need one, and
//! `can_get_line_numbers` (wave 17), which `GetLineNumberTable` needs, and
//! (wave 44) `can_access_local_variables`,
//! `can_get_current_contended_monitor` and
//! `can_get_owned_monitor_stack_depth_info`, which the functions of slots
//! 11, 21-30, 153 and 155 need: an
//! agent that asks for anything else learns at
//! `AddCapabilities` that it is not available (`JVMTI_ERROR_NOT_AVAILABLE`),
//! rather than being granted an event that never arrives (the D14 rule,
//! `jvmti/capabilities.rs`).
//!
//! Delivery, always on the thread the event happens on, with a `JNIEnv*`,
//! the thread's `java.lang.Thread` as a local reference (in a local frame
//! popped after the callback) and the thread's JNI context installed, so the
//! callback can call JNI ([`in_event_context`]):
//!
//! * `Breakpoint`, `Exception`, (wave 17) `MethodEntry` / `MethodExit` and
//!   (wave 22) `SingleStep`
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
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::types::ObjectRef;
use crate::vm::SharedVm;

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
const ERR_NOT_AVAILABLE: JInt = 98;
const ERR_MUST_POSSESS_CAPABILITY: JInt = 99;
const ERR_NULL_POINTER: JInt = 100;
const ERR_ABSENT_INFORMATION: JInt = 101;
const ERR_INVALID_EVENT_TYPE: JInt = 102;
const ERR_ILLEGAL_ARGUMENT: JInt = 103;
const ERR_OUT_OF_MEMORY: JInt = 110;
const ERR_NATIVE_METHOD: JInt = 111;
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
/// `can_access_local_variables`, the 15th bit field (wave 44: the
/// `GetLocal*` / `SetLocal*` functions and `GetLocalInstance`).
const CAN_ACCESS_LOCAL_VARIABLES: u32 = 1 << 14;
/// `can_get_owned_monitor_stack_depth_info`, the 35th bit field: bit 2 of
/// the second word (wave 44: `GetOwnedMonitorStackDepthInfo`).
const CAN_GET_OWNED_MONITOR_STACK_DEPTH_INFO: u32 = 1 << 2;
/// `can_suspend`, the 21st bit field (interpreter round i1 wave 45, lane
/// L1: `SuspendThread`, `ResumeThread`, `SuspendThreadList`,
/// `ResumeThreadList`).
const CAN_SUSPEND: u32 = 1 << 20;
/// What this table can grant in the OnLoad phase beyond [`POTENTIAL`] (wave
/// 45): `can_suspend`, which HotSpot 25.0.3 grants only there (measured: an
/// env obtained in the live phase lists it as not potential and
/// `AddCapabilities` answers `JVMTI_ERROR_NOT_AVAILABLE`, even when a
/// startup agent holds it; `tools/probes/interp/L1/L1W45JvmtiSuspendAndOtherStacks.java`).
const ONLOAD_POTENTIAL: [u32; CAPABILITY_WORDS] = [CAN_SUSPEND, 0, 0, 0];
/// The capabilities of [`POTENTIAL`] that HotSpot 25.0.3 lists as potential
/// in the live phase only when a startup agent acquired them in
/// `Agent_OnLoad`, each on its own, and for good (a later relinquish does not
/// take it back): the three of the local and monitor functions (interpreter
/// round i1 wave 46, lane L1; measured,
/// `tools/probes/interp/L1/L1W46JvmtiLivePhasePotential.java`). Until wave 46
/// this table listed them in every phase, more than HotSpot grants.
const ONLOAD_ONLY: [u32; CAPABILITY_WORDS] = [
    CAN_ACCESS_LOCAL_VARIABLES | CAN_GET_CURRENT_CONTENDED_MONITOR,
    CAN_GET_OWNED_MONITOR_STACK_DEPTH_INFO,
    0,
    0,
];

// The `jvmtiThreadState` bits `GetThreadState` answers (wave 45).
const THREAD_STATE_ALIVE: JInt = 0x0001;
const THREAD_STATE_TERMINATED: JInt = 0x0002;
const THREAD_STATE_RUNNABLE: JInt = 0x0004;
const THREAD_STATE_WAITING_INDEFINITELY: JInt = 0x0010;
const THREAD_STATE_WAITING_WITH_TIMEOUT: JInt = 0x0020;
const THREAD_STATE_WAITING: JInt = 0x0080;
const THREAD_STATE_BLOCKED_ON_MONITOR_ENTER: JInt = 0x0400;
const THREAD_STATE_SUSPENDED: JInt = 0x0010_0000;
const THREAD_STATE_INTERRUPTED: JInt = 0x0020_0000;

/// What this table can grant (in the live phase; [`ONLOAD_POTENTIAL`] adds
/// to it in the OnLoad phase): only what it delivers or answers. In the live
/// phase [`ONLOAD_ONLY`]'s capabilities are potential only once a startup
/// agent acquired them.
const POTENTIAL: [u32; CAPABILITY_WORDS] = [
    CAN_GET_LINE_NUMBERS
        | CAN_GENERATE_SINGLE_STEP_EVENTS
        | CAN_GENERATE_EXCEPTION_EVENTS
        | CAN_GENERATE_BREAKPOINT_EVENTS
        | CAN_GENERATE_METHOD_ENTRY_EVENTS
        | CAN_GENERATE_METHOD_EXIT_EVENTS
        | CAN_GET_CURRENT_CONTENDED_MONITOR
        | CAN_ACCESS_LOCAL_VARIABLES
        | CAN_GENERATE_FRAME_POP_EVENTS,
    CAN_GET_OWNED_MONITOR_STACK_DEPTH_INFO,
    0,
    0,
];

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
// The `jvmtiEvent` numbers of the events this table delivers (`jvmti.h`).
// Until wave 13 `Breakpoint` was taken as 60 — `SingleStep` — and its
// callback read from `SingleStep`'s member of `jvmtiEventCallbacks`, so an
// agent compiled against `jvmti.h` could not enable it.
const EVENT_VM_INIT: JInt = 50;
const EVENT_VM_DEATH: JInt = 51;
const EVENT_CLASS_PREPARE: JInt = 56;
const EVENT_EXCEPTION: JInt = 58;
const EVENT_SINGLE_STEP: JInt = 60;
const EVENT_FRAME_POP: JInt = 61;
const EVENT_BREAKPOINT: JInt = 62;
const EVENT_METHOD_ENTRY: JInt = 65;
const EVENT_METHOD_EXIT: JInt = 66;
/// The events delivered through the env's delivery env on the VM's manager
/// ([`JvmtiNativeEnv::sync_event`]), each with the capability it needs.
const MANAGER_EVENTS: [(JInt, u32); 6] = [
    (EVENT_BREAKPOINT, CAN_GENERATE_BREAKPOINT_EVENTS),
    (EVENT_SINGLE_STEP, CAN_GENERATE_SINGLE_STEP_EVENTS),
    (EVENT_EXCEPTION, CAN_GENERATE_EXCEPTION_EVENTS),
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
    u32::try_from(event - MIN_EVENT)
        .ok()
        .and_then(|shift| 1u64.checked_shl(shift))
        .unwrap_or(0)
}

/// The capability an event this table delivers needs (0: none), or `None`
/// for an event it does not deliver.
fn required_capability(event: JInt) -> Option<u32> {
    match event {
        EVENT_VM_INIT | EVENT_VM_DEATH | EVENT_CLASS_PREPARE => Some(0),
        EVENT_EXCEPTION => Some(CAN_GENERATE_EXCEPTION_EVENTS),
        EVENT_BREAKPOINT => Some(CAN_GENERATE_BREAKPOINT_EVENTS),
        EVENT_SINGLE_STEP => Some(CAN_GENERATE_SINGLE_STEP_EVENTS),
        EVENT_METHOD_ENTRY => Some(CAN_GENERATE_METHOD_ENTRY_EVENTS),
        EVENT_METHOD_EXIT => Some(CAN_GENERATE_METHOD_EXIT_EVENTS),
        EVENT_FRAME_POP => Some(CAN_GENERATE_FRAME_POP_EVENTS),
        _ => None,
    }
}

thread_local! {
    /// While a VM loads its startup agents on this thread
    /// ([`collect_startup_envs`]), the envs their `GetEnv` calls hand out.
    static STARTUP_ENVS: RefCell<Option<Vec<usize>>> = const { RefCell::new(None) };
}

/// Run `load` (the startup agents' `Agent_OnLoad`s) and answer the envs they
/// obtained through `GetEnv` meanwhile, on this thread. Those envs act on the
/// VM being built: they stay unbound (live-phase functions answer
/// `JVMTI_ERROR_WRONG_PHASE`) until `Vm::new` binds them
/// ([`bind_startup_envs`]). Before wave 13 such an env bound itself to
/// whatever VM the thread could reach (`process_vm()`), which is another VM
/// when one is already running in the process, and no `VMInit` reached it.
pub(crate) fn collect_startup_envs(load: impl FnOnce()) -> Vec<usize> {
    /// Puts back the collection this one replaced, also on a panic (a
    /// failed startup agent panics the boot).
    struct Restore(Option<Option<Vec<usize>>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(outer) = self.0.take() {
                let _ = STARTUP_ENVS.try_with(|c| *c.borrow_mut() = outer);
            }
        }
    }
    let outer = STARTUP_ENVS.with(|c| c.replace(Some(Vec::new())));
    let restore = Restore(Some(outer));
    load();
    let collected = STARTUP_ENVS
        .with(|c| c.borrow_mut().take())
        .unwrap_or_default();
    drop(restore);
    collected
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
    let supported = match major {
        1 => minor <= 2,
        9 | 11..=25 => true,
        _ => false,
    };
    if !supported {
        return None;
    }
    let startup = STARTUP_ENVS.with(|c| c.borrow().is_some());
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
            disposed: false,
        }),
        sync: parking_lot::Mutex::new(()),
    });
    // OWNERSHIP: handed to the agent and never freed (see the module doc).
    let env = Box::into_raw(env);
    if startup {
        STARTUP_ENVS.with(|c| {
            if let Some(list) = c.borrow_mut().as_mut() {
                list.push(env as usize); // Cast: the env's address, its registry key
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
    let slots: [(usize, usize); 48] = [
        (2, set_event_notification_mode as *const () as usize),
        (4, get_all_threads as *const () as usize),
        (5, suspend_thread as *const () as usize),
        (6, resume_thread as *const () as usize),
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
        (153, get_owned_monitor_stack_depth_info as *const () as usize),
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

    fn possesses(&self, capability: u32) -> bool {
        self.state.lock().capabilities[0] & capability != 0
    }

    /// What this env can be granted now (wave 45): [`POTENTIAL`], and in the
    /// OnLoad phase (a startup agent's env not bound yet) also
    /// [`ONLOAD_POTENTIAL`]; in the live phase (wave 46) [`ONLOAD_ONLY`]'s
    /// capabilities only as far as a startup env of the VM acquired them
    /// (`JvmtiEnv::onload_acquired`).
    fn potential(&self) -> [u32; CAPABILITY_WORDS] {
        let mut caps = POTENTIAL;
        if self.state.lock().startup_pending {
            for (cap, onload) in caps.iter_mut().zip(ONLOAD_POTENTIAL) {
                *cap |= onload;
            }
            return caps;
        }
        let acquired = self
            .vm()
            .map_or([0; CAPABILITY_WORDS], |shared| onload_acquired_of(&shared));
        for ((cap, only), got) in caps.iter_mut().zip(ONLOAD_ONLY).zip(acquired) {
            *cap &= !(only & !got);
        }
        caps
    }

    /// Is the VM this env acts on in JVMTI's start phase (wave 14): bound by
    /// `Vm::new`, `VMInit` not sent yet ([`enter_live_phase`])? The functions
    /// JVMTI allows only in the OnLoad and live phases answer
    /// `JVMTI_ERROR_WRONG_PHASE` then, as HotSpot's do. An env still in
    /// `Agent_OnLoad` (no VM yet) is not.
    fn in_start_phase(&self) -> bool {
        self.vm()
            .is_some_and(|shared| shared.debug.debugger_gates.jvmti_start_phase())
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
        shared.debug.jvmti_env.lock().native_envs.push(self.addr());
        // Wave 45 (lane L1): blocked threads open their inspection window for
        // this table's stack functions from now on.
        shared.debug.debugger_gates.set_native_env_present();
        for (event, _) in MANAGER_EVENTS {
            let _ = self.sync_event(shared, event);
        }
        refresh_class_prepare_gate(shared);
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
            EVENT_METHOD_ENTRY => JvmtiEventKind::MethodEntry,
            EVENT_METHOD_EXIT => JvmtiEventKind::MethodExit,
            EVENT_FRAME_POP => JvmtiEventKind::FramePop,
            EVENT_CLASS_PREPARE => {
                refresh_class_prepare_gate(shared);
                return ERR_NONE;
            }
            _ => return ERR_NONE,
        };
        let _serial = self.sync.lock();
        let live = !shared.debug.debugger_gates.jvmti_start_phase();
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
    shared.debug.jvmti_env.lock().onload_acquired
}

/// Raise or lower `shared`'s `ClassPrepare` gate
/// (`DebuggerGates::native_class_prepare_armed`) from its native envs'
/// state. Under the VM's JVMTI lock, so two envs changing their state at
/// once cannot publish out of order.
fn refresh_class_prepare_gate(shared: &SharedVm) {
    let jvmti = shared.debug.jvmti_env.lock();
    let armed = jvmti.native_envs.iter().any(|&addr| {
        // SAFETY: the list holds only addresses `new_env_for_version` handed
        // out.
        let state = unsafe { env_at(addr) }.state.lock();
        !state.disposed && state.enabled_anywhere(EVENT_CLASS_PREPARE)
    });
    shared.debug.debugger_gates.set_native_class_prepare(armed);
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
    let depth = jni::local_frame_depth();
    jni::push_local_frame(16);
    // No safepoint between the registry read and the handle.
    let thread = registry
        .java_thread_obj(thread_id)
        .map_or(0, jni::new_local_handle);
    let prev_vm = jni::replace_jni_context(shared);
    // The executing thread's own `JvmThread`, which only it may use.
    let prev_thread = registry
        .own_jvm_thread_addr(thread_id)
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
        if !state.class_prepare_posted.insert(class_id) {
            return;
        }
        state.native_envs.clone()
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
    let envs = std::mem::take(&mut shared.debug.jvmti_env.lock().startup_native_envs);
    if envs.is_empty() {
        return;
    }
    if shared.get_init_level() < 3 {
        shared.debug.debugger_gates.swap_jvmti_start_phase(true);
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
        for (record, got) in jvmti.onload_acquired.iter_mut().zip(acquired) {
            *record |= got;
        }
    }
    for env_addr in envs {
        // SAFETY: an address `new_env_for_version` handed out.
        unsafe { env_at(env_addr) }.attach_to(shared);
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
    if !shared.debug.debugger_gates.swap_jvmti_start_phase(false) {
        // Another thread made the transition.
        return;
    }
    post_vm_init(&shared, 0);
}

/// The live phase's first act on `shared`'s native envs: arm the events the
/// start phase held back, then `VMInit` on thread `tid`.
fn post_vm_init(shared: &Arc<SharedVm>, tid: u64) {
    let envs = shared.debug.jvmti_env.lock().native_envs.clone();
    for &env_addr in &envs {
        // SAFETY: an address `new_env_for_version` handed out.
        let env = unsafe { env_at(env_addr) };
        for (event, _) in MANAGER_EVENTS {
            let _ = env.sync_event(shared, event);
        }
    }
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
    // no `VMDeath`, as HotSpot does for a VM whose creation failed.
    let envs = if shared.debug.debugger_gates.jvmti_start_phase() {
        Vec::new()
    } else {
        shared.debug.jvmti_env.lock().native_envs.clone()
    };
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
    let mut agents = std::mem::take(&mut shared.debug.jvmti_env.lock().agent_registry);
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
    let _fx = jni::ForeignJniEntry::enter();
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
    if env.in_start_phase() {
        return ERR_WRONG_PHASE;
    }
    // `JVMTI_ENABLE` / `JVMTI_DISABLE`.
    let enable = match mode {
        1 => true,
        0 => false,
        _ => return ERR_ILLEGAL_ARGUMENT,
    };
    if !(MIN_EVENT..=MAX_EVENT).contains(&event_type) {
        return ERR_INVALID_EVENT_TYPE;
    }
    let Some(capability) = required_capability(event_type) else {
        return ERR_NOT_AVAILABLE;
    };
    if event_thread != 0 && matches!(event_type, EVENT_VM_INIT | EVENT_VM_DEATH) {
        // Not controllable per thread.
        return ERR_ILLEGAL_ARGUMENT;
    }
    if enable && capability != 0 && !env.possesses(capability) {
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
    if env.in_start_phase() {
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
    if shared.debug.debugger_gates.jvmti_start_phase() {
        return ERR_WRONG_PHASE;
    }
    if !env.possesses(CAN_GENERATE_BREAKPOINT_EVENTS) {
        return ERR_MUST_POSSESS_CAPABILITY;
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
    if shared.debug.debugger_gates.jvmti_start_phase() {
        return ERR_WRONG_PHASE;
    }
    if !env.possesses(CAN_GENERATE_BREAKPOINT_EVENTS) {
        return ERR_MUST_POSSESS_CAPABILITY;
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
    let mem = allocate_block(bytes.len());
    if !mem.is_null() {
        // SAFETY: `mem` is a fresh block of `bytes.len()` bytes.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), mem, bytes.len()) };
    }
    mem
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
    let Some(shared) = env.vm() else {
        return ERR_WRONG_PHASE;
    };
    let Some(class_id) = class_of(&shared, klass) else {
        return ERR_INVALID_CLASS;
    };
    let Some((name, generic)) = shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .map(|c| (c.name.to_string(), c.signature.clone()))
    else {
        return ERR_INVALID_CLASS;
    };
    let signature = class_signature(&name);
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
    m.attributes.iter().find_map(|a| {
        match a.decoded_or_decode(&class.constant_pool)?.as_ref() {
            Attribute::Signature(s) => Some(s.to_string()),
            _ => None,
        }
    })
}

/// Slot 52: `GetClassMethods(env, klass, method_count_ptr, methods_ptr)`:
/// the `jmethodID`s of the methods the class declares (constructors and the
/// static initializer included), in class-file order, in an `Allocate`
/// block — the ids `SetBreakpoint` and `GetMethodName` take.
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
    if method_count_ptr.is_null() || methods_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let Some(shared) = env.vm() else {
        return ERR_WRONG_PHASE;
    };
    let Some(class_id) = class_of(&shared, klass) else {
        return ERR_INVALID_CLASS;
    };
    let count = shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .map_or(0, |c| c.methods.len());
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

/// `read` of the declaring class and the method a real `(class id << 32) |
/// index` `jmethodID` names in `shared`, under the class-manager read lock;
/// `None` for an id that names no method.
fn with_method<R>(
    shared: &SharedVm,
    method: u64,
    read: impl FnOnce(ClassId, &cratonvm_reader::method::ClassFileMethod) -> R,
) -> Option<R> {
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
    let Some(shared) = env.vm() else {
        return ERR_WRONG_PHASE;
    };
    let Some((_, name, descriptor)) = method_of(&shared, method) else {
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
    if declaring_class_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let Some(shared) = env.vm() else {
        return ERR_WRONG_PHASE;
    };
    let Some((class_id, _, _)) = method_of(&shared, method) else {
        return ERR_INVALID_METHODID;
    };
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
    let Some(shared) = env.vm() else {
        return ERR_WRONG_PHASE;
    };
    let Some(class_id) = class_of(&shared, klass) else {
        return ERR_INVALID_CLASS;
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
    let Some(shared) = env.vm() else {
        return ERR_WRONG_PHASE;
    };
    let Some(modifiers) = with_method(&shared, method, |_, m| {
        JInt::from(m.access_flags.bits() & RECOGNIZED_METHOD_MODIFIERS)
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
    let Some(shared) = env.vm() else {
        return ERR_WRONG_PHASE;
    };
    let Some(native) = with_method(&shared, method, |_, m| m.is_native()) else {
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
    let Some(shared) = env.vm() else {
        return ERR_WRONG_PHASE;
    };
    let Some(size) = with_method(&shared, method, |_, m| {
        (!m.is_native()).then(|| m.code().map_or(0, |code| code.code.len()))
    }) else {
        return ERR_INVALID_METHODID;
    };
    let Some(size) = size else {
        return ERR_NATIVE_METHOD;
    };
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
    let Some(shared) = env.vm() else {
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

/// The Java frames of the calling thread, the current one of `shared`,
/// innermost first, as `(jmethodID, jlocation)` (interpreter round i1 wave
/// 22, lane L1;
/// `i13-L1-proposal-jvmti-c-table-stack-and-line-functions`, stage 2): its
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
/// `docs/internal/fixed-bugs/interpreter-L1-jvmti-stack-functions-omit-the-native-method-that-calls-them-FIXED-20261006.md`):
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
///   `docs/known-issues/interpreter/i43-L3-a-throwable-a-reflective-native-raises-lists-no-reflection-frames-20261007.md`;
///   `stackwalker::reflective_entries`, anchored at `JvmThread::reflective_calls`).
///   HotSpot also lists the accessor's `@Hidden` `invokeImpl` and the
///   method-handle frames under it, which this VM does not run.
fn listed_rows(shared: &SharedVm, current: &JvmThread, with_frames: bool) -> Vec<ListedRow> {
    let frames = &current.frames;
    let natives = &current.jni_native_frames;
    let calls = &current.reflective_calls;
    let (trace, positions) = if natives.is_empty() && calls.is_empty() && !with_frames {
        (
            crate::runtime::stackwalker::capture_full_trace_without_store(frames),
            Vec::new(),
        )
    } else {
        let frame_anchors = if with_frames { frames.len() } else { 0 };
        let mut anchors: Vec<(u32, u32)> =
            Vec::with_capacity(natives.len() + calls.len() + frame_anchors);
        anchors.extend(
            natives
                .iter()
                .map(|&(depth, _, _, jit_depth)| (u32::try_from(depth).unwrap_or(u32::MAX), jit_depth)),
        );
        anchors.extend(calls.iter().map(|call| (call.interp_depth, call.jit_depth)));
        // A frame's own slot: no compiled activation's chain index reaches
        // `u32::MAX`, so the first slot at or past frame `d` that the anchor
        // finds is frame `d`'s.
        anchors.extend((0..frame_anchors).map(|d| (u32::try_from(d).unwrap_or(u32::MAX), u32::MAX)));
        crate::runtime::stackwalker::capture_trace_with_anchor_positions(shared, frames, &anchors)
    };
    let native_end = natives.len().min(positions.len());
    let call_end = (native_end + calls.len()).min(positions.len());
    let native_positions = &positions[..native_end];
    let call_positions = &positions[native_end..call_end];
    let frame_positions = &positions[call_end..];
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
    let cm = shared.classes.class_manager.read();
    // Each reflective call's JDK frames, OUTERMOST first, at its position; a
    // call whose frames cannot be named (a JDK without these accessors) is
    // left out, as a stack trace leaves it out.
    let reflective: Vec<(usize, Vec<crate::native::registry::StackTraceEntry>)> = calls
        .iter()
        .zip(call_positions)
        .filter_map(|(call, &position)| {
            crate::runtime::stackwalker::reflective_entries(&cm.class_store, call.kind)
                .map(|entries| (position, entries))
        })
        .collect();
    let rows = splice_rows(&trace, native_positions, natives, &reflective);
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
        let (entry, frame) = match *row {
            FrameRow::Java(at, entry) => {
                // The interpreter frame whose row this is, if any: the
                // innermost frame anchored there (a frame the trace does not
                // list anchors at the next one's slot).
                let frame = frame_positions
                    .iter()
                    .rposition(|&p| p == at)
                    .filter(|index| !stale.contains(index));
                (entry, frame)
            }
            FrameRow::Spliced(entry) => (entry, None),
            FrameRow::Native(class_id, key) => {
                let Some(class) = cm.get_class(class_id) else {
                    continue;
                };
                // A native method's frame has no bytecode: location -1.
                let Some(index) = row_method_index(shared, class, key).and_then(|i| u16::try_from(i).ok())
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
        });
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
    inserts.extend(reflective.iter().enumerate().map(|(k, (position, entries))| {
        (*position, 0, k, Inserted::Reflective(entries.as_slice()))
    }));
    inserts.extend(natives.iter().enumerate().map(|(k, &(_, class_id, key, _))| {
        let position = positions.get(k).copied().unwrap_or(usize::MAX);
        (position, 1, k, Inserted::Native(class_id, key))
    }));
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

/// The index, in `class`, of the native method a row's `key` names
/// ([`NativeFrameRow`]); `None` when none does. Read-time work only: a
/// hashed key is compared with each method's [`native_row_method_hash`]; a
/// function pointer with what this VM's JNI table binds each native method
/// of the class to (`jni::find_jni_native`: `RegisterNatives` and the
/// naming-convention lookup, which caches its answer there before the call).
/// Two methods bound to one function are told apart by neither; the first is
/// named.
fn row_method_index(shared: &SharedVm, class: &crate::classloading::Class, key: u64) -> Option<usize> {
    if key & ROW_KEY_IS_HASH != 0 {
        return class.methods.iter().position(|m| {
            native_row_method_hash(&m.name, &m.descriptor) | ROW_KEY_IS_HASH == key
        });
    }
    class.methods.iter().position(|m| {
        m.is_native()
            && jni::find_jni_native(&shared.natives, &class.name, &m.name, &m.descriptor)
                // Widening: a function address into the key.
                .is_some_and(|bound| bound as u64 == key)
    })
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
    pub(crate) unsafe fn enter_jni(thread: *mut JvmThread, declaring: ClassId, fn_ptr: usize) -> Self {
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

/// The VM a stack function acts on, when it is in the live phase (the stack
/// functions are live-phase only).
fn live_phase_vm(env: *mut JvmtiNativeEnv) -> Result<Arc<SharedVm>, JInt> {
    let env = live_env(env)?;
    if env.in_start_phase() {
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
    let Some(shared) = env.vm() else {
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
extern "C" fn get_frame_count(env: *mut JvmtiNativeEnv, thread: JObject, count_ptr: *mut JInt) -> JInt {
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
// `docs/internal/fixed-bugs/interpreter-L1-proposal-c-jvmti-all-threads-stack-traces-FIXED-20261010.md`:
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

/// Every live thread of `shared` that has a `java.lang.Thread`, as local
/// references of the calling thread (made inside its foreign-thread entry,
/// in `shared`'s JNI context).
fn all_thread_handles(shared: &SharedVm) -> Vec<JObject> {
    let _fx = jni::ForeignJniEntry::enter();
    let threads = shared.threads.thread_registry.alive_thread_objects(usize::MAX);
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
// `docs/known-issues/interpreter/i44-L1-proposal-c-jvmti-suspension-and-frame-control-20261008.md`.
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
        ctx.begin_blocking_region();
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
/// none. `None` when no listing of this suspension stands.
fn published_rows(
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
            let interpreted = !native && opaque != Some(true);
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
    let (answer, via) = match crate::runtime::interpreter::with_blocked_frames(shared, tid, &window) {
        Some(rows) => (Ok(rows), "window"),
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
                    eprintln!("[JVMTI_SUSPEND] handshake tid={tid} stopped={}", answer.is_ok());
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
                        Ok(other_rows(shared, rows).into_iter().map(|row| row.info).collect()),
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
        return current_tid.map(|tid| (tid, true)).ok_or(ERR_UNATTACHED_THREAD);
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
/// region is `JVMTI_ERROR_NOT_AVAILABLE` for now.
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

/// Slot 17: `GetThreadState(env, thread, thread_state_ptr)` (wave 45; no
/// capability). A NULL `thread` is the calling thread. The answer carries
/// `java.lang.Thread.State`'s bits (from the thread's blocking kind,
/// `ThreadRegistry::java_block_state`: `RUNNABLE`, `BLOCKED_ON_MONITOR_ENTER`,
/// `WAITING` with `INDEFINITELY` or `WITH_TIMEOUT`), `SUSPENDED` (a JDWP or a
/// JVMTI suspension, `DebugState::is_thread_suspended`) and `INTERRUPTED`;
/// not yet the finer `SLEEPING`, `IN_OBJECT_WAIT`, `PARKED` and `IN_NATIVE`.
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
    let mut state = THREAD_STATE_ALIVE
        | match registry.java_block_state(id) {
            1 => THREAD_STATE_WAITING | THREAD_STATE_WAITING_INDEFINITELY,
            2 => THREAD_STATE_BLOCKED_ON_MONITOR_ENTER,
            3 => THREAD_STATE_WAITING | THREAD_STATE_WAITING_WITH_TIMEOUT,
            _ => THREAD_STATE_RUNNABLE,
        };
    if registry
        .get_interrupted_flag(id)
        .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire))
    {
        state |= THREAD_STATE_INTERRUPTED;
    }
    if shared.debug.debug_state.lock().is_thread_suspended(tid) {
        state |= THREAD_STATE_SUSPENDED;
    }
    state
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
    use cratonvm_reader::attribute::Attribute;
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
    let bci = frame.last_instr_pc;
    let mut has_table = false;
    for attribute in &code.attributes {
        let Attribute::LocalVariableTable(entries) = attribute else {
            continue;
        };
        has_table = true;
        for e in entries {
            let start = usize::from(e.start_pc);
            if e.index == slot && start <= bci && bci < start + usize::from(e.length) {
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
    let index = row.frame.ok_or(ERR_OPAQUE_FRAME)?;
    // SAFETY: the calling thread's own `JvmThread`, inside this call: its
    // frames do not change while they are read.
    let frame = unsafe { (*current).frames.get(index) }.ok_or(ERR_OPAQUE_FRAME)?;
    let declared = u16::try_from(slot).map_or(Declared::NoTable, |s| declared_local(&shared, frame, s));
    // The frame's references are read, and a handle made, inside the
    // foreign-thread entry, so no collection moves them meanwhile.
    let _fx = jni::ForeignJniEntry::enter();
    let slot = checked_local(declared, frame, slot, kind)?;
    let value = crate::debug::debugger_local(frame, slot);
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
    let Some(index) = row.frame else {
        return ERR_OPAQUE_FRAME;
    };
    let declared = {
        // SAFETY: as in `read_local`.
        let Some(frame) = (unsafe { (*current).frames.get(index) }) else {
            return ERR_OPAQUE_FRAME;
        };
        u16::try_from(slot).map_or(Declared::NoTable, |s| declared_local(&shared, frame, s))
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
    match read_local(env, thread, depth, slot, LocalKind::Object, value_ptr.is_null()) {
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
    match read_local(env, thread, depth, slot, LocalKind::Int, value_ptr.is_null()) {
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
    match read_local(env, thread, depth, slot, LocalKind::Long, value_ptr.is_null()) {
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
    match read_local(env, thread, depth, slot, LocalKind::Float, value_ptr.is_null()) {
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
    match read_local(env, thread, depth, slot, LocalKind::Double, value_ptr.is_null()) {
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
    write_local(env, thread, depth, slot, LocalKind::Object, LocalRead::Object(value))
}

/// Slot 27: `SetLocalInt(env, thread, depth, slot, value)` (wave 44).
extern "C" fn set_local_int(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    value: JInt,
) -> JInt {
    write_local(env, thread, depth, slot, LocalKind::Int, LocalRead::Int(value))
}

/// Slot 28: `SetLocalLong(env, thread, depth, slot, value)` (wave 44).
extern "C" fn set_local_long(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    value: i64,
) -> JInt {
    write_local(env, thread, depth, slot, LocalKind::Long, LocalRead::Long(value))
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
    write_local(env, thread, depth, slot, LocalKind::Float, LocalRead::Float(value))
}

/// Slot 30: `SetLocalDouble(env, thread, depth, slot, value)` (wave 44).
extern "C" fn set_local_double(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    value: f64,
) -> JInt {
    write_local(env, thread, depth, slot, LocalKind::Double, LocalRead::Double(value))
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
    let is_static = with_method(&shared, row.info.method, |_, m| m.is_static()).unwrap_or(true);
    if is_static {
        return ERR_INVALID_SLOT;
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
/// JDK-8256314), NULL when none, from its lock record
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
    if monitor_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let _fx = jni::ForeignJniEntry::enter();
    let contended = shared
        .threads
        .thread_registry
        .jmx_lock_snapshot(ThreadId(tid))
        .and_then(|snapshot| snapshot.0);
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
    let shared = match live_phase_vm_with(env, 1, CAN_GET_OWNED_MONITOR_STACK_DEPTH_INFO) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
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
    if count_ptr.is_null() || info_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
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
    let pairs =
        crate::runtime::interpreter::held_monitors::frame_locked_monitors(unsafe { &(*current).frames });
    let monitors = crate::debug::inspect::jvmti_owned_monitor_order(&owned, pairs, |index| {
        rows.iter()
            .position(|row| row.frame == Some(index))
            .and_then(|depth| JInt::try_from(depth).ok())
    });
    let Ok(count) = JInt::try_from(monitors.len()) else {
        return ERR_INTERNAL;
    };
    let Some(size) = monitors.len().checked_mul(std::mem::size_of::<MonitorDepthRow>()) else {
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
// `docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-reads-no-other-threads-stack-FIXED-20261010.md`:
// the local-variable functions of another (suspended) thread and
// `GetOwnedMonitorStackDepthInfo` of another (any) thread. Both are answered
// where the thread's frames hold still, by the same two routes the stack
// functions use ([`other_thread_frames`]):
//
// * blocked in a native region: through its inspection window
//   (`interpreter::with_blocked_frames`), read by the caller while the
//   thread's leave waits. A reference local is its current address from the
//   blocking deposit's per-slot record; a primitive may be written in place
//   (the wake rewrites only reference slots); a reference may not
//   (`JVMTI_ERROR_NOT_AVAILABLE`: it would be neither a root nor remapped
//   while the thread sleeps).
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
        move |shared: &SharedVm, thread: &mut JvmThread| local_op_on_parked(shared, thread, depth, op),
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
        if let Some(answer) = crate::runtime::interpreter::with_blocked_frames(shared, tid, &window) {
            return Ok((answer, "window"));
        }
        if shared.debug.debug_state.lock().is_parked(tid) {
            let work = parked.clone();
            let ran = gc_safe(shared, || crate::debug::run_on_parked_thread(shared, Some(tid), work));
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
        gc_safe(shared, || std::thread::sleep(std::time::Duration::from_millis(1)));
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
    u64::from(frame.class_id.as_u32()) == raw.class_id && crate::debug::frame_method_id(frame) == raw.method_id
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
    if matches!(op, LocalOp::Instance)
        && with_method(shared, row.info.method, |_, m| m.is_static()).unwrap_or(true)
    {
        return Err(ERR_INVALID_SLOT);
    }
    let index = row.frame.ok_or(ERR_OPAQUE_FRAME)?;
    let frame = thread.frames.get(index).ok_or(ERR_OPAQUE_FRAME)?;
    match op {
        LocalOp::Instance => {
            let crate::types::Value::Object(receiver) = crate::debug::debugger_local(frame, 0) else {
                return Err(ERR_TYPE_MISMATCH);
            };
            Ok(Some(LocalRead::Object(global_ref_of(shared, receiver))))
        }
        LocalOp::Read(kind, slot) => {
            let declared =
                u16::try_from(slot).map_or(Declared::NoTable, |s| declared_local(shared, frame, s));
            let slot = checked_local(declared, frame, slot, kind)?;
            let value = crate::debug::debugger_local(frame, slot);
            local_read_of(kind, value, |obj| global_ref_of(shared, obj)).map(Some)
        }
        LocalOp::Write(kind, slot, value) => {
            let declared =
                u16::try_from(slot).map_or(Declared::NoTable, |s| declared_local(shared, frame, s));
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
    let _fx = jni::ForeignJniEntry::enter();
    let now = |slot: u16, v: crate::types::Value| match v {
        crate::types::Value::Object(Some(held_ref)) => {
            crate::types::Value::Object(held.current_reference(shared, index, slot, held_ref))
        }
        other => other,
    };
    match op {
        LocalOp::Instance => {
            let crate::types::Value::Object(receiver) = now(0, crate::debug::debugger_local(frame, 0)) else {
                return Err(ERR_TYPE_MISMATCH);
            };
            Ok(Some(LocalRead::Object(global_ref_of(shared, receiver))))
        }
        LocalOp::Read(kind, slot) => {
            let slot = checked_local(declared, frame, slot, kind)?;
            let value = now(slot, crate::debug::debugger_local(frame, slot));
            local_read_of(kind, value, |obj| global_ref_of(shared, obj)).map(Some)
        }
        LocalOp::Write(kind, slot, value) => {
            let slot = checked_local(declared, frame, slot, kind)?;
            let value = match value {
                LocalRead::Int(v) => crate::types::Value::Int(v),
                LocalRead::Long(v) => crate::types::Value::Long(v),
                LocalRead::Float(v) => crate::types::Value::Float(v),
                LocalRead::Double(v) => crate::types::Value::Double(v),
                // Not stored into a sleeping thread's frame (see the section
                // note); HotSpot writes it.
                LocalRead::Object(_) => return Err(ERR_NOT_AVAILABLE),
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
            answer.as_ref().map_or(-1, |m| i64::try_from(m.len()).unwrap_or(-1))
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
fn monitors_on_parked(shared: &SharedVm, thread: &mut JvmThread) -> Result<Vec<(JObject, JInt)>, JInt> {
    let rows = listed_rows(shared, thread, true);
    let owned = owned_monitors(shared, thread.thread_id.0)?;
    let pairs = crate::runtime::interpreter::held_monitors::frame_locked_monitors(&thread.frames);
    let monitors = crate::debug::inspect::jvmti_owned_monitor_order(&owned, pairs, |index| {
        rows.iter()
            .position(|row| row.frame == Some(index))
            .and_then(|depth| JInt::try_from(depth).ok())
    });
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
    let owned = owned_monitors(shared, tid)?;
    let frames = held.frames();
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
        monitors.iter().for_each(|&(global, _)| release_global(shared, global));
        return ERR_INTERNAL;
    };
    let Some(size) = monitors.len().checked_mul(std::mem::size_of::<MonitorDepthRow>()) else {
        monitors.iter().for_each(|&(global, _)| release_global(shared, global));
        return ERR_OUT_OF_MEMORY;
    };
    let block = allocate_block(size).cast::<MonitorDepthRow>();
    if block.is_null() {
        monitors.iter().for_each(|&(global, _)| release_global(shared, global));
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
        Ok(env) if env.in_start_phase() => ERR_WRONG_PHASE,
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
    if env.in_start_phase() {
        return ERR_WRONG_PHASE;
    }
    let Some(requested) = read_capabilities(caps) else {
        return ERR_NULL_POINTER;
    };
    if requested.iter().zip(env.potential()).any(|(&r, p)| r & !p != 0) {
        return ERR_NOT_AVAILABLE;
    }
    let mut state = env.state.lock();
    for (have, r) in state.capabilities.iter_mut().zip(requested) {
        *have |= r;
    }
    ERR_NONE
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
    if env.in_start_phase() {
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
            .native_envs
            .retain(|&listed| listed != addr);
        refresh_class_prepare_gate(&shared);
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
    if known as JInt != error {
        return ERR_ILLEGAL_ARGUMENT;
    }
    let mem = allocate_modified_utf8(known.name());
    if mem.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *name_ptr = mem };
    ERR_NONE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::frame::Frame;
    use crate::types::Value;
    use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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
        shared.debug.jvmti_env.lock().onload_acquired = ONLOAD_ONLY;
        shared
    }

    /// Wave 46: in the live phase the three capabilities of the local and
    /// monitor functions are potential only as far as a startup env acquired
    /// them ([`ONLOAD_ONLY`], HotSpot 25.0.3's answer), each on its own.
    #[test]
    fn the_onload_only_capabilities_follow_what_a_startup_env_acquired() {
        let _lock = rt::jvmti_registry_test_lock();
        let shared = vm_fixture();
        shared.debug.jvmti_env.lock().onload_acquired = [0; CAPABILITY_WORDS];
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
        shared.debug.jvmti_env.lock().onload_acquired = locals;
        assert_eq!(potential(env, &mut caps), ERR_NONE);
        assert_eq!(caps[0] & CAN_ACCESS_LOCAL_VARIABLES, CAN_ACCESS_LOCAL_VARIABLES);
        assert_eq!(caps[0] & CAN_GET_CURRENT_CONTENDED_MONITOR, 0);
        assert_eq!(caps[1] & CAN_GET_OWNED_MONITOR_STACK_DEPTH_INFO, 0);
        assert_eq!(add(env, &locals), ERR_NONE);
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
                attributes: vec![LazyAttribute::Decoded(Attribute::Code(CodeAttribute {
                    max_stack: 2,
                    max_locals: 3,
                    code: cratonvm_reader::ByteView::from_vec(loop_code()),
                    exception_table: vec![],
                    attributes: vec![],
                }))],
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
            ERR_NOT_AVAILABLE,
            "GetTag is not provided"
        );

        let mut caps: Caps = [0; CAPABILITY_WORDS];
        assert_eq!(potential(env, &mut caps), ERR_NONE);
        assert_eq!(caps, POTENTIAL);
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
        assert_eq!(
            add(env, &tag_objects),
            ERR_NOT_AVAILABLE,
            "not delivered, not granted"
        );
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
        assert_eq!(
            set_mode(env, 1, 73, 0),
            ERR_NOT_AVAILABLE,
            "MonitorWait is not delivered"
        );
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
            shared.debug.jvmti_env.lock().native_envs.is_empty(),
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
            shared.debug.jvmti_env.lock().native_envs.is_empty(),
            "not bound to the VM the thread reaches"
        );
        assert!(!shared.debug.debugger_gates.native_class_prepare_armed());

        // `Vm::new`: bound, in the start phase.
        shared.debug.jvmti_env.lock().startup_native_envs = envs;
        bind_startup_envs(&shared);
        assert_eq!(VM_INITS.load(Ordering::SeqCst), 0, "no VMInit before live");
        assert!(shared.debug.debugger_gates.jvmti_start_phase());
        assert_eq!(
            shared.debug.jvmti_env.lock().native_envs,
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
            assert_eq!(add(env, &POTENTIAL), ERR_NONE);
            let callbacks = callbacks_with(&[(EVENT_EXCEPTION, callback)]);
            let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
            assert_eq!(
                set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
                ERR_NONE
            );
            envs.push(env);
        }
        let (a, b) = (envs[0], envs[1]);

        // A enables `Exception`; B enables only `Breakpoint` (so it has a
        // delivery env on the manager too).
        assert_eq!(set_mode(a, 1, EVENT_EXCEPTION, 0), ERR_NONE);
        assert_eq!(set_mode(b, 1, EVENT_BREAKPOINT, 0), ERR_NONE);
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
        assert!(manager.is_event_enabled(JvmtiEventKind::Breakpoint, Some(0)));
        assert_eq!(dispose(b), ERR_NONE);
        assert!(
            !manager.is_event_enabled(JvmtiEventKind::Breakpoint, Some(0)),
            "B's dispose took its Breakpoint off"
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
            class_sig(env, jni::class_id_to_jclass(generic), &mut sig, &mut generic_out),
            ERR_NONE
        );
        assert_eq!(text(sig).as_deref(), Some("Lcratonvm/test/W41Generic;"));
        assert_eq!(
            text(generic_out).as_deref(),
            Some("<T:Ljava/lang/Object;>Ljava/lang/Object;")
        );
        let (mut sig, mut generic_out) = (std::ptr::null_mut::<u8>(), marker);
        assert_eq!(
            class_sig(env, jni::class_id_to_jclass(plain), &mut sig, &mut generic_out),
            ERR_NONE
        );
        assert_eq!(text(sig).as_deref(), Some("Lcratonvm/test/W41Plain;"));
        assert!(generic_out.is_null(), "no Signature attribute: NULL");

        let (mut name, mut desc, mut generic_out) =
            (std::ptr::null_mut::<u8>(), std::ptr::null_mut::<u8>(), marker);
        assert_eq!(
            method_name(env, generic_jmid, &mut name, &mut desc, &mut generic_out),
            ERR_NONE
        );
        assert_eq!(text(name).as_deref(), Some("jvmtiBpLoop"));
        assert_eq!(text(desc).as_deref(), Some("()I"));
        assert_eq!(text(generic_out).as_deref(), Some("<U:Ljava/lang/Object;>()I"));
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

    /// Wave 17 (proposal `i14-L1-proposal-c-jvmti-table-interpreter-events-through-the-delivery-env`,
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
        assert!(gates.jvmti_method_events_armed(), "MethodExit is still enabled");
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

    /// Wave 17 (proposal `i13-L1-proposal-jvmti-c-table-stack-and-line-functions`,
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
            assert_eq!(add(env, &POTENTIAL), ERR_NONE);
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
    /// (`i14-L1-proposal-c-jvmti-table-interpreter-events-through-the-delivery-env`,
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
        assert!(!gates.method_events_armed(), "the funnel's gate follows the disable");
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
    /// (`i13-L1-proposal-jvmti-c-table-stack-and-line-functions`, stage 2):
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
        let callbacks = callbacks_with(&[(
            EVENT_BREAKPOINT,
            on_breakpoint_stack as *const () as usize,
        )]);
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
        assert_eq!(stack_trace_start(0, 0), Some(0), "an empty stack from the top");
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
                })
                .collect()
        };
        let c = ClassId::new(7);
        // main, a, b: interpreter frames 0..3, one row each.
        let trace = vec![entry("main"), entry("a"), entry("b")];
        let rows = splice_native_rows(&trace, &[3], &[(3, c, 1, 0)]);
        assert_eq!(names(&rows), ["main", "a", "b", "N1"], "called by b: on top");
        let rows = splice_native_rows(&trace, &[usize::MAX], &[(3, c, 1, 0)]);
        assert_eq!(names(&rows), ["main", "a", "b", "N1"], "past the end: on top");
        let rows = splice_native_rows(&trace, &[2], &[(2, c, 1, 0)]);
        assert_eq!(names(&rows), ["main", "a", "N1", "b"], "called by a, b is its upcall");
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
        let trace = vec![entry("main"), entry("compiled"), entry("upcalled"), entry("b")];
        let rows = splice_native_rows(&trace, &[2], &[(1, c, 1, 1)]);
        assert_eq!(
            names(&rows),
            ["main", "compiled", "N1", "upcalled", "b"],
            "above the compiled caller, below the upcall's compiled target"
        );
        // Wave 46 (lane L1): `trace` is the four-row one here; this row
        // expected wave 42's first trace's names since wave 44 renamed it.
        let rows = splice_native_rows(&trace, &[], &[]);
        assert_eq!(names(&rows), ["main", "compiled", "upcalled", "b"], "no native: the trace");
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
            assert_eq!(unsafe { (*ptr).jni_native_frames.clone() }, vec![(0, c, 0x1000, 0)]);
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
        assert_eq!(checked_local(Declared::NoTable, &f, 99, Int), Err(ERR_INVALID_SLOT));
        assert_eq!(checked_local(Declared::NoTable, &f, -1, Int), Err(ERR_INVALID_SLOT));
        assert_eq!(
            std::mem::size_of::<MonitorDepthRow>(),
            16,
            "jvmtiMonitorStackDepthInfo"
        );
    }
}
