#[allow(unused_imports)]
use super::*;


/// Slots in JDK 25's `jvmtiInterface_1_`: function 1 (`reserved1`) through
/// 156 (`SetHeapSamplingInterval`). A property of the header an agent was
/// compiled against, as `jni::JNI_FUNCTION_COUNT` is of `jni.h`.
pub(in crate::jvmti::native_env) const JVMTI_FUNCTION_COUNT: usize = 156;

/// `JVMTI_VERSION` of JDK 25's `jvmti.h` (`GetVersionNumber`).
pub(in crate::jvmti::native_env) const JVMTI_VERSION_25: JInt = 0x3019_0000;

// The `jvmtiError` values this table answers.
pub(in crate::jvmti::native_env) const ERR_NONE: JInt = 0;
pub(in crate::jvmti::native_env) const ERR_INVALID_THREAD: JInt = 10;
pub(in crate::jvmti::native_env) const ERR_THREAD_NOT_SUSPENDED: JInt = 13;
pub(in crate::jvmti::native_env) const ERR_THREAD_SUSPENDED: JInt = 14;
pub(in crate::jvmti::native_env) const ERR_NO_MORE_FRAMES: JInt = 31;
pub(in crate::jvmti::native_env) const ERR_OPAQUE_FRAME: JInt = 32;
pub(in crate::jvmti::native_env) const ERR_TYPE_MISMATCH: JInt = 34;
pub(in crate::jvmti::native_env) const ERR_INVALID_SLOT: JInt = 35;
pub(in crate::jvmti::native_env) const ERR_DUPLICATE: JInt = 40;
pub(in crate::jvmti::native_env) const ERR_THREAD_NOT_ALIVE: JInt = 15;
pub(in crate::jvmti::native_env) const ERR_INVALID_OBJECT: JInt = 20;
pub(in crate::jvmti::native_env) const ERR_INVALID_CLASS: JInt = 21;
pub(in crate::jvmti::native_env) const ERR_INVALID_METHODID: JInt = 23;
/// `JVMTI_ERROR_UNMODIFIABLE_CLASS` (wave 54, `RetransformClasses`).
pub(in crate::jvmti::native_env) const ERR_UNMODIFIABLE_CLASS: JInt = 79;
/// `JVMTI_ERROR_UNSUPPORTED_OPERATION` (wave 58, the CPU time of a virtual
/// thread).
pub(in crate::jvmti::native_env) const ERR_UNSUPPORTED_OPERATION: JInt = 73;
pub(in crate::jvmti::native_env) const ERR_NOT_AVAILABLE: JInt = 98;
pub(in crate::jvmti::native_env) const ERR_MUST_POSSESS_CAPABILITY: JInt = 99;
pub(in crate::jvmti::native_env) const ERR_NULL_POINTER: JInt = 100;
pub(in crate::jvmti::native_env) const ERR_ABSENT_INFORMATION: JInt = 101;
pub(in crate::jvmti::native_env) const ERR_INVALID_EVENT_TYPE: JInt = 102;
pub(in crate::jvmti::native_env) const ERR_ILLEGAL_ARGUMENT: JInt = 103;
pub(in crate::jvmti::native_env) const ERR_OUT_OF_MEMORY: JInt = 110;
/// `JVMTI_ERROR_NATIVE_METHOD` (104; 111 until wave 57, which is
/// `JVMTI_ERROR_ACCESS_DENIED`: MEASURED, `L1W57JvmtiLocalVariableTable`).
pub(in crate::jvmti::native_env) const ERR_NATIVE_METHOD: JInt = 104;
pub(in crate::jvmti::native_env) const ERR_WRONG_PHASE: JInt = 112;
pub(in crate::jvmti::native_env) const ERR_INTERNAL: JInt = 113;
pub(in crate::jvmti::native_env) const ERR_UNATTACHED_THREAD: JInt = 115;
pub(in crate::jvmti::native_env) const ERR_INVALID_ENVIRONMENT: JInt = 116;

/// `sizeof(jvmtiCapabilities)` is 16 bytes: bit fields packed from the low
/// bit of the first `unsigned int`, in declaration order.
pub(in crate::jvmti::native_env) const CAPABILITY_WORDS: usize = 4;
/// `can_get_line_numbers`, the 13th bit field (`GetLineNumberTable`, wave 17).
pub(in crate::jvmti::native_env) const CAN_GET_LINE_NUMBERS: u32 = 1 << 12;
/// `can_generate_single_step_events`, the 17th bit field (wave 22).
pub(in crate::jvmti::native_env) const CAN_GENERATE_SINGLE_STEP_EVENTS: u32 = 1 << 16;
/// `can_generate_exception_events`, the 18th bit field.
pub(in crate::jvmti::native_env) const CAN_GENERATE_EXCEPTION_EVENTS: u32 = 1 << 17;
/// `can_generate_frame_pop_events`, the 19th bit field (interpreter round
/// i1 wave 45, lane L1: `NotifyFramePop` and the `FramePop` event; potential
/// in the live phase too, as on HotSpot 25.0.3, measured).
pub(in crate::jvmti::native_env) const CAN_GENERATE_FRAME_POP_EVENTS: u32 = 1 << 18;
/// `can_generate_breakpoint_events`, the 20th bit field.
pub(in crate::jvmti::native_env) const CAN_GENERATE_BREAKPOINT_EVENTS: u32 = 1 << 19;
/// `can_generate_method_entry_events`, the 25th bit field (wave 17).
pub(in crate::jvmti::native_env) const CAN_GENERATE_METHOD_ENTRY_EVENTS: u32 = 1 << 24;
/// `can_generate_method_exit_events`, the 26th bit field (wave 17).
pub(in crate::jvmti::native_env) const CAN_GENERATE_METHOD_EXIT_EVENTS: u32 = 1 << 25;
/// `can_get_current_contended_monitor`, the 7th bit field (interpreter
/// round i1 wave 44, lane L1: `GetCurrentContendedMonitor`).
pub(in crate::jvmti::native_env) const CAN_GET_CURRENT_CONTENDED_MONITOR: u32 = 1 << 6;
/// `can_get_owned_monitor_info`, the 6th bit field (interpreter round i1
/// wave 51, lane L1: `GetOwnedMonitorInfo`). HotSpot groups it with the
/// three wave-44 monitor and local capabilities among its OnLoad ones
/// (`JvmtiManageCapabilities::init_onload_capabilities`), so it is
/// [`ONLOAD_ONLY`] here too.
pub(in crate::jvmti::native_env) const CAN_GET_OWNED_MONITOR_INFO: u32 = 1 << 5;
/// `can_access_local_variables`, the 15th bit field (wave 44: the
/// `GetLocal*` / `SetLocal*` functions and `GetLocalInstance`).
pub(in crate::jvmti::native_env) const CAN_ACCESS_LOCAL_VARIABLES: u32 = 1 << 14;
/// `can_get_owned_monitor_stack_depth_info`, the 35th bit field: bit 2 of
/// the second word (wave 44: `GetOwnedMonitorStackDepthInfo`).
pub(in crate::jvmti::native_env) const CAN_GET_OWNED_MONITOR_STACK_DEPTH_INFO: u32 = 1 << 2;
/// `can_retransform_classes`, the 38th bit field: bit 5 of the second word
/// (interpreter round i2 wave 54, lane L1: `RetransformClasses` and
/// `ClassFileLoadHook` at a retransformation; potential in the OnLoad and
/// the live phase while `DebuggerGates::c_retransform` is on).
pub(in crate::jvmti::native_env) const CAN_RETRANSFORM_CLASSES: u32 = 1 << 5;
/// `can_redefine_classes`, the 10th bit field (interpreter round i2 wave 57,
/// lane L1: `RedefineClasses`; potential in the OnLoad and the live phase
/// while `DebuggerGates::c_redefine` is on).
pub(in crate::jvmti::native_env) const CAN_REDEFINE_CLASSES: u32 = 1 << 9;
/// `can_generate_all_class_hook_events`, the 27th bit field (interpreter
/// round i2 wave 55, lane L1): potential in the OnLoad and the live phase
/// while `DebuggerGates::c_class_hooks` is on, as HotSpot lists it in both
/// (measured, `L1W54JvmtiRetransform`'s first port run and
/// `L1W55JvmtiPhases`). The hook is offered every class file the load path
/// offers (`runtime::instrument::run_chain_over_bytes`) with or without it;
/// a C instrumentation agent that asks for it in `AddCapabilities` was
/// refused all its capabilities (98) until wave 55.
pub(in crate::jvmti::native_env) const CAN_GENERATE_ALL_CLASS_HOOK_EVENTS: u32 = 1 << 26;
/// `can_pop_frame`, the 9th bit field (interpreter round i2 wave 61, lane
/// L1: `PopFrame`; potential while `DebuggerGates::c_frame_control` is on).
pub(in crate::jvmti::native_env) const CAN_POP_FRAME: u32 = 1 << 8;
/// `can_force_early_return`, the 34th bit field: bit 1 of the second word
/// (wave 61: `ForceEarlyReturn*`; as [`CAN_POP_FRAME`]).
pub(in crate::jvmti::native_env) const CAN_FORCE_EARLY_RETURN: u32 = 1 << 1;
/// `can_suspend`, the 21st bit field (interpreter round i1 wave 45, lane
/// L1: `SuspendThread`, `ResumeThread`, `SuspendThreadList`,
/// `ResumeThreadList`).
pub(in crate::jvmti::native_env) const CAN_SUSPEND: u32 = 1 << 20;
/// `jvmtiCapabilities` bit field `n` (0-based, declaration order) as a
/// capability set.
pub(in crate::jvmti::native_env) const fn cap(n: usize) -> [u32; CAPABILITY_WORDS] {
    let mut set = [0; CAPABILITY_WORDS];
    set[n / 32] = 1 << (n % 32);
    set
}

/// The union of `sets`.
pub(in crate::jvmti::native_env) const fn caps_of(sets: &[[u32; CAPABILITY_WORDS]]) -> [u32; CAPABILITY_WORDS] {
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
pub(in crate::jvmti::native_env) const CAP_TAG_OBJECTS: usize = 0;
pub(in crate::jvmti::native_env) const CAP_FIELD_MODIFICATION_EVENTS: usize = 1;
pub(in crate::jvmti::native_env) const CAP_FIELD_ACCESS_EVENTS: usize = 2;
pub(in crate::jvmti::native_env) const CAP_GET_MONITOR_INFO: usize = 7;
pub(in crate::jvmti::native_env) const CAP_SIGNAL_THREAD: usize = 10;
pub(in crate::jvmti::native_env) const CAP_GET_SOURCE_DEBUG_EXTENSION: usize = 13;
pub(in crate::jvmti::native_env) const CAP_MAINTAIN_ORIGINAL_METHOD_ORDER: usize = 15;
pub(in crate::jvmti::native_env) const CAP_REDEFINE_ANY_CLASS: usize = 21;
pub(in crate::jvmti::native_env) const CAP_COMPILED_METHOD_LOAD_EVENTS: usize = 27;
pub(in crate::jvmti::native_env) const CAP_MONITOR_EVENTS: usize = 28;
pub(in crate::jvmti::native_env) const CAP_VM_OBJECT_ALLOC_EVENTS: usize = 29;
pub(in crate::jvmti::native_env) const CAP_NATIVE_METHOD_BIND_EVENTS: usize = 30;
pub(in crate::jvmti::native_env) const CAP_GARBAGE_COLLECTION_EVENTS: usize = 31;
pub(in crate::jvmti::native_env) const CAP_OBJECT_FREE_EVENTS: usize = 32;
pub(in crate::jvmti::native_env) const CAP_GET_CONSTANT_POOL: usize = 35;
pub(in crate::jvmti::native_env) const CAP_SET_NATIVE_METHOD_PREFIX: usize = 36;
pub(in crate::jvmti::native_env) const CAP_RETRANSFORM_ANY_CLASS: usize = 38;
pub(in crate::jvmti::native_env) const CAP_RESOURCE_EXHAUSTION_HEAP_EVENTS: usize = 39;
pub(in crate::jvmti::native_env) const CAP_RESOURCE_EXHAUSTION_THREADS_EVENTS: usize = 40;
pub(in crate::jvmti::native_env) const CAP_EARLY_VMSTART: usize = 41;
pub(in crate::jvmti::native_env) const CAP_EARLY_CLASS_HOOK_EVENTS: usize = 42;
pub(in crate::jvmti::native_env) const CAP_SAMPLED_OBJECT_ALLOC_EVENTS: usize = 43;
pub(in crate::jvmti::native_env) const CAP_SUPPORT_VIRTUAL_THREADS: usize = 44;

/// Does `set` hold capability bit field `n`?
pub(in crate::jvmti::native_env) fn has_cap(set: &[u32; CAPABILITY_WORDS], n: usize) -> bool {
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
pub(in crate::jvmti::native_env) const ALWAYS: [u32; CAPABILITY_WORDS] = caps_of(&[
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
pub(in crate::jvmti::native_env) const ALWAYS_SOLO: [u32; CAPABILITY_WORDS] =
    caps_of(&[[CAN_SUSPEND, 0, 0, 0], cap(CAP_SAMPLED_OBJECT_ALLOC_EVENTS)]);
/// See [`ALWAYS`].
pub(in crate::jvmti::native_env) const ONLOAD: [u32; CAPABILITY_WORDS] = caps_of(&[
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
pub(in crate::jvmti::native_env) const ONLOAD_SOLO: [u32; CAPABILITY_WORDS] = caps_of(&[
    cap(CAP_FIELD_MODIFICATION_EVENTS),
    cap(CAP_FIELD_ACCESS_EVENTS),
    [CAN_GENERATE_BREAKPOINT_EVENTS, 0, 0, 0],
]);
/// The capabilities one env at a time may hold.
pub(in crate::jvmti::native_env) const SOLO: [u32; CAPABILITY_WORDS] = caps_of(&[ALWAYS_SOLO, ONLOAD_SOLO]);
/// What a startup env may acquire that the live phase then offers: the
/// OnLoad sets (wave 46 measured three of them; wave 63 the rest).
pub(in crate::jvmti::native_env) const ONLOAD_ONLY: [u32; CAPABILITY_WORDS] = caps_of(&[ONLOAD, ONLOAD_SOLO]);

// The `jvmtiThreadState` bits `GetThreadState` answers (wave 45).
pub(in crate::jvmti::native_env) const THREAD_STATE_ALIVE: JInt = 0x0001;
pub(in crate::jvmti::native_env) const THREAD_STATE_TERMINATED: JInt = 0x0002;
pub(in crate::jvmti::native_env) const THREAD_STATE_RUNNABLE: JInt = 0x0004;
pub(in crate::jvmti::native_env) const THREAD_STATE_WAITING_INDEFINITELY: JInt = 0x0010;
pub(in crate::jvmti::native_env) const THREAD_STATE_WAITING_WITH_TIMEOUT: JInt = 0x0020;
pub(in crate::jvmti::native_env) const THREAD_STATE_WAITING: JInt = 0x0080;
pub(in crate::jvmti::native_env) const THREAD_STATE_SLEEPING: JInt = 0x0040;
pub(in crate::jvmti::native_env) const THREAD_STATE_IN_OBJECT_WAIT: JInt = 0x0100;
pub(in crate::jvmti::native_env) const THREAD_STATE_PARKED: JInt = 0x0200;
pub(in crate::jvmti::native_env) const THREAD_STATE_BLOCKED_ON_MONITOR_ENTER: JInt = 0x0400;
pub(in crate::jvmti::native_env) const THREAD_STATE_SUSPENDED: JInt = 0x0010_0000;
pub(in crate::jvmti::native_env) const THREAD_STATE_INTERRUPTED: JInt = 0x0020_0000;
pub(in crate::jvmti::native_env) const THREAD_STATE_IN_NATIVE: JInt = 0x0040_0000;

/// Everything this table can grant: every set of [`ALWAYS`]'s model (what
/// a lone env is offered in the OnLoad phase).
pub(in crate::jvmti::native_env) const POTENTIAL: [u32; CAPABILITY_WORDS] = caps_of(&[ALWAYS, ALWAYS_SOLO, ONLOAD, ONLOAD_SOLO]);

/// `JVM_RECOGNIZED_METHOD_MODIFIERS`: what `GetMethodModifiers` answers of a
/// method's access flags (public through synthetic, without the bits the
/// class-file format reserves).
pub(in crate::jvmti::native_env) const RECOGNIZED_METHOD_MODIFIERS: u16 = 0x1DFF;
/// `JVMTI_CLASS_STATUS_ARRAY` / `JVMTI_CLASS_STATUS_PRIMITIVE`; the other
/// `jvmtiClassStatus` bits are JDWP's (`debug::jdwp_class_status`).
pub(in crate::jvmti::native_env) const CLASS_STATUS_ARRAY: JInt = 16;
pub(in crate::jvmti::native_env) const CLASS_STATUS_PRIMITIVE: JInt = 32;

/// `jvmtiLineNumberEntry`: `{ jlocation start_location; jint line_number; }`,
/// 16 bytes with its padding.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jvmti::native_env) struct LineNumberRow {
    pub(in crate::jvmti::native_env) start_location: i64,
    pub(in crate::jvmti::native_env) line_number: JInt,
}

/// `jvmtiLocalVariableEntry`: `{ jlocation start_location; jint length;
/// char* name; char* signature; char* generic_signature; jint slot; }`, 48
/// bytes with its padding (interpreter round i2 wave 57).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jvmti::native_env) struct LocalVariableRow {
    pub(in crate::jvmti::native_env) start_location: i64,
    pub(in crate::jvmti::native_env) length: JInt,
    pub(in crate::jvmti::native_env) name: *mut u8,
    pub(in crate::jvmti::native_env) signature: *mut u8,
    pub(in crate::jvmti::native_env) generic_signature: *mut u8,
    pub(in crate::jvmti::native_env) slot: JInt,
}

/// `jvmtiFrameInfo`: `{ jmethodID method; jlocation location; }`, 16 bytes
/// (wave 22).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jvmti::native_env) struct FrameInfo {
    pub(in crate::jvmti::native_env) method: u64,
    pub(in crate::jvmti::native_env) location: i64,
}

/// `JVMTI_MIN_EVENT_TYPE_VAL` .. `JVMTI_MAX_EVENT_TYPE_VAL` of JDK 25
/// (`VirtualThreadEnd` is the last).
pub(in crate::jvmti::native_env) const MIN_EVENT: JInt = 50;
pub(in crate::jvmti::native_env) const MAX_EVENT: JInt = 88;
/// HotSpot's first extension event number (`EXT_MIN_EVENT_TYPE_VAL`: the
/// three below [`MIN_EVENT`], `VirtualThreadUnmount`, `VirtualThreadMount`,
/// `ClassUnload`), the low end of what `SetEventNotificationMode` accepts
/// (interpreter round i2 wave 61, lane L1; MEASURED).
pub(in crate::jvmti::native_env) const MIN_EXTENSION_EVENT: JInt = 47;
/// The event numbers `jvmti.h` leaves reserved in the range (wave 61).
pub(in crate::jvmti::native_env) const RESERVED_EVENTS: [JInt; 5] = [72, 77, 78, 79, 85];
// The `jvmtiEvent` numbers of the events this table delivers (`jvmti.h`).
// Until wave 13 `Breakpoint` was taken as 60 — `SingleStep` — and its
// callback read from `SingleStep`'s member of `jvmtiEventCallbacks`, so an
// agent compiled against `jvmti.h` could not enable it.
pub(in crate::jvmti::native_env) const EVENT_VM_INIT: JInt = 50;
pub(in crate::jvmti::native_env) const EVENT_VM_DEATH: JInt = 51;
/// `ThreadStart` / `ThreadEnd` (interpreter round i1 wave 52, lane L1).
pub(in crate::jvmti::native_env) const EVENT_THREAD_START: JInt = 52;
pub(in crate::jvmti::native_env) const EVENT_THREAD_END: JInt = 53;
/// `ClassFileLoadHook` / `ClassLoad` (interpreter round i2 wave 53, lane L1,
/// behind `DebuggerGates::c_class_hooks`, `CRATONVM_JVMTI_C_CLASS_HOOKS`).
pub(in crate::jvmti::native_env) const EVENT_CLASS_FILE_LOAD_HOOK: JInt = 54;
pub(in crate::jvmti::native_env) const EVENT_CLASS_LOAD: JInt = 55;
pub(in crate::jvmti::native_env) const EVENT_CLASS_PREPARE: JInt = 56;
/// `VMStart` (interpreter round i2 wave 55, lane L1, behind
/// `DebuggerGates::c_jvmti_phases`, `CRATONVM_JVMTI_C_PHASES`).
pub(in crate::jvmti::native_env) const EVENT_VM_START: JInt = 57;
pub(in crate::jvmti::native_env) const EVENT_EXCEPTION: JInt = 58;
/// Interpreter round i1 wave 52, lane L1 (behind
/// `DebuggerGates::c_exception_catch`, `CRATONVM_JVMTI_C_EXCEPTION_CATCH`).
pub(in crate::jvmti::native_env) const EVENT_EXCEPTION_CATCH: JInt = 59;
pub(in crate::jvmti::native_env) const EVENT_SINGLE_STEP: JInt = 60;
pub(in crate::jvmti::native_env) const EVENT_FRAME_POP: JInt = 61;
pub(in crate::jvmti::native_env) const EVENT_BREAKPOINT: JInt = 62;
pub(in crate::jvmti::native_env) const EVENT_METHOD_ENTRY: JInt = 65;
pub(in crate::jvmti::native_env) const EVENT_METHOD_EXIT: JInt = 66;
/// The events delivered through the env's delivery env on the VM's manager
/// ([`JvmtiNativeEnv::sync_event`]), each with the capability it needs.
pub(in crate::jvmti::native_env) const MANAGER_EVENTS: [(JInt, u32); 7] = [
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
pub(in crate::jvmti::native_env) const CALLBACK_SLOTS: usize = 48;

/// `void (JNICALL *VMInit)(jvmtiEnv*, JNIEnv*, jthread)`.
pub(in crate::jvmti::native_env) type VmInitCallback = unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject);
/// `void (JNICALL *VMDeath)(jvmtiEnv*, JNIEnv*)`.
pub(in crate::jvmti::native_env) type VmDeathCallback = unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv);
/// `void (JNICALL *ClassPrepare)(jvmtiEnv*, JNIEnv*, jthread, jclass)`.
pub(in crate::jvmti::native_env) type ClassPrepareCallback = unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject, JObject);
/// `void (JNICALL *Exception)(jvmtiEnv*, JNIEnv*, jthread, jmethodID,
/// jlocation, jobject exception, jmethodID catch_method, jlocation
/// catch_location)`.
pub(in crate::jvmti::native_env) type ExceptionCallback =
    unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject, u64, i64, JObject, u64, i64);
/// `void (JNICALL *ExceptionCatch)(jvmtiEnv*, JNIEnv*, jthread, jmethodID,
/// jlocation, jobject exception)` (wave 52).
pub(in crate::jvmti::native_env) type ExceptionCatchCallback =
    unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject, u64, i64, JObject);
/// `void (JNICALL *Breakpoint)(jvmtiEnv*, JNIEnv*, jthread, jmethodID,
/// jlocation)`.
pub(in crate::jvmti::native_env) type BreakpointCallback = unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject, u64, i64);
/// `void (JNICALL *MethodEntry)(jvmtiEnv*, JNIEnv*, jthread, jmethodID)`.
pub(in crate::jvmti::native_env) type MethodEntryCallback = unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject, u64);
/// `void (JNICALL *MethodExit)(jvmtiEnv*, JNIEnv*, jthread, jmethodID,
/// jboolean was_popped_by_exception, jvalue return_value)`. The 8-byte
/// `jvalue` union goes in an integer register under both 64-bit C calling
/// conventions (SysV merges its integer and floating members to INTEGER;
/// Windows passes an 8-byte aggregate by value in a register), so it is a
/// `u64` here.
pub(in crate::jvmti::native_env) type MethodExitCallback = unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject, u64, u8, u64);
/// `void (JNICALL *FramePop)(jvmtiEnv*, JNIEnv*, jthread, jmethodID,
/// jboolean was_popped_by_exception)` (wave 45).
pub(in crate::jvmti::native_env) type FramePopCallback = unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject, u64, u8);

/// A native agent's `jvmtiEnv`. `#[repr(C)]` with the table pointer first:
/// an agent calls `(*env)->SetBreakpoint(env, ...)`.
#[repr(C)]
pub(crate) struct JvmtiNativeEnv {
    pub(in crate::jvmti::native_env) functions: *const usize,
    pub(in crate::jvmti::native_env) state: parking_lot::Mutex<EnvState>,
    /// Serialises mirroring the env's `Breakpoint` / `Exception` state into
    /// its delivery env ([`JvmtiNativeEnv::sync_event`]) and disposing it,
    /// so two agent threads cannot publish their changes out of order. Never
    /// taken by a delivery.
    pub(in crate::jvmti::native_env) sync: parking_lot::Mutex<()>,
}

/// Everything an env holds, under one lock (agents may call from any thread).
pub(in crate::jvmti::native_env) struct EnvState {
    /// The capabilities this env possesses.
    pub(in crate::jvmti::native_env) capabilities: [u32; CAPABILITY_WORDS],
    /// The agent's `jvmtiEventCallbacks`, copied (`SetEventCallbacks`).
    pub(in crate::jvmti::native_env) callbacks: [usize; CALLBACK_SLOTS],
    /// Bit `event - MIN_EVENT` of every event enabled for all threads.
    pub(in crate::jvmti::native_env) enabled: u64,
    /// `(event, thread id)` of every event enabled for one thread.
    pub(in crate::jvmti::native_env) thread_enabled: Vec<(JInt, u64)>,
    /// The VM this env acts on, once one was in hand. Never replaced: an env
    /// whose VM is gone acts on none.
    pub(in crate::jvmti::native_env) vm: Option<Weak<SharedVm>>,
    /// Obtained by a startup agent in `Agent_OnLoad` and not bound yet: it
    /// acts on the VM being built, which `Vm::new` binds it to
    /// ([`bind_startup_envs`]), never on another VM the thread can reach.
    pub(in crate::jvmti::native_env) startup_pending: bool,
    /// The delivery registered on that VM's manager.
    pub(in crate::jvmti::native_env) delivery: Option<(Arc<rt::JvmtiEventManager>, Arc<rt::JvmtiEnv>)>,
    /// The breakpoints this env set, cleared when it is disposed.
    pub(in crate::jvmti::native_env) breakpoints: Vec<(u64, i64)>,
    /// `SetEnvironmentLocalStorage`'s pointer (wave 52).
    pub(in crate::jvmti::native_env) local_storage: usize,
    /// `SetThreadLocalStorage`'s pointers, by VM thread id (wave 52).
    pub(in crate::jvmti::native_env) thread_storage: Vec<(u64, usize)>,
    /// `SetExtensionEventCallback`'s callbacks of the extension events 47-49
    /// (interpreter round i2 wave 63, lane L1; `extensions`).
    pub(in crate::jvmti::native_env) ext_callbacks: [usize; 3],
    /// `SetNativeMethodPrefix(es)`'s prefixes, in the agent's order (wave
    /// 63).
    pub(in crate::jvmti::native_env) native_prefixes: Vec<String>,
    /// The tags this env set on objects (wave 63; `heap`).
    pub(in crate::jvmti::native_env) tags: heap::TagTable,
    /// zt t23/x19 (review page item 28e): a startup env's VM's effective
    /// class-unloading bit, copied from its OnLoad scope
    /// ([`StartupScope::class_unloading`]); what `IsClassUnloadingEnabled`
    /// answers before the env has a VM. `None` for a live-phase env.
    pub(in crate::jvmti::native_env) onload_class_unloading: Option<bool>,
    pub(in crate::jvmti::native_env) disposed: bool,
}

impl EnvState {
    /// Is `event` enabled for thread `tid` (globally or for that thread)?
    pub(in crate::jvmti::native_env) fn enabled_for(&self, event: JInt, tid: u64) -> bool {
        self.enabled & event_bit(event) != 0 || self.thread_enabled.contains(&(event, tid))
    }

    /// Is `event` enabled for any thread?
    pub(in crate::jvmti::native_env) fn enabled_anywhere(&self, event: JInt) -> bool {
        self.enabled & event_bit(event) != 0 || self.thread_enabled.iter().any(|&(e, _)| e == event)
    }

    /// Disable `event` globally and for every thread.
    pub(in crate::jvmti::native_env) fn disable_everywhere(&mut self, event: JInt) {
        self.enabled &= !event_bit(event);
        self.thread_enabled.retain(|&(e, _)| e != event);
    }
}

/// The bit of `event` in [`EnvState::enabled`]; 0 outside the event range.
pub(in crate::jvmti::native_env) fn event_bit(event: JInt) -> u64 {
    // Wave 63: from the extension events (47) on, so they enable too.
    u32::try_from(event - MIN_EXTENSION_EVENT)
        .ok()
        .and_then(|shift| 1u64.checked_shl(shift))
        .unwrap_or(0)
}

/// The capability an event this table delivers needs (0: none), or `None`
/// for an event it does not deliver.
pub(in crate::jvmti::native_env) fn required_capability(event: JInt) -> Option<u32> {
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
pub(in crate::jvmti::native_env) fn event_capability_set(event: JInt) -> Option<[u32; CAPABILITY_WORDS]> {
    required_capability(event)
        .map(|word0| [word0, 0, 0, 0])
        .or_else(|| events_more::event_capability(event))
}

thread_local! {
    /// While a VM loads its startup agents on this thread
    /// ([`collect_startup_envs`]), the envs their `GetEnv` calls hand out,
    /// and (wave 61) the VM properties of the OnLoad phase and what the
    /// agents did to the VM being built.
    pub(in crate::jvmti::native_env) static STARTUP_ENVS: RefCell<Option<StartupScope>> = const { RefCell::new(None) };
}

/// The OnLoad phase of a VM being built ([`STARTUP_ENVS`]).
#[derive(Default)]
pub(in crate::jvmti::native_env) struct StartupScope {
    pub(in crate::jvmti::native_env) envs: Vec<usize>,
    /// The VM properties `GetSystemProperty` answers in `Agent_OnLoad`
    /// (interpreter round i2 wave 61, lane L1; [`onload_vm_properties`]),
    /// each with whether `SetSystemProperty` may write it.
    pub(in crate::jvmti::native_env) properties: Vec<(String, String, bool)>,
    pub(in crate::jvmti::native_env) effects: OnLoadEffects,
    /// zt t23/x19 (review page item 28e): the VM's effective
    /// `-XX:±ClassUnloading` (`VmConfig::class_unloading` under
    /// `CRATONVM_CLASS_UNLOADING_OFF_KEEPS_CLASSES`), noted by the VM being
    /// built before its agents load ([`note_onload_class_unloading`]), so
    /// `IsClassUnloadingEnabled` answers it in `Agent_OnLoad`, before the VM
    /// exists. `None`: not noted (a test scope).
    pub(in crate::jvmti::native_env) class_unloading: Option<bool>,
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
pub(in crate::jvmti::native_env) fn function_table() -> *const usize {
    static TABLE: std::sync::OnceLock<Box<[usize; JVMTI_FUNCTION_COUNT]>> =
        std::sync::OnceLock::new();
    TABLE.get_or_init(build_function_table).as_ptr()
}

pub(in crate::jvmti::native_env) fn build_function_table() -> Box<[usize; JVMTI_FUNCTION_COUNT]> {
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
pub(in crate::jvmti::native_env) extern "C" fn not_available(_env: *mut JvmtiNativeEnv) -> JInt {
    ERR_NOT_AVAILABLE
}

/// The env at `addr`, an address [`new_env_for_version`] handed out.
///
/// # Safety
///
/// `addr` is such an address; envs are never freed.
pub(in crate::jvmti::native_env) unsafe fn env_at<'a>(addr: usize) -> &'a JvmtiNativeEnv {
    &*(addr as *const JvmtiNativeEnv) // Cast: the address back to the env it names
}

/// The env behind a `jvmtiEnv*`, when it is one this table handed out and
/// not disposed; else the error to answer.
pub(in crate::jvmti::native_env) fn live_env<'a>(env: *mut JvmtiNativeEnv) -> Result<&'a JvmtiNativeEnv, JInt> {
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
    pub(in crate::jvmti::native_env) fn addr(&self) -> usize {
        self as *const JvmtiNativeEnv as usize // Cast: the env's address, its registry key
    }

    /// The VM a call acts on: the one this env is bound to (while it lives),
    /// else the calling thread's, which it is bound to from now on. `None`
    /// before the VM exists (a startup agent's `OnLoad` phase) and after it
    /// is gone, which the live-phase functions answer with
    /// `JVMTI_ERROR_WRONG_PHASE`.
    pub(in crate::jvmti::native_env) fn vm(&self) -> Option<Arc<SharedVm>> {
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
    pub(in crate::jvmti::native_env) fn onload_class_unloading(&self) -> Option<bool> {
        self.state.lock().onload_class_unloading
    }

    pub(in crate::jvmti::native_env) fn possesses(&self, capability: u32) -> bool {
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
    pub(in crate::jvmti::native_env) fn potential(&self) -> [u32; CAPABILITY_WORDS] {
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
    pub(in crate::jvmti::native_env) fn solos_held_by_others(&self, shared: Option<&SharedVm>) -> [u32; CAPABILITY_WORDS] {
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
    pub(in crate::jvmti::native_env) fn can_retransform(&self) -> bool {
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
    pub(in crate::jvmti::native_env) fn outside_live_phase(&self) -> bool {
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
    pub(in crate::jvmti::native_env) fn start_or_live_vm(&self) -> Option<Arc<SharedVm>> {
        self.vm().filter(|shared| {
            !matches!(
                shared.debug.debugger_gates.jvmti_phase(),
                crate::debug::JVMTI_PHASE_PRIMORDIAL | crate::debug::JVMTI_PHASE_DEAD
            )
        })
    }

    /// Bind this env to `shared`, once: listed as one of its native envs,
    /// and the events enabled before it had a VM (`Agent_OnLoad`) armed.
    pub(in crate::jvmti::native_env) fn attach_to(&self, shared: &Arc<SharedVm>) {
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
    pub(in crate::jvmti::native_env) fn sync_event(&self, shared: &Arc<SharedVm>, event: JInt) -> JInt {
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
    pub(in crate::jvmti::native_env) fn bind_delivery(&self, shared: &Arc<SharedVm>) -> Result<Arc<rt::JvmtiEnv>, JInt> {
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
pub(in crate::jvmti::native_env) fn onload_acquired_of(shared: &SharedVm) -> [u32; CAPABILITY_WORDS] {
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
pub(in crate::jvmti::native_env) fn refresh_class_prepare_gate(shared: &SharedVm) {
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
pub(in crate::jvmti::native_env) fn refresh_method_events_gate(shared: &SharedVm) {
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
pub(in crate::jvmti::native_env) fn callback_for(env_addr: usize, event: JInt, tid: u64) -> Option<usize> {
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
pub(in crate::jvmti::native_env) fn in_event_context(shared: &SharedVm, tid: u64, call: impl FnOnce(JNIEnv, JObject)) {
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
pub(in crate::jvmti::native_env) fn event_thread_to_install(own: Option<usize>, installed: Option<usize>) -> Option<usize> {
    own.filter(|&addr| installed != Some(addr))
}

/// The `Breakpoint` event for the env at `env_addr`, on the executing
/// thread (`JvmtiEventManager::fire_breakpoint`).
pub(in crate::jvmti::native_env) fn deliver_breakpoint(env_addr: usize, vm: &Weak<SharedVm>, tid: u64, method: u64, location: i64) {
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
pub(in crate::jvmti::native_env) fn deliver_single_step(env_addr: usize, vm: &Weak<SharedVm>, tid: u64, method: u64, location: i64) {
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
pub(in crate::jvmti::native_env) fn deliver_exception(
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
pub(in crate::jvmti::native_env) fn deliver_exception_catch(
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
pub(in crate::jvmti::native_env) fn deliver_method_entry(env_addr: usize, vm: &Weak<SharedVm>, tid: u64, method: u64) {
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
pub(in crate::jvmti::native_env) fn deliver_frame_pop(env_addr: usize, vm: &Weak<SharedVm>, tid: u64, method: u64, popped: bool) {
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
pub(in crate::jvmti::native_env) fn return_jvalue(value: &rt::LocalValue, handle: JObject) -> u64 {
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
pub(in crate::jvmti::native_env) fn deliver_method_exit(
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
            return_jvalue(value, handle)        };
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
pub(in crate::jvmti::native_env) type ThreadEventCallback = unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject);

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
/// (default on, `runtime_flag_on`). On, the C table's `ClassLoad` is posted
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
    true
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
    false}

/// gci i12/ev: is the pointer installed as this thread's JNI binding
/// (`installed`) its own published `JvmThread` (`own`), i.e. installed by a
/// poster or a native funnel from the live `&mut`?
pub(in crate::jvmti::native_env) fn own_handle_installed(own: Option<usize>, installed: Option<usize>) -> bool {
    own.is_some() && installed == own
}

/// gci i12/ev: does a `ClassLoad` posted on a published thread whose own
/// handle is not installed (`own`, `installed`) wait for a flush point?
/// Only while the switch is on (`switch_on`, asked last).
pub(in crate::jvmti::native_env) fn class_load_deferred(
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
pub(in crate::jvmti::native_env) fn take_deferred_class_loads(queue: &mut Vec<(u64, ClassId)>, tid: u64) -> Vec<ClassId> {
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
pub(in crate::jvmti::native_env) type ClassFileLoadHookCallback = unsafe extern "C" fn(
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
pub(in crate::jvmti::native_env) struct HookObjects<'a> {
    pub(in crate::jvmti::native_env) shared: &'a SharedVm,
    pub(in crate::jvmti::native_env) handles: [JObject; 3],
}

impl<'a> HookObjects<'a> {
    pub(in crate::jvmti::native_env) fn new(shared: &'a SharedVm, objects: [Option<ObjectRef>; 3]) -> Self {
        let mut globals = shared.natives.jni_global_refs.lock();
        let handles = objects.map(|obj| obj.map_or(0, |obj| globals.add(obj)));
        drop(globals);
        Self { shared, handles }
    }

    /// Local references of the three objects' current addresses, in the
    /// callback's local frame (NULL for an absent one).
    pub(in crate::jvmti::native_env) fn locals(&self) -> [JObject; 3] {
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
pub(in crate::jvmti::native_env) fn note_onload_local_access(shared: &SharedVm, acquired: &[u32; CAPABILITY_WORDS]) {
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
pub(in crate::jvmti::native_env) fn onload_acquired_observes_monitors(acquired: &[u32; CAPABILITY_WORDS]) -> bool {
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
pub(in crate::jvmti::native_env) fn replay_early_class_hooks(shared: &Arc<SharedVm>) {
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
                    .map(|b| (c.id, Arc::clone(&c.name), b))            })
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
pub(in crate::jvmti::native_env) fn post_vm_init(shared: &Arc<SharedVm>, tid: u64) {
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
pub(in crate::jvmti::native_env) fn thread_id_of(shared: &SharedVm, thread: JObject) -> Result<u64, JInt> {
    match lookup_thread(shared, thread)? {
        ThreadLookup::Live(tid) => Ok(tid),
        ThreadLookup::Dead | ThreadLookup::Unstarted => Err(ERR_THREAD_NOT_ALIVE),
    }
}

/// What a `jthread` handle names ([`lookup_thread`], interpreter round i1
/// wave 45, lane L1): `GetThreadState` tells a terminated thread from one
/// never started.
pub(in crate::jvmti::native_env) enum ThreadLookup {
    /// A live VM thread, by its id.
    Live(u64),
    /// A thread the registry knows as terminated.
    Dead,
    /// A `java.lang.Thread` the registry does not know: not started.
    Unstarted,
}

/// [`thread_id_of`]'s lookup, with the three answers apart;
/// `JVMTI_ERROR_INVALID_THREAD` for a handle that names no `Thread`.
pub(in crate::jvmti::native_env) fn lookup_thread(shared: &SharedVm, thread: JObject) -> Result<ThreadLookup, JInt> {
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
pub(in crate::jvmti::native_env) fn is_thread_object(shared: &SharedVm, obj: ObjectRef) -> bool {
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
pub(in crate::jvmti::native_env) fn class_of(shared: &SharedVm, klass: JObject) -> Option<ClassId> {
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
pub(in crate::jvmti::native_env) extern "C" fn set_event_notification_mode(
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
pub(in crate::jvmti::native_env) extern "C" fn set_event_callbacks(
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
pub(in crate::jvmti::native_env) extern "C" fn set_breakpoint(env: *mut JvmtiNativeEnv, method: u64, location: i64) -> JInt {
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
pub(in crate::jvmti::native_env) extern "C" fn clear_breakpoint(env: *mut JvmtiNativeEnv, method: u64, location: i64) -> JInt {
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
pub(in crate::jvmti::native_env) const BLOCK_HEADER: usize = 16;

pub(in crate::jvmti::native_env) fn allocate_block(size: usize) -> *mut u8 {
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
pub(in crate::jvmti::native_env) unsafe fn free_block(mem: *mut u8) {
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
pub(in crate::jvmti::native_env) fn allocate_modified_utf8(text: &str) -> *mut u8 {
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
pub(in crate::jvmti::native_env) fn modified_utf8(text: &str) -> Vec<u8> {
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
pub(in crate::jvmti::native_env) extern "C" fn allocate(env: *mut JvmtiNativeEnv, size: i64, mem_ptr: *mut *mut u8) -> JInt {
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
pub(in crate::jvmti::native_env) extern "C" fn deallocate(env: *mut JvmtiNativeEnv, mem: *mut u8) -> JInt {
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
pub(in crate::jvmti::native_env) fn class_signature(name: &str) -> String {
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
pub(in crate::jvmti::native_env) extern "C" fn get_class_signature(
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
pub(in crate::jvmti::native_env) fn write_modified_utf8_outs(out: &[(*mut *mut u8, Option<&str>)]) -> bool {
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
pub(in crate::jvmti::native_env) fn method_generic_signature(shared: &SharedVm, method: u64) -> Option<String> {
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
pub(in crate::jvmti::native_env) extern "C" fn get_class_methods(
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
            return write_id_array::<u64>(method_count_ptr, methods_ptr, &[]);        }
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
pub(in crate::jvmti::native_env) fn method_of(shared: &SharedVm, method: u64) -> Option<(ClassId, Arc<str>, Arc<str>)> {
    with_method(shared, method, |class_id, m| {
        (class_id, Arc::clone(&m.name), Arc::clone(&m.descriptor))
    })
}

/// [`method_of`], or (wave 56) the lambda proxy method a `jmethodID` names.
pub(in crate::jvmti::native_env) fn method_or_proxy_method(shared: &SharedVm, method: u64) -> Option<(ClassId, Arc<str>, Arc<str>)> {
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
pub(in crate::jvmti::native_env) fn with_method<R>(
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
pub(in crate::jvmti::native_env) extern "C" fn get_method_name(
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
        method_generic_signature(&shared, method)    };
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
pub(in crate::jvmti::native_env) extern "C" fn get_method_declaring_class(
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
pub(in crate::jvmti::native_env) fn class_status(class: &crate::classloading::Class) -> JInt {
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
pub(in crate::jvmti::native_env) extern "C" fn get_class_status(
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
pub(in crate::jvmti::native_env) extern "C" fn get_method_modifiers(
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
        JInt::from(m.access_flags & RECOGNIZED_METHOD_MODIFIERS)    }) else {
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
pub(in crate::jvmti::native_env) extern "C" fn is_method_native(
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
pub(in crate::jvmti::native_env) extern "C" fn get_method_location(
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
pub(in crate::jvmti::native_env) fn write_method_location(
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
pub(in crate::jvmti::native_env) fn proxy_method_code(shared: &SharedVm, method: u64) -> Option<Result<Vec<u8>, JInt>> {
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
pub(in crate::jvmti::native_env) extern "C" fn get_line_number_table(
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
            return ERR_ABSENT_INFORMATION;        }
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
pub(in crate::jvmti::native_env) struct LocalVariableFacts {
    pub(in crate::jvmti::native_env) start: u16,
    pub(in crate::jvmti::native_env) length: u16,
    pub(in crate::jvmti::native_env) slot: u16,
    pub(in crate::jvmti::native_env) name: String,
    pub(in crate::jvmti::native_env) signature: String,
    pub(in crate::jvmti::native_env) generic: Option<String>,
}

/// The `LocalVariableTable` rows of the method `method` names, in class-file
/// order, each with the generic signature of the `LocalVariableTypeTable`
/// row of the same slot and start (JDWP's `Method.VariableTableWithGeneric`
/// pairs them the same way); `Err` with the code to answer: 111 for a native
/// method, 101 for one without a table (javac without `-g`, an abstract
/// method), 23 for an id that names no method.
pub(in crate::jvmti::native_env) fn local_variable_facts(shared: &SharedVm, method: u64) -> Result<Vec<LocalVariableFacts>, JInt> {
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
pub(in crate::jvmti::native_env) extern "C" fn get_local_variable_table(
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
        allocate_block(bytes)    };
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
pub(in crate::jvmti::native_env) fn frames_of(shared: &SharedVm, thread: JObject) -> Result<Vec<FrameInfo>, JInt> {
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
pub(in crate::jvmti::native_env) struct ListedRow {
    pub(in crate::jvmti::native_env) info: FrameInfo,
    pub(in crate::jvmti::native_env) frame: Option<usize>,
    /// A lambda proxy method's row: its locals record
    /// (`JvmThread::lambda_proxy_locals`; the i54-L1 retirement).
    pub(in crate::jvmti::native_env) proxy_locals: Option<usize>,
    /// JIT round 19 proposal wave 7, lane gc7 (SYNC619-3): a trace row's index
    /// in the trace the listing was built from (`FrameRow::Java`), `None` for
    /// every other row and for a stale interpreter frame's row. Read only by
    /// the calling thread's `GetOwnedMonitorStackDepthInfo`, which attributes
    /// monitors by trace index when compiled activations are on its stack.
    /// Interpreter round i6 wave 120 follow-up (lane L1): the shared listing
    /// finds it from the row's interpreter frame or compiled activation
    /// (`held_monitors::trace_position_owners`).
    pub(in crate::jvmti::native_env) at: Option<usize>,
    /// j27 wave 5 (lane JVMTIFRAME): a compiled activation's row, or an
    /// OSR'd interpreter frame's, of the shared listing asked with frames:
    /// what [`compiled_locals`] reads its locals by
    /// (`CRATONVM_JVMTI_C_COMPILED_FRAME_LOCALS`). `None` for every other row.
    pub(in crate::jvmti::native_env) home: Option<crate::runtime::interpreter::compiled_frame_locals::HomeRow>,
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
///   `docs/feature-designs/real-jdk-mode.md`;
///   `stackwalker::reflective_entries`, anchored at `JvmThread::reflective_calls`).
///   HotSpot also lists the accessor's `@Hidden` `invokeImpl` and the
///   method-handle frames under it, which this VM does not run.
pub(in crate::jvmti::native_env) fn listed_rows(shared: &SharedVm, current: &JvmThread, with_frames: bool) -> Vec<ListedRow> {
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
pub(in crate::jvmti::native_env) enum FrameRow<'a> {
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

/// What [`splice_rows`] puts between two rows of the trace.
pub(in crate::jvmti::native_env) enum Inserted<'a> {
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
pub(in crate::jvmti::native_env) fn splice_rows<'a>(
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
pub(in crate::jvmti::native_env) fn inferred_proxy_candidates(
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
pub(in crate::jvmti::native_env) fn with_inferred_proxy_rows<'a>(
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
                    );                }
                out.push(FrameRow::Inferred(proxy, i64::from(entry.byte_code_index)));            }        }    }
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
pub(in crate::jvmti::native_env) const ROW_KEY_IS_HASH: u64 = 1 << 63;

/// With [`ROW_KEY_IS_HASH`] clear, the bit of a row's key that marks it a
/// lambda proxy method's frame (interpreter round i2 wave 60, lane L1;
/// [`NativeFrameRow::enter_lambda_proxy`]): the row's class is the proxy,
/// the key's low 16 bits the method's index among
/// `runtime::lambda_proxy::lambda_proxy_methods`. A user-space function
/// address never has bit 62 set either.
pub(in crate::jvmti::native_env) const ROW_KEY_IS_PROXY: u64 = 1 << 62;

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


/// The index, in `class`, of the native method a row's `key` names
/// ([`NativeFrameRow`]); `None` when none does. Read-time work only: a
/// hashed key is compared with each method's [`native_row_method_hash`]; a
/// function pointer with what this VM's JNI table binds each native method
/// of the class to (`jni::find_jni_native`: `RegisterNatives` and the
/// naming-convention lookup, which caches its answer there before the call).
/// Two methods bound to one function are told apart by neither; the first is
/// named.
pub(in crate::jvmti::native_env) fn row_method_index(
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
    pub(in crate::jvmti::native_env) thread: *mut JvmThread,
    pub(in crate::jvmti::native_env) base: usize,
}

