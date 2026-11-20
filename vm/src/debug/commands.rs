// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JDWP command-set handlers.
//!
//! The central entry point is [`dispatch`] which routes an incoming command
//! packet to the appropriate handler based on `(command_set, command)`.

use crate::debug::events::{EventKind, EventModifier, StepDepth, StepSize, SuspendPolicy};
use crate::debug::ids::{FieldId, MethodId, ThreadId};
use crate::debug::protocol::{PayloadReader, PayloadWriter};
use crate::debug::{BridgeError, DebugState, DebuggerValue, InvokeOutcome};

// ---------------------------------------------------------------------------
// JDWP error codes
// ---------------------------------------------------------------------------

pub const ERR_NONE: u16 = 0;
pub const ERR_INVALID_THREAD: u16 = 10;
/// JDWP `INVALID_THREAD_GROUP` (interpreter round i1 wave 24).
pub const ERR_INVALID_THREAD_GROUP: u16 = 11;
pub const ERR_THREAD_NOT_SUSPENDED: u16 = 13;
pub const ERR_INVALID_CLASS: u16 = 21;
/// JDWP `CLASS_NOT_PREPARED` (interpreter round i5 wave 103): what HotSpot's
/// back end answers `ReferenceType.Fields` / `Methods` / `Interfaces` (and
/// their generic forms) of a class loaded but not prepared, from JVMTI
/// `GetClassFields` / `GetClassMethods` / `GetImplementedInterfaces`.
pub const ERR_CLASS_NOT_PREPARED: u16 = 22;
pub const ERR_INVALID_OBJECT: u16 = 20;
pub const ERR_INVALID_METHODID: u16 = 23;
/// JDWP `INVALID_LOCATION`: a breakpoint's code index is past its method's
/// code (interpreter round i1 wave 29).
pub const ERR_INVALID_LOCATION: u16 = 24;
pub const ERR_INVALID_FIELDID: u16 = 25;
pub const ERR_INVALID_FRAMEID: u16 = 30;
/// `NO_MORE_FRAMES` (31): `StackFrame.PopFrames` of a frame with no caller
/// (interpreter round i1 wave 44, `debug::pop_frames`).
pub const ERR_NO_MORE_FRAMES: u16 = 31;
/// JDWP `OPAQUE_FRAME`: the frame is a native method's (wave 15).
pub const ERR_OPAQUE_FRAME: u16 = 32;
pub const ERR_TYPE_MISMATCH: u16 = 34;
pub const ERR_INVALID_SLOT: u16 = 35;
/// JDWP `INVALID_MODULE`: the object is not a `java.lang.Module` (wave 25).
pub const ERR_INVALID_MODULE: u16 = 42;
/// JDWP `NOT_FOUND`: what HotSpot's back end answers the delete of the last
/// breakpoint request at a location whose JVMTI breakpoint a redefinition
/// cleared (interpreter round i6 wave 119, lane L3; measured).
pub const ERR_NOT_FOUND: u16 = 41;
pub const ERR_NOT_IMPLEMENTED: u16 = 99;
/// JDWP `ABSENT_INFORMATION`: the class or method has no such attribute (no
/// `SourceFile`, no `LocalVariableTable`; an array class's version).
pub const ERR_ABSENT_INFORMATION: u16 = 101;
/// JDWP `INVALID_EVENT_TYPE` is 102. Until wave 8 this constant was 500,
/// which is `INVALID_TAG`, so an unknown event kind in `EventRequest.Set`
/// was reported to the debugger as a bad value tag.
pub const ERR_INVALID_EVENT_TYPE: u16 = 102;
pub const ERR_ILLEGAL_ARGUMENT: u16 = 103;
/// JDWP `OUT_OF_MEMORY`: what HotSpot's back end answers when the JNI call
/// behind a command throws (interpreter round i1 wave 43:
/// `ArrayType.NewInstance` of a negative length).
pub const ERR_OUT_OF_MEMORY: u16 = 110;
pub const ERR_VM_DEAD: u16 = 112;
pub const ERR_INTERNAL: u16 = 113;
/// JDWP `INVALID_TAG`: a value tag that names no type (interpreter round i1
/// wave 29: `StackFrame.GetValues`).
pub const ERR_INVALID_TAG: u16 = 500;
pub const ERR_ALREADY_INVOKING: u16 = 502;
pub const ERR_INVALID_INDEX: u16 = 503;
pub const ERR_INVALID_LENGTH: u16 = 504;
pub const ERR_INVALID_STRING: u16 = 506;
/// JDWP `INVALID_CLASS_LOADER`: the object is not a class loader
/// (interpreter round i1 wave 43: `ClassLoaderReference.VisibleClasses`).
pub const ERR_INVALID_CLASS_LOADER: u16 = 507;
pub const ERR_INVALID_ARRAY: u16 = 508;
/// JDWP `NATIVE_METHOD`: `Method.LineTable` / `VariableTable` of a native
/// method.
pub const ERR_NATIVE_METHOD: u16 = 511;

// ---------------------------------------------------------------------------
// JDWP ThreadStatus constants (`ThreadReference.Status`)
// ---------------------------------------------------------------------------

pub const THREAD_STATUS_ZOMBIE: u32 = 0;
pub const THREAD_STATUS_RUNNING: u32 = 1;
pub const THREAD_STATUS_SLEEPING: u32 = 2;
pub const THREAD_STATUS_MONITOR: u32 = 3;
pub const THREAD_STATUS_WAIT: u32 = 4;

/// The id of the one thread group the server models, "system"
/// (`VirtualMachine.TopLevelThreadGroups`, `ThreadReference.ThreadGroup`).
/// Interpreter round i1 wave 23: just below the object ids
/// (`ids::HEAP_OBJECT_ID_BASE`), far above any thread id. It was 1, the id
/// of the VM's first thread, so the group and that thread shared an id — and
/// since wave 23 a thread id also names its `Thread` object in the
/// `ObjectReference` commands (`debug::object_for_id`), which would have
/// answered the thread's class for the group's `referenceType()`.
pub const SYSTEM_THREAD_GROUP_ID: u64 = crate::debug::ids::HEAP_OBJECT_ID_BASE - 1;

// ---------------------------------------------------------------------------
// Command set / command constants
// ---------------------------------------------------------------------------

// VirtualMachine (1)
pub const CS_VM: u8 = 1;
pub const CMD_VM_VERSION: u8 = 1;
pub const CMD_VM_CLASSES_BY_SIGNATURE: u8 = 2;
pub const CMD_VM_ALL_THREADS: u8 = 4;
pub const CMD_VM_TOP_LEVEL_THREAD_GROUPS: u8 = 5;
pub const CMD_VM_DISPOSE: u8 = 6;
pub const CMD_VM_ID_SIZES: u8 = 7;
pub const CMD_VM_SUSPEND: u8 = 8;
pub const CMD_VM_RESUME: u8 = 9;
pub const CMD_VM_EXIT: u8 = 10;
pub const CMD_VM_CAPABILITIES: u8 = 12;
/// `VirtualMachine.CreateString` is command 11. Until wave 7 this constant
/// was 14, the number of `DisposeObjects`: jdb's `CreateString` answered
/// NOT_IMPLEMENTED, and every `DisposeObjects` JDI batches up was parsed as a
/// string and answered with a new, meaningless object id.
pub const CMD_VM_CREATE_STRING: u8 = 11;
/// `VirtualMachine.ClassPaths` (interpreter round i1 wave 24; served by the
/// JDWP server from the VM's system properties, `debug::class_paths`).
pub const CMD_VM_CLASS_PATHS: u8 = 13;
pub const CMD_VM_DISPOSE_OBJECTS: u8 = 14;
pub const CMD_VM_HOLD_EVENTS: u8 = 15;
pub const CMD_VM_RELEASE_EVENTS: u8 = 16;
pub const CMD_VM_CAPABILITIES_NEW: u8 = 17;
/// `VirtualMachine.RedefineClasses` (interpreter round i5 wave 104, lane L1;
/// `canRedefineClasses`; served by the JDWP server on a registered mutator,
/// `debug::redefine_classes`).
pub const CMD_VM_REDEFINE_CLASSES: u8 = 18;
pub const CMD_VM_SET_DEFAULT_STRATUM: u8 = 19;
pub const CMD_VM_ALL_CLASSES_WITH_GENERIC: u8 = 20;
/// `VirtualMachine.InstanceCounts` (interpreter round i5 wave 103; a heap
/// command walked on a parked thread, `debug::inspect`).
pub const CMD_VM_INSTANCE_COUNTS: u8 = 21;
/// `VirtualMachine.AllModules` (JDWP 9; wave 25, a heap command).
pub const CMD_VM_ALL_MODULES: u8 = 22;

// ReferenceType (2)
pub const CS_REF_TYPE: u8 = 2;
pub const CMD_RT_SIGNATURE: u8 = 1;
pub const CMD_RT_CLASS_LOADER: u8 = 2;
pub const CMD_RT_MODIFIERS: u8 = 3;
pub const CMD_RT_FIELDS: u8 = 4;
pub const CMD_RT_METHODS: u8 = 5;
pub const CMD_RT_GET_VALUES: u8 = 6;
pub const CMD_RT_SOURCE_FILE: u8 = 7;
pub const CMD_RT_STATUS: u8 = 9;
pub const CMD_RT_INTERFACES: u8 = 10;
pub const CMD_RT_CLASS_OBJECT: u8 = 11;
/// `ReferenceType.SourceDebugExtension` (interpreter round i1 wave 43;
/// served by the JDWP server from the class file, `debug::source_debug_extension`).
pub const CMD_RT_SOURCE_DEBUG_EXTENSION: u8 = 12;
pub const CMD_RT_SIGNATURE_WITH_GENERIC: u8 = 13;
pub const CMD_RT_FIELDS_WITH_GENERIC: u8 = 14;
pub const CMD_RT_METHODS_WITH_GENERIC: u8 = 15;
/// `ReferenceType.Instances` (interpreter round i5 wave 103; a heap command
/// walked on a parked thread, `debug::inspect`).
pub const CMD_RT_INSTANCES: u8 = 16;
pub const CMD_RT_CLASS_FILE_VERSION: u8 = 17;
/// `ReferenceType.ConstantPool` (interpreter round i5 wave 103; served by
/// the JDWP server from the class file, `debug::constant_pool`).
pub const CMD_RT_CONSTANT_POOL: u8 = 18;
/// `ReferenceType.Module` (JDWP 9; wave 25, a heap command).
pub const CMD_RT_MODULE: u8 = 19;

// ThreadReference (11)
pub const CS_THREAD_REF: u8 = 11;
pub const CMD_TR_NAME: u8 = 1;
pub const CMD_TR_SUSPEND: u8 = 2;
pub const CMD_TR_RESUME: u8 = 3;
pub const CMD_TR_STATUS: u8 = 4;
pub const CMD_TR_THREAD_GROUP: u8 = 5;
pub const CMD_TR_FRAMES: u8 = 6;
pub const CMD_TR_FRAME_COUNT: u8 = 7;
/// `ThreadReference.OwnedMonitors` (wave 25; a heap command, `debug::inspect`).
pub const CMD_TR_OWNED_MONITORS: u8 = 8;
/// `ThreadReference.CurrentContendedMonitor` (wave 25; a heap command).
pub const CMD_TR_CURRENT_CONTENDED_MONITOR: u8 = 9;
/// `ThreadReference.Stop` (wave 24; a heap command, `debug::inspect`).
pub const CMD_TR_STOP: u8 = 10;
/// `ThreadReference.Interrupt` (wave 24; a heap command, `debug::inspect`).
pub const CMD_TR_INTERRUPT: u8 = 11;
pub const CMD_TR_SUSPEND_COUNT: u8 = 12;
/// `ThreadReference.OwnedMonitorsStackDepthInfo` (wave 25; a heap command).
pub const CMD_TR_OWNED_MONITORS_STACK_DEPTH_INFO: u8 = 13;
/// `ThreadReference.IsVirtual` (JDWP 21; wave 25, served by the JDWP server
/// from the VM's virtual-thread table, `debug::thread_is_virtual`).
pub const CMD_TR_IS_VIRTUAL: u8 = 15;
/// `ThreadReference.ForceEarlyReturn` (interpreter round i1 wave 43; served
/// by the JDWP server on the thread, `debug::early_return`).
pub const CMD_TR_FORCE_EARLY_RETURN: u8 = 14;

// EventRequest (15)
pub const CS_EVENT_REQUEST: u8 = 15;
pub const CMD_ER_SET: u8 = 1;
pub const CMD_ER_CLEAR: u8 = 2;
pub const CMD_ER_CLEAR_ALL_BREAKPOINTS: u8 = 3;

// `invokeOptions` bits of `ClassType.InvokeMethod` / `ObjectReference.InvokeMethod`.
/// Only the invoking thread runs for the call (wave 9: honoured).
pub const INVOKE_SINGLE_THREADED: u32 = 0x01;
/// Select the method on the class named, not on the receiver's class.
pub const INVOKE_NONVIRTUAL: u32 = 0x02;

// ClassType (3)
pub const CS_CLASS_TYPE: u8 = 3;
pub const CMD_CT_SUPERCLASS: u8 = 1;
pub const CMD_CT_SET_VALUES: u8 = 2;
pub const CMD_CT_INVOKE_METHOD: u8 = 3;
/// `ClassType.NewInstance` (wave 24).
pub const CMD_CT_NEW_INSTANCE: u8 = 4;

// ArrayType (4)
pub const CS_ARRAY_TYPE: u8 = 4;
pub const CMD_AT_NEW_INSTANCE: u8 = 1;

// InterfaceType (5)
pub const CS_INTERFACE_TYPE: u8 = 5;
/// `InterfaceType.InvokeMethod` (wave 24): a static interface method.
pub const CMD_IT_INVOKE_METHOD: u8 = 1;

// Method (6)
pub const CS_METHOD: u8 = 6;
pub const CMD_M_LINE_TABLE: u8 = 1;
pub const CMD_M_VARIABLE_TABLE: u8 = 2;
pub const CMD_M_BYTECODES: u8 = 3;
pub const CMD_M_IS_OBSOLETE: u8 = 4;
pub const CMD_M_VARIABLE_TABLE_WITH_GENERIC: u8 = 5;

// ObjectReference (9)
pub const CS_OBJECT_REF: u8 = 9;
pub const CMD_OR_REFERENCE_TYPE: u8 = 1;
pub const CMD_OR_GET_VALUES: u8 = 2;
pub const CMD_OR_SET_VALUES: u8 = 3;
/// `ObjectReference.MonitorInfo` (interpreter round i5 wave 103; a heap
/// command, `debug::inspect`).
pub const CMD_OR_MONITOR_INFO: u8 = 5;
pub const CMD_OR_INVOKE_METHOD: u8 = 6;
pub const CMD_OR_DISABLE_COLLECTION: u8 = 7;
pub const CMD_OR_ENABLE_COLLECTION: u8 = 8;
pub const CMD_OR_IS_COLLECTED: u8 = 9;
/// `ObjectReference.ReferringObjects` (interpreter round i5 wave 103; a heap
/// command walked on a parked thread, `debug::inspect`).
pub const CMD_OR_REFERRING_OBJECTS: u8 = 10;

// StringReference (10)
pub const CS_STRING_REF: u8 = 10;
pub const CMD_SR_VALUE: u8 = 1;

// ThreadGroupReference (12)
pub const CS_THREAD_GROUP_REF: u8 = 12;
pub const CMD_TGR_NAME: u8 = 1;
pub const CMD_TGR_PARENT: u8 = 2;
pub const CMD_TGR_CHILDREN: u8 = 3;

// ArrayReference (13)
pub const CS_ARRAY_REF: u8 = 13;
pub const CMD_AR_LENGTH: u8 = 1;
pub const CMD_AR_GET_VALUES: u8 = 2;
pub const CMD_AR_SET_VALUES: u8 = 3;

// ClassLoaderReference (14)
pub const CS_CLASSLOADER_REF: u8 = 14;
pub const CMD_CLR_VISIBLE_CLASSES: u8 = 1;

// ModuleReference (18, JDWP 9; wave 25, heap commands)
pub const CS_MODULE_REF: u8 = 18;
pub const CMD_MR_NAME: u8 = 1;
pub const CMD_MR_CLASS_LOADER: u8 = 2;

// StackFrame (16)
pub const CS_STACK_FRAME: u8 = 16;
pub const CMD_SF_GET_VALUES: u8 = 1;
pub const CMD_SF_SET_VALUES: u8 = 2;
pub const CMD_SF_THIS_OBJECT: u8 = 3;
/// `StackFrame.PopFrames` (wave 44): served by the JDWP server on the
/// frame's parked thread (`debug::pop_frames`).
pub const CMD_SF_POP_FRAMES: u8 = 4;

// ClassObjectReference (17)
pub const CS_CLASS_OBJ_REF: u8 = 17;
pub const CMD_COR_REFLECTED_TYPE: u8 = 1;

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Result of a command handler: reply payload + error code.
pub struct CommandResult {
    pub error_code: u16,
    pub data: Vec<u8>,
}

impl CommandResult {
    pub fn ok(data: Vec<u8>) -> Self {
        Self {
            error_code: ERR_NONE,
            data,
        }
    }

    pub fn error(code: u16) -> Self {
        Self {
            error_code: code,
            data: Vec::new(),
        }
    }
}

/// Route a JDWP command to the correct handler.
pub fn dispatch(
    command_set: u8,
    command: u8,
    data: &[u8],
    state: &mut DebugState,
) -> CommandResult {
    match (command_set, command) {
        // -- Heap commands (wave 8) ----------------------------------------
        // Served against the live VM by `debug::inspect` (the JDWP server
        // routes them there before this table). A bare `DebugState` has no
        // heap behind it; these answered placeholders — class 0, `null`
        // fields, empty arrays, `""`, success for writes that wrote nothing.
        (set, cmd) if crate::debug::inspect::is_heap_command(set, cmd) => {
            CommandResult::error(ERR_NOT_IMPLEMENTED)
        }

        // -- VirtualMachine ------------------------------------------------
        (CS_VM, CMD_VM_VERSION) => handle_vm_version(),
        (CS_VM, CMD_VM_CLASSES_BY_SIGNATURE) => handle_vm_classes_by_sig(data, state),
        (CS_VM, CMD_VM_ALL_THREADS) => handle_vm_all_threads(state),
        (CS_VM, CMD_VM_TOP_LEVEL_THREAD_GROUPS) => handle_vm_top_level_thread_groups(),
        (CS_VM, CMD_VM_DISPOSE) => handle_vm_dispose(state),
        (CS_VM, CMD_VM_ID_SIZES) => handle_vm_id_sizes(),
        (CS_VM, CMD_VM_SUSPEND) => handle_vm_suspend(state),
        (CS_VM, CMD_VM_RESUME) => handle_vm_resume(state),
        (CS_VM, CMD_VM_EXIT) => handle_vm_exit(data, state),
        (CS_VM, CMD_VM_CAPABILITIES) => handle_vm_capabilities(),
        (CS_VM, CMD_VM_DISPOSE_OBJECTS) => handle_vm_dispose_objects(data, state),
        (CS_VM, CMD_VM_CAPABILITIES_NEW) => handle_vm_capabilities_new(),
        // Wave 23: JDI sends `HoldEvents` / `ReleaseEvents` itself when its
        // event queue backs up (`TargetVM`), and `SetDefaultStratum` from
        // `VirtualMachine.setDefaultStratum`; all three answered
        // `NOT_IMPLEMENTED`, which JDI printed as a stack trace or threw.
        (CS_VM, CMD_VM_HOLD_EVENTS) => {
            state.events_held = true;
            CommandResult::ok(Vec::new())
        }
        (CS_VM, CMD_VM_RELEASE_EVENTS) => {
            state.events_held = false;
            CommandResult::ok(Vec::new())
        }
        // JDI applies a stratum itself, from the SMAP it reads with
        // `ReferenceType.SourceDebugExtension` (wave 43); no reply of this
        // server depends on the default one.
        (CS_VM, CMD_VM_SET_DEFAULT_STRATUM) => match PayloadReader::new(data).read_string() {
            Ok(_) => CommandResult::ok(Vec::new()),
            Err(_) => CommandResult::error(ERR_INTERNAL),
        },
        (CS_VM, CMD_VM_ALL_CLASSES_WITH_GENERIC) => handle_vm_all_classes_generic(state),

        // -- ReferenceType -------------------------------------------------
        // Wave 23: the `*WithGeneric` forms (a JDWP 1.5+ target is asked for
        // them, never the plain ones), `Modifiers`, `Status`, `Interfaces`
        // and `ClassFileVersion`. Only the four plain commands were served,
        // so JDI's first `ReferenceType.methods()` — every line breakpoint —
        // failed with `UnsupportedOperationException`.
        (CS_REF_TYPE, CMD_RT_SIGNATURE) => handle_rt_signature(data, state, false),
        (CS_REF_TYPE, CMD_RT_SIGNATURE_WITH_GENERIC) => handle_rt_signature(data, state, true),
        (CS_REF_TYPE, CMD_RT_MODIFIERS) => handle_rt_modifiers(data, state),
        (CS_REF_TYPE, CMD_RT_FIELDS) => handle_rt_fields(data, state, false),
        (CS_REF_TYPE, CMD_RT_FIELDS_WITH_GENERIC) => handle_rt_fields(data, state, true),
        (CS_REF_TYPE, CMD_RT_METHODS) => handle_rt_methods(data, state, false),
        (CS_REF_TYPE, CMD_RT_METHODS_WITH_GENERIC) => handle_rt_methods(data, state, true),
        (CS_REF_TYPE, CMD_RT_SOURCE_FILE) => handle_rt_source_file(data, state),
        (CS_REF_TYPE, CMD_RT_STATUS) => handle_rt_status(data, state),
        (CS_REF_TYPE, CMD_RT_INTERFACES) => handle_rt_interfaces(data, state),
        (CS_REF_TYPE, CMD_RT_CLASS_FILE_VERSION) => handle_rt_class_file_version(data, state),

        // -- ThreadReference -----------------------------------------------
        (CS_THREAD_REF, CMD_TR_NAME) => handle_tr_name(data, state),
        (CS_THREAD_REF, CMD_TR_SUSPEND) => handle_tr_suspend(data, state),
        (CS_THREAD_REF, CMD_TR_RESUME) => handle_tr_resume(data, state),
        (CS_THREAD_REF, CMD_TR_STATUS) => handle_tr_status(data, state),
        (CS_THREAD_REF, CMD_TR_THREAD_GROUP) => handle_tr_thread_group(data, state),
        (CS_THREAD_REF, CMD_TR_FRAMES) => handle_tr_frames(data, state),
        (CS_THREAD_REF, CMD_TR_FRAME_COUNT) => handle_tr_frame_count(data, state),
        (CS_THREAD_REF, CMD_TR_SUSPEND_COUNT) => handle_tr_suspend_count(data, state),

        // -- ClassType -----------------------------------------------------
        (CS_CLASS_TYPE, CMD_CT_SUPERCLASS) => handle_ct_superclass(data, state),
        (CS_CLASS_TYPE, CMD_CT_INVOKE_METHOD) => handle_ct_invoke_method(data, state),
        // Wave 24: both answered `NOT_IMPLEMENTED`, so no expression
        // evaluator could write `new Foo()` or call a static interface method.
        (CS_CLASS_TYPE, CMD_CT_NEW_INSTANCE) => match prepare_ct_new_instance(data, state) {
            Ok(call) => call.run(),
            Err(refused) => refused,
        },
        (CS_INTERFACE_TYPE, CMD_IT_INVOKE_METHOD) => handle_ct_invoke_method(data, state),

        // -- ArrayType -----------------------------------------------------
        (CS_ARRAY_TYPE, CMD_AT_NEW_INSTANCE) => handle_at_new_instance(data, state),

        // -- Method --------------------------------------------------------
        (CS_METHOD, CMD_M_LINE_TABLE) => handle_method_line_table(data, state),
        (CS_METHOD, CMD_M_VARIABLE_TABLE) => handle_method_variable_table(data, state),
        (CS_METHOD, CMD_M_BYTECODES) => handle_method_bytecodes(data, state),
        (CS_METHOD, CMD_M_IS_OBSOLETE) => handle_method_is_obsolete(data),
        (CS_METHOD, CMD_M_VARIABLE_TABLE_WITH_GENERIC) => {
            handle_method_variable_table_generic(data, state)
        }

        // -- ObjectReference -----------------------------------------------
        (CS_OBJECT_REF, CMD_OR_INVOKE_METHOD) => handle_or_invoke_method(data, state),

        // -- ThreadGroupReference ------------------------------------------
        (CS_THREAD_GROUP_REF, CMD_TGR_NAME) => handle_tgr_name(data, state),
        (CS_THREAD_GROUP_REF, CMD_TGR_PARENT) => handle_tgr_parent(),
        (CS_THREAD_GROUP_REF, CMD_TGR_CHILDREN) => handle_tgr_children(state),

        // -- ClassLoaderReference ------------------------------------------
        // Wave 24: `VisibleClasses` is a heap command (`debug::inspect`): it
        // compares each class's loader with the one named, which needs the
        // heap. It listed every class of every loader.

        // -- EventRequest --------------------------------------------------
        (CS_EVENT_REQUEST, CMD_ER_SET) => handle_er_set(data, state),
        (CS_EVENT_REQUEST, CMD_ER_CLEAR) => handle_er_clear(data, state),
        (CS_EVENT_REQUEST, CMD_ER_CLEAR_ALL_BREAKPOINTS) => handle_er_clear_all_breakpoints(state),

        // -- StackFrame ----------------------------------------------------
        (CS_STACK_FRAME, CMD_SF_GET_VALUES) => handle_sf_get_values(data, state),
        // Until wave 8 this answered success and wrote nothing, so jdb's
        // `set x = 5` reported success on a local it never changed. Since
        // wave 9 the JDWP server writes the local on the frame's parked
        // thread (`debug::inspect`); a bare `DebugState` has no frame to
        // write.
        (CS_STACK_FRAME, CMD_SF_SET_VALUES) => CommandResult::error(ERR_NOT_IMPLEMENTED),
        (CS_STACK_FRAME, CMD_SF_THIS_OBJECT) => handle_sf_this_object(data, state),

        // -- Unknown -------------------------------------------------------
        _ => {
            tracing::warn!(command_set, command, "unimplemented JDWP command");
            CommandResult::error(ERR_NOT_IMPLEMENTED)
        }
    }
}

// ===========================================================================
// VirtualMachine command set (1)
// ===========================================================================

/// `VirtualMachine.Version` of a bare `DebugState` (no VM behind it): JDWP
/// 1.8. The JDWP server answers from the VM's JDK instead (interpreter round
/// i1 wave 25, `debug::vm_version`: JDWP 25.0 for a JDK 25), before this
/// table is consulted.
fn handle_vm_version() -> CommandResult {
    let mut pw = PayloadWriter::new();
    pw.put_string("CratonVM JDWP Debug Server"); // description
    pw.put_u32_be(1); // jdwpMajor
    pw.put_u32_be(8); // jdwpMinor
    pw.put_string("1.8.0"); // vmVersion
    pw.put_string("CratonVM"); // vmName
    CommandResult::ok(pw.into_bytes())
}

fn handle_vm_classes_by_sig(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let signature = match reader.read_string() {
        Ok(s) => s,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    // Every class of that name, whichever loader defined it (wave 15: one
    // per name was kept, the last defined).
    let mut matches: Vec<(u64, u8)> = state
        .class_signatures
        .iter()
        .filter(|&(_, sig)| *sig == signature)
        // Wave 103 (interpreter round i5, lane L1): only prepared classes
        // (or arrays), as HotSpot's `classesForSignature` keeps them.
        .filter(|&(id, _)| !state.unprepared_classes.contains(id))
        .map(|(&id, _)| (id, state.location_type_tag(id)))
        .collect();
    matches.sort_unstable();
    let mut pw = PayloadWriter::new();
    pw.put_u32_be(matches.len() as u32); // Cast: bounded by the class count
    for (ref_type_id, type_tag) in matches {
        pw.put_u8(type_tag); // refTypeTag: 1=class, 2=interface, 3=array
        pw.put_u64_be(crate::debug::ids::class_to_wire(ref_type_id));
        pw.put_u32_be(class_status(&*state, ref_type_id));
    }
    CommandResult::ok(pw.into_bytes())
}

fn handle_vm_all_threads(state: &mut DebugState) -> CommandResult {
    let ids = listed_thread_ids(state);
    let mut pw = PayloadWriter::new();
    pw.put_u32_be(ids.len() as u32);
    // Wire ids (wave 25): the main thread, VM thread 0, is not sent as 0,
    // which JDI reads as a null thread (`ids::thread_to_wire`).
    for tid in &ids {
        pw.put_u64_be(state.wire_thread(*tid));
    }
    CommandResult::ok(pw.into_bytes())
}

/// The thread ids the debugger may name, ascending: the threads the VM
/// published (`thread_names`, the VM's own thread ids — the ones events,
/// frames and suspensions use) and any registered through
/// `IdManager::register_thread`. Wave 7: `AllThreads` and
/// `ThreadGroupReference.Children` answered from the id manager alone, which
/// the running VM never fills, so jdb's `threads` listed nothing.
///
/// Wave 11: a thread the VM reports dead (`THREAD_STATUS_ZOMBIE` in
/// `thread_statuses`) is left out, as HotSpot's `AllThreads` lists live
/// threads only. The registry keeps a dead thread's entry (so its name and
/// `Status` still answer), which listed it here until it was purged.
/// The threads `VirtualMachine.AllThreads` and `ThreadGroupReference.Children`
/// list (interpreter round i1 wave 49, lane L1;
/// `docs/feature-designs/jvmti-delivery-threading.md`):
/// [`known_thread_ids`] without the virtual threads, as HotSpot's back end
/// answers unless its agent was started with `includevirtualthreads=y`
/// (`CRATONVM_JDWP_INCLUDE_VIRTUAL_THREADS=1` here,
/// `DebugState::include_virtual_threads`). A debugger learns a virtual thread
/// from its `ThreadStart` event or from the thread an event stops.
pub(crate) fn listed_thread_ids(state: &DebugState) -> Vec<u64> {
    let mut ids = known_thread_ids(state);
    if !state.include_virtual_threads {
        ids.retain(|tid| !state.is_virtual_thread(*tid));
    }
    ids
}

pub(crate) fn known_thread_ids(state: &DebugState) -> Vec<u64> {
    let mut ids: Vec<u64> = state.thread_names.keys().copied().collect();
    ids.extend(state.ids.all_thread_ids().iter().map(|t| t.0));
    ids.retain(|tid| state.thread_statuses.get(tid) != Some(&THREAD_STATUS_ZOMBIE));
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn handle_vm_top_level_thread_groups() -> CommandResult {
    let mut pw = PayloadWriter::new();
    pw.put_u32_be(1); // one top-level thread group
    pw.put_u64_be(SYSTEM_THREAD_GROUP_ID);
    CommandResult::ok(pw.into_bytes())
}

fn handle_vm_dispose(state: &mut DebugState) -> CommandResult {
    state.disposed = true;
    CommandResult::ok(Vec::new())
}

fn handle_vm_id_sizes() -> CommandResult {
    let mut pw = PayloadWriter::new();
    pw.put_u32_be(8); // fieldIDSize
    pw.put_u32_be(8); // methodIDSize
    pw.put_u32_be(8); // objectIDSize
    pw.put_u32_be(8); // referenceTypeIDSize
    pw.put_u32_be(8); // frameIDSize
    CommandResult::ok(pw.into_bytes())
}

fn handle_vm_suspend(state: &mut DebugState) -> CommandResult {
    state.suspend_all();
    CommandResult::ok(Vec::new())
}

fn handle_vm_resume(state: &mut DebugState) -> CommandResult {
    // JDWP `VirtualMachine.Resume` takes one from every thread's suspend
    // count, so it also resumes a thread a `SUSPEND_EVENT_THREAD` breakpoint
    // suspended on its own (jdb's `cont` sends this command, not
    // `ThreadReference.Resume`). A thread suspended twice stays suspended
    // (wave 7: suspend counts).
    state.resume_vm();
    CommandResult::ok(Vec::new())
}

fn handle_vm_exit(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let exit_code = reader.read_u32_be().unwrap_or(0);
    state.exit_code = Some(exit_code as i32);
    CommandResult::ok(Vec::new())
}

fn handle_vm_capabilities() -> CommandResult {
    let mut pw = PayloadWriter::new();
    // 7 boolean capabilities:
    // [0] canWatchFieldModification, [1] canWatchFieldAccess: true again
    // (wave 10). Wave 9 answered false because nothing delivered a watch
    // event; the interpreter's four field bytecodes now report them
    // (`interpreter::deliver_field_watch_if_armed`). See
    // docs/feature-designs/jvmti-delivery-threading.md.
    pw.put_u8(1);
    pw.put_u8(1);
    // [2] canGetBytecodes: true since wave 24, when the JDWP server began to
    // answer `Method.Bytecodes` from the class (`debug::method_bytecodes`);
    // HotSpot answers true.
    pw.put_u8(1);
    // [3] canGetSyntheticAttribute: true since wave 23, when the members'
    // `modBits` began to carry `MOD_SYNTHETIC` (`debug::add_class_metadata`),
    // as HotSpot's do.
    pw.put_u8(1);
    // [4] canGetOwnedMonitorInfo, [5] canGetCurrentContendedMonitor: true
    // since wave 25, when `ThreadReference.OwnedMonitors` /
    // `CurrentContendedMonitor` began to answer from the thread's lock stack
    // and its frames' monitor records (`debug::inspect`, owned monitors);
    // HotSpot answers true.
    pw.put_u8(1);
    pw.put_u8(1);
    // [6] canGetMonitorInfo: true since interpreter round i5 wave 103 (lane
    // L1), when `ObjectReference.MonitorInfo` began to be served from the
    // monitor table (`debug::inspect`); HotSpot answers true.
    pw.put_u8(1);
    CommandResult::ok(pw.into_bytes())
}

/// `CapabilitiesNew.canGetMonitorFrameInfo` (index 17): true since wave 25,
/// with `ThreadReference.OwnedMonitorsStackDepthInfo` (JDI
/// `ownedMonitorsAndFrames()`).
const CAPABILITY_NEW_MONITOR_FRAME_INFO: usize = 17;

/// `CapabilitiesNew.canRedefineClasses` (index 7): true since interpreter
/// round i5 wave 104 (lane L1), when `VirtualMachine.RedefineClasses` began
/// to be served (`debug::redefine_classes`); HotSpot answers true, and
/// `canAddMethod` (8) and `canUnrestrictedlyRedefineClasses` (9) false, as
/// here.
const CAPABILITY_NEW_REDEFINE_CLASSES: usize = 7;

/// `CapabilitiesNew.canRequestVMDeathEvent` (index 13): true since wave 26,
/// when a `VMDeath` request's `SUSPEND_ALL` began to hold the dying VM until
/// the debugger resumes it (`debug::report_vm_death`); HotSpot answers true,
/// and JDI's `createVMDeathRequest` throws `UnsupportedOperationException`
/// without it.
const CAPABILITY_NEW_VM_DEATH_EVENT: usize = 13;

/// `CapabilitiesNew.canGetSourceDebugExtension` (index 12): true since
/// interpreter round i1 wave 43, when the server began to answer
/// `ReferenceType.SourceDebugExtension` from the class file
/// (`debug::source_debug_extension`); HotSpot answers true, and JDI reads no
/// SMAP (no Kotlin or JSP stratum) without it.
const CAPABILITY_NEW_SOURCE_DEBUG_EXTENSION: usize = 12;

/// `CapabilitiesNew.canForceEarlyReturn` (index 20): true since interpreter
/// round i1 wave 43, when `ThreadReference.ForceEarlyReturn` began to be
/// served at the interpreter's suspend point (`debug::early_return`);
/// HotSpot answers true, and JDI refuses "Force Return" without it.
const CAPABILITY_NEW_FORCE_EARLY_RETURN: usize = 20;

/// `CapabilitiesNew.canPopFrames` (index 10): true since interpreter round
/// i1 wave 44, when `StackFrame.PopFrames` began to be served at the
/// interpreter's suspend point (`debug::pop_frames`); HotSpot answers true,
/// and JDI refuses "Drop Frame" without it.
const CAPABILITY_NEW_POP_FRAMES: usize = 10;

/// `CapabilitiesNew.canUseInstanceFilters` (index 11): true since
/// interpreter round i4 wave 85 (lane L1), when `InstanceOnly` began to be
/// applied on breakpoint, step, method entry and exit and exception requests
/// (`EventManager::match_location_events_with_instance`); HotSpot answers
/// true, and JDI's `addInstanceFilter` throws
/// `UnsupportedOperationException` without it.
const CAPABILITY_NEW_INSTANCE_FILTERS: usize = 11;

/// `CapabilitiesNew.canSetDefaultStratum` (index 14) and
/// `canUseSourceNameFilters` (index 18): true since interpreter round i5
/// wave 100 (lane L1). HotSpot's back end (`VirtualMachineImpl.c`
/// `capabilitiesNew`) writes both as a constant `JNI_TRUE`, and this server
/// serves both (`VirtualMachine.SetDefaultStratum` since wave 43, the
/// `SourceNameMatch` modifier since wave 23); they answered false, so JDI's
/// `VirtualMachine.canSetDefaultStratum()` said false where HotSpot says
/// true.
///
/// Interpreter round i5 wave 103 (lane L1): MEASURED on HotSpot 25 (build
/// w103, `tools/probes/interp/L1/L1W103RawJdwpCapabilities.java`):
/// `canUseSourceNameFilters` (index 18) is 0 there, so it is 0 here again;
/// the `SourceNameMatch` modifier stays served (JDI offers
/// `addSourceNameFilter` from the JDWP version, not from this bit).
const CAPABILITY_NEW_SET_DEFAULT_STRATUM: usize = 14;

/// `CapabilitiesNew.canGetInstanceInfo` (index 15) and `canGetConstantPool`
/// (index 19): true since interpreter round i5 wave 103 (lane L1), when the
/// server began to answer `VirtualMachine.InstanceCounts`,
/// `ReferenceType.Instances` and `ObjectReference.ReferringObjects` (a heap
/// walk on a parked thread, `debug::inspect`) and `ReferenceType.ConstantPool`
/// (from the class file, `debug::constant_pool`). HotSpot answers true.
const CAPABILITY_NEW_INSTANCE_INFO: usize = 15;
const CAPABILITY_NEW_CONSTANT_POOL: usize = 19;

/// `CapabilitiesNew.canRequestMonitorEvents` (index 16): true since
/// interpreter round i5 wave 103 (lane L1), when the four monitor events
/// (43-46) began to be reported from the monitor paths that post the C JVMTI
/// ones (`debug::post_jdwp_monitor_event`). HotSpot answers true.
const CAPABILITY_NEW_MONITOR_EVENTS: usize = 16;

/// `VirtualMachine.DisposeObjects` (wave 7): `requests` × (objectID,
/// refCnt). Each id loses `refCnt` of the references the debugger received;
/// an id with none left is released (the JDWP server frees its handle after
/// the command, `debug::release_disposed_objects`). Ids this table does not
/// know are ignored, as the reference back end ignores them.
fn handle_vm_dispose_objects(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let requests = match reader.read_u32_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };
    for _ in 0..requests {
        let (Ok(id), Ok(ref_count)) = (reader.read_u64_be(), reader.read_u32_be()) else {
            return CommandResult::error(ERR_INTERNAL);
        };
        state.objects.dispose(id, ref_count);
    }
    CommandResult::ok(Vec::new())
}

fn handle_vm_capabilities_new() -> CommandResult {
    let mut pw = PayloadWriter::new();
    // 32 boolean capabilities (the "new" set). The first seven repeat
    // `VirtualMachine.Capabilities` (JDI reads the watch capabilities from
    // here when the target answers this command, so the two must agree:
    // wave 10); of the rest, the ones named above are true.
    let old = handle_vm_capabilities().data;
    for i in 0..32 {
        let granted = i == CAPABILITY_NEW_MONITOR_FRAME_INFO
            || i == CAPABILITY_NEW_REDEFINE_CLASSES
            || i == CAPABILITY_NEW_VM_DEATH_EVENT
            || i == CAPABILITY_NEW_SOURCE_DEBUG_EXTENSION
            || i == CAPABILITY_NEW_FORCE_EARLY_RETURN
            || i == CAPABILITY_NEW_POP_FRAMES
            || i == CAPABILITY_NEW_INSTANCE_FILTERS
            || i == CAPABILITY_NEW_SET_DEFAULT_STRATUM
            || i == CAPABILITY_NEW_INSTANCE_INFO
            || i == CAPABILITY_NEW_MONITOR_EVENTS
            || i == CAPABILITY_NEW_CONSTANT_POOL;
        pw.put_u8(old.get(i).copied().unwrap_or(u8::from(granted)));
    }
    CommandResult::ok(pw.into_bytes())
}

fn handle_vm_all_classes_generic(state: &mut DebugState) -> CommandResult {
    let mut pw = PayloadWriter::new();
    // Wave 103 (interpreter round i5, lane L1): a class loaded but not
    // prepared is left out, as HotSpot's `allClasses1` filters it.
    let classes = state.listed_classes();
    pw.put_u32_be(classes.len() as u32); // Cast: bounded by the class count
    for (ref_type_id, type_tag, sig) in classes {
        pw.put_u8(type_tag); // refTypeTag
        pw.put_u64_be(crate::debug::ids::class_to_wire(ref_type_id)); // typeID
        pw.put_string(sig); // signature
                            // genericSignature (wave 23: always empty, which JDI caches, so a
                            // debugger that listed every class first never saw one)
        let generic = state
            .class_details
            .get(&ref_type_id)
            .and_then(|d| d.generic_signature.as_deref())
            .unwrap_or("");
        pw.put_string(generic);
        pw.put_u32_be(class_status(&*state, ref_type_id));
    }
    CommandResult::ok(pw.into_bytes())
}

/// The JDWP `ClassStatus` of class `ref_type_id` (interpreter round i1 wave
/// 15): the bits the server refreshed from the class manager before this
/// command (`debug::refresh_class_statuses`), else VERIFIED | PREPARED (a
/// bare `DebugState`). It was always 3, whatever the class's state — and
/// the comment beside it said "initialized", which 3 is not.
fn class_status(state: &DebugState, ref_type_id: u64) -> u32 {
    state
        .class_statuses
        .get(&ref_type_id)
        .copied()
        .unwrap_or(1 | 2)
}

// ===========================================================================
// ReferenceType command set (2)
// ===========================================================================

/// The `refType` a `ReferenceType` command names: its wire ID, or the reply
/// refusing the command (`INVALID_CLASS` for an ID naming no class the
/// session knows, as HotSpot answers a stale class ID's object).
fn read_known_class(data: &[u8], state: &DebugState) -> Result<u64, CommandResult> {
    let ref_id = PayloadReader::new(data)
        .read_u64_be()
        .map_err(|_| CommandResult::error(ERR_INTERNAL))?;
    if state.class_signatures.contains_key(&ref_id) || state.class_methods.contains_key(&ref_id) {
        Ok(ref_id)
    } else {
        Err(CommandResult::error(ERR_INVALID_CLASS))
    }
}

/// [`read_known_class`] for the commands HotSpot answers from a JVMTI
/// function that requires a prepared class (`Fields`, `Methods`,
/// `Interfaces` and their generic forms; interpreter round i5 wave 103,
/// lane L1): `CLASS_NOT_PREPARED` for a class loaded but not prepared
/// (`DebugState::unprepared_classes`, refreshed for the named class before
/// these commands, `debug::refresh_class_statuses_for`). A debugger reaches
/// such a class through a `Class` object (`ClassObjectReference.reflectedType`
/// of `Class.forName(n, false, l)`), not through the class listings.
fn read_prepared_class(data: &[u8], state: &DebugState) -> Result<u64, CommandResult> {
    let ref_id = read_known_class(data, state)?;
    if state.unprepared_classes.contains(&ref_id) {
        return Err(CommandResult::error(ERR_CLASS_NOT_PREPARED));
    }
    Ok(ref_id)
}

/// `ReferenceType.Signature` (2/1) and, with `generic`,
/// `SignatureWithGeneric` (2/13, wave 23): the JNI signature, then the
/// class's `Signature` attribute or the empty string.
fn handle_rt_signature(data: &[u8], state: &mut DebugState, generic: bool) -> CommandResult {
    let ref_id = match read_known_class(data, state) {
        Ok(id) => id,
        Err(refused) => return refused,
    };
    let Some(sig) = state.class_signatures.get(&ref_id) else {
        return CommandResult::error(ERR_INVALID_CLASS);
    };
    let mut pw = PayloadWriter::new();
    pw.put_string(sig);
    if generic {
        let generic_sig = state
            .class_details
            .get(&ref_id)
            .and_then(|d| d.generic_signature.as_deref())
            .unwrap_or("");
        pw.put_string(generic_sig);
    }
    CommandResult::ok(pw.into_bytes())
}

/// `ReferenceType.Modifiers` (2/3, wave 23): the class's modifiers as
/// HotSpot's JVMTI `GetClassModifiers` answers them
/// (`debug::jdwp_class_modifiers`); 0 for a class the metadata does not
/// describe (a bare `DebugState`).
fn handle_rt_modifiers(data: &[u8], state: &mut DebugState) -> CommandResult {
    let ref_id = match read_known_class(data, state) {
        Ok(id) => id,
        Err(refused) => return refused,
    };
    let modifiers = state.class_details.get(&ref_id).map_or(0, |d| d.modifiers);
    let mut pw = PayloadWriter::new();
    pw.put_u32_be(modifiers);
    CommandResult::ok(pw.into_bytes())
}

/// `ReferenceType.Fields` (2/4) and, with `generic`, `FieldsWithGeneric`
/// (2/14, wave 23): `fieldID, name, signature, [genericSignature,]
/// modBits` per declared field, in declaration order. An unknown class is
/// `INVALID_CLASS` (wave 23: it answered an empty list).
fn handle_rt_fields(data: &[u8], state: &mut DebugState, generic: bool) -> CommandResult {
    let ref_id = match read_prepared_class(data, state) {
        Ok(id) => id,
        Err(refused) => return refused,
    };
    let fields = state
        .class_fields
        .get(&ref_id)
        .map_or(&[][..], Vec::as_slice);
    let details = state.class_details.get(&ref_id);
    let mut pw = PayloadWriter::new();
    pw.put_u32_be(fields.len() as u32); // Cast: bounded by the u16 `fields_count`
    for f in fields {
        pw.put_u64_be(f.field_id.0);
        pw.put_string(&f.name);
        pw.put_string(&f.signature);
        if generic {
            let generic_sig = details
                .and_then(|d| d.field_generics.get(&f.field_id.0))
                .map_or("", String::as_str);
            pw.put_string(generic_sig);
        }
        pw.put_u32_be(f.mod_bits);
    }
    CommandResult::ok(pw.into_bytes())
}

/// `ReferenceType.Methods` (2/5) and, with `generic`, `MethodsWithGeneric`
/// (2/15, wave 23): `methodID, name, signature, [genericSignature,]
/// modBits` per declared method, in declaration order (HotSpot's back end
/// keeps the class file's order too). An unknown class is `INVALID_CLASS`
/// (wave 23: it answered an empty list).
fn handle_rt_methods(data: &[u8], state: &mut DebugState, generic: bool) -> CommandResult {
    let ref_id = match read_prepared_class(data, state) {
        Ok(id) => id,
        Err(refused) => return refused,
    };
    let methods = state
        .class_methods
        .get(&ref_id)
        .map_or(&[][..], Vec::as_slice);
    let details = state.class_details.get(&ref_id);
    let mut pw = PayloadWriter::new();
    pw.put_u32_be(methods.len() as u32); // Cast: bounded by the u16 `methods_count`
    for m in methods {
        pw.put_u64_be(m.method_id.0);
        pw.put_string(&m.name);
        pw.put_string(&m.signature);
        if generic {
            let generic_sig = details
                .and_then(|d| d.method_generics.get(&m.method_id.0))
                .map_or("", String::as_str);
            pw.put_string(generic_sig);
        }
        pw.put_u32_be(m.mod_bits);
    }
    CommandResult::ok(pw.into_bytes())
}

/// `ReferenceType.SourceFile` (2/7). A class without a `SourceFile`
/// attribute, and an array class, answer `ABSENT_INFORMATION`, as HotSpot
/// does, which JDI turns into `AbsentInformationException` (wave 23: they
/// answered `INVALID_CLASS`, which JDI reports as an internal error).
fn handle_rt_source_file(data: &[u8], state: &mut DebugState) -> CommandResult {
    let ref_id = match read_known_class(data, state) {
        Ok(id) => id,
        Err(refused) => return refused,
    };
    match state.class_source_files.get(&ref_id) {
        Some(src) => {
            let mut pw = PayloadWriter::new();
            pw.put_string(src);
            CommandResult::ok(pw.into_bytes())
        }
        None => CommandResult::error(ERR_ABSENT_INFORMATION),
    }
}

/// `ReferenceType.Status` (2/9, wave 23): the class's `ClassStatus`
/// (refreshed before this command, `debug::reads_class_status`).
fn handle_rt_status(data: &[u8], state: &mut DebugState) -> CommandResult {
    let ref_id = match read_known_class(data, state) {
        Ok(id) => id,
        Err(refused) => return refused,
    };
    let mut pw = PayloadWriter::new();
    pw.put_u32_be(class_status(&*state, ref_id));
    CommandResult::ok(pw.into_bytes())
}

/// `ReferenceType.Interfaces` (2/10, wave 23): the direct superinterfaces,
/// in declaration order; none for an array class, as JVMTI
/// `GetImplementedInterfaces` answers. JDI asks it of every class whose
/// methods or fields it lists (`visibleMethods`, `allFields`).
fn handle_rt_interfaces(data: &[u8], state: &mut DebugState) -> CommandResult {
    let ref_id = match read_prepared_class(data, state) {
        Ok(id) => id,
        Err(refused) => return refused,
    };
    let interfaces = state
        .class_details
        .get(&ref_id)
        .map_or(&[][..], |d| d.interfaces.as_slice());
    let mut pw = PayloadWriter::new();
    pw.put_u32_be(interfaces.len() as u32); // Cast: bounded by the u16 `interfaces_count`
    for &id in interfaces {
        pw.put_u64_be(crate::debug::ids::class_to_wire(id));
    }
    CommandResult::ok(pw.into_bytes())
}

/// `ReferenceType.ClassFileVersion` (2/17, wave 23): `majorVersion`,
/// `minorVersion`; `ABSENT_INFORMATION` for an array class (and a class the
/// metadata does not describe), as HotSpot answers.
fn handle_rt_class_file_version(data: &[u8], state: &mut DebugState) -> CommandResult {
    let ref_id = match read_known_class(data, state) {
        Ok(id) => id,
        Err(refused) => return refused,
    };
    match state.class_details.get(&ref_id).and_then(|d| d.version) {
        Some((major, minor)) => {
            let mut pw = PayloadWriter::new();
            pw.put_u32_be(u32::from(major));
            pw.put_u32_be(u32::from(minor));
            CommandResult::ok(pw.into_bytes())
        }
        None => CommandResult::error(ERR_ABSENT_INFORMATION),
    }
}

// ===========================================================================
// ThreadReference command set (11)
// ===========================================================================

fn handle_tr_name(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let tid = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    if let Some(name) = state.thread_names.get(&tid) {
        let mut pw = PayloadWriter::new();
        pw.put_string(name);
        CommandResult::ok(pw.into_bytes())
    } else {
        CommandResult::error(ERR_INVALID_THREAD)
    }
}

fn handle_tr_suspend(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let tid = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    // Wave 11: an id that names no thread is `INVALID_THREAD`. A dead
    // thread is counted, as HotSpot 25 counts it (`SuspendCount` then
    // answers 1; wave 29, `L1/L1W29RawJdwpThreadAndFrameErrors`), but it does
    // not arm `DebuggerGates`: `publish_debugger_gates` asks
    // `any_suspension_of_a_live_thread`, so no live thread is held in the
    // interpreter's suspend point for a thread that will never reach one
    // (the reason wave 11 stopped counting it).
    match thread_status_of(state, tid) {
        None => return CommandResult::error(ERR_INVALID_THREAD),
        Some(_) => state.suspend_thread(tid),
    }
    CommandResult::ok(Vec::new())
}

/// `ThreadReference.SuspendCount` (wave 7): the thread's JDWP suspend count.
/// Wave 15: an id that names no thread is `INVALID_THREAD`, as for every
/// other `ThreadReference` command; it answered 0, or the VM-wide count.
fn handle_tr_suspend_count(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let tid = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };
    let count = match thread_status_of(state, tid) {
        None => return CommandResult::error(ERR_INVALID_THREAD),
        // A dead thread answers its own suspensions (wave 29: HotSpot 25
        // answers 1 after a `Suspend`), never a VM-wide one.
        Some(THREAD_STATUS_ZOMBIE) => state.own_suspend_count(tid),
        Some(_) => state.suspend_count(tid),
    };
    let mut pw = PayloadWriter::new();
    pw.put_u32_be(count);
    CommandResult::ok(pw.into_bytes())
}

fn handle_tr_resume(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let tid = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    // One off the thread's suspend count (wave 7), whichever suspension put
    // it there — its own or a VM-wide one (wave 6: until then a thread resumed
    // on its own after `VirtualMachine.Suspend` stayed parked). Wave 15: an
    // id that names no thread is `INVALID_THREAD` (it was taken as a thread
    // and given a negative count of its own), and a dead thread loses one of
    // its own suspensions only (wave 29, as `Suspend` now counts them).
    match thread_status_of(state, tid) {
        None => return CommandResult::error(ERR_INVALID_THREAD),
        Some(THREAD_STATUS_ZOMBIE) => state.resume_own_suspension(tid),
        Some(_) => state.resume_thread(tid),
    }
    CommandResult::ok(Vec::new())
}

fn handle_tr_status(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let tid = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    // A VM-wide suspension counts too (wave 6: a thread stopped by
    // `VirtualMachine.Suspend` reported "not suspended").
    let is_suspended = state.is_thread_suspended(tid);

    // Wave 11: the thread's real state, refreshed from the thread registry
    // before this command (`debug::populate_thread_metadata`). It answered
    // RUNNING for every id, known or not. A thread the server knows with no
    // recorded state (a bare `DebugState`, an id registered through
    // `IdManager::register_thread`) is RUNNING, as before; an id that names
    // no thread is `INVALID_THREAD`.
    let Some(thread_status) = thread_status_of(state, tid) else {
        return CommandResult::error(ERR_INVALID_THREAD);
    };

    let mut pw = PayloadWriter::new();
    pw.put_u32_be(thread_status);
    // suspendStatus: 1 = SUSPENDED, 0 = not. A dead thread is not suspended
    // (HotSpot answers 0 for a zombie).
    let suspended = is_suspended && thread_status != THREAD_STATUS_ZOMBIE;
    pw.put_u32_be(if suspended { 1 } else { 0 });
    CommandResult::ok(pw.into_bytes())
}

/// The JDWP status of thread `tid` as the server knows it (wave 11): the
/// state refreshed from the registry, else `RUNNING` for a thread it knows
/// otherwise (named, registered in the id manager, or holding a suspension of
/// its own — a bare `DebugState` records no states), else `None`: the id
/// names no thread.
///
/// Wave 44 (lane L1): a suspension of its own, not the VM-wide count. Every
/// id is covered by a `VirtualMachine.Suspend` (or the re-suspension that
/// ends an invocation), so once one was in force an id naming nothing passed
/// for a thread: `ClassType.InvokeMethod` on it answered `INVALID_THREAD`,
/// `ForceEarlyReturn` `OPAQUE_FRAME` and `ArrayType.NewInstance` of it
/// `INVALID_CLASS` where HotSpot 25.0.3 answers `INVALID_OBJECT`
/// (`tools/probes/interp/L1/L1W43RawJdwpObjectErrorAnswers.java`).
pub(crate) fn thread_status_of(state: &DebugState, tid: u64) -> Option<u32> {
    if let Some(&status) = state.thread_statuses.get(&tid) {
        return Some(status);
    }
    let known = state.has_own_suspension(tid)
        || state.thread_names.contains_key(&tid)
        || state.ids.all_thread_ids().iter().any(|t| t.0 == tid);
    known.then_some(THREAD_STATUS_RUNNING)
}

/// Why a command that acts on a LIVE thread (`ThreadReference.Interrupt` /
/// `Stop`, interpreter round i1 wave 24) cannot act on `tid`:
/// `INVALID_THREAD` for an id naming no thread and for a dead one — JVMTI's
/// `THREAD_NOT_ALIVE`, which HotSpot's back end reports as `INVALID_THREAD`.
pub(crate) fn live_thread_refusal(state: &DebugState, tid: u64) -> Option<u16> {
    match thread_status_of(state, tid) {
        None | Some(THREAD_STATUS_ZOMBIE) => Some(ERR_INVALID_THREAD),
        Some(_) => None,
    }
}

/// `ThreadReference.ThreadGroup` (11/5, wave 11): a live thread is in the
/// one "system" group (id 1) the server models (`TopLevelThreadGroups`,
/// `ThreadGroupReference.Children`); a dead one answers the null group, as
/// HotSpot does for a terminated thread. It answered `NOT_IMPLEMENTED`, which
/// JDI's `ThreadReference.threadGroup()` — jdb's `threads` — turns into an
/// `InternalException`.
fn handle_tr_thread_group(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let tid = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };
    let group = match thread_status_of(state, tid) {
        None => return CommandResult::error(ERR_INVALID_THREAD),
        Some(THREAD_STATUS_ZOMBIE) => 0,
        Some(_) => SYSTEM_THREAD_GROUP_ID,
    };
    let mut pw = PayloadWriter::new();
    pw.put_u64_be(group);
    CommandResult::ok(pw.into_bytes())
}

fn handle_tr_frames(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let tid = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };
    // `startFrame` and `length` are JDWP ints; `length` -1 is "all remaining".
    let start_frame = reader.read_u32_be().unwrap_or(0) as i32; // Cast: JDWP int
    let length = reader.read_u32_be().unwrap_or(u32::MAX) as i32; // Cast: JDWP int
    if let Some(refusal) = frames_refusal(state, tid) {
        return CommandResult::error(refusal);
    }
    let frames: &[crate::debug::FrameEntry] = state
        .thread_frames
        .get(&tid)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let count = frames.len();

    // Wave 11: the range is checked as HotSpot's back end checks it
    // (`threadReference.c` `frames`); it was clamped, so a debugger asking
    // past the end got fewer frames and no error.
    //
    // Wave 42 (lane L1), as HotSpot 25.0.3 answers on a stack of 2
    // (`tools/probes/interp/L1/L1W42RawJdwpErrorAnswers.java`): a negative
    // start, or "all remaining" from past the end (3, -1), is
    // `INVALID_INDEX`; a range that runs past the end ((0, 3), (1, 2)) is
    // `INVALID_LENGTH` (it answered `INVALID_INDEX`); an empty range is no
    // frames wherever it starts ((2, 0), (3, 0)).
    let Ok(start) = usize::try_from(start_frame) else {
        return CommandResult::error(ERR_INVALID_INDEX);
    };
    let len = if length == -1 {
        match count.checked_sub(start) {
            Some(len) => len,
            None => return CommandResult::error(ERR_INVALID_INDEX),
        }
    } else {
        match usize::try_from(length) {
            Ok(len) => len,
            Err(_) => return CommandResult::error(ERR_INVALID_LENGTH),
        }
    };
    let slice = if len == 0 {
        &[][..]
    } else {
        match start
            .checked_add(len)
            .and_then(|end| frames.get(start..end))
        {
            Some(slice) => slice,
            None => return CommandResult::error(ERR_INVALID_LENGTH),
        }
    };

    let mut pw = PayloadWriter::new();
    pw.put_u32_be(slice.len() as u32); // Cast: bounded by the frame count
    for entry in slice {
        pw.put_u64_be(entry.frame_id);
        // Location: tag(1) + classID(8) + methodID(8) + index(8) = 25 bytes
        pw.put_u8(state.location_type_tag(entry.class_id)); // wave 15: not always CLASS
        pw.put_u64_be(crate::debug::ids::class_to_wire(entry.class_id));
        pw.put_u64_be(entry.method_id);
        pw.put_u64_be(entry.offset);
    }
    CommandResult::ok(pw.into_bytes())
}

fn handle_tr_frame_count(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let tid = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };
    if let Some(refusal) = frames_refusal(state, tid) {
        return CommandResult::error(refusal);
    }

    let mut pw = PayloadWriter::new();
    let count = state.thread_frames.get(&tid).map_or(0, |f| f.len());
    pw.put_u32_be(count as u32); // Cast: a thread's frame count
    CommandResult::ok(pw.into_bytes())
}

/// Why `ThreadReference.Frames` / `FrameCount` cannot answer for `tid`
/// (wave 11): `INVALID_THREAD` for an id naming no thread,
/// `THREAD_NOT_SUSPENDED` for a thread the debugger has not suspended —
/// HotSpot's `validateSuspendedThread`. Both answered 0 frames or the frames
/// of a snapshot the thread published while blocked (until wave 12 a blocked
/// thread published one whether or not it was suspended), so jdb's `where` on
/// a running thread printed an empty or stale stack instead of "Thread is not
/// suspended".
pub(crate) fn frames_refusal(state: &DebugState, tid: u64) -> Option<u16> {
    match thread_status_of(state, tid) {
        None => Some(ERR_INVALID_THREAD),
        Some(_) if !state.is_thread_suspended(tid) => Some(ERR_THREAD_NOT_SUSPENDED),
        Some(_) => None,
    }
}

/// Why `StackFrame.GetValues` / `ThisObject` cannot answer for frame
/// `frame_id` of `tid` (interpreter round i1 wave 14): the thread's refusal
/// ([`frames_refusal`]), or `INVALID_FRAMEID` for an id the thread's
/// published frames do not hold (a frame from before a resume, or none) —
/// HotSpot's `validateThreadFrame`. Both used to answer as if the frame
/// existed: typed zeros and a null `this`.
fn frame_refusal(state: &DebugState, tid: u64, frame_id: u64) -> Option<u16> {
    frames_refusal(state, tid).or_else(|| {
        let known = state
            .thread_frames
            .get(&tid)
            .is_some_and(|frames| frames.iter().any(|f| f.frame_id == frame_id));
        (!known).then_some(ERR_INVALID_FRAMEID)
    })
}

/// Is frame `frame_id` of `tid` a native method's — published with location
/// -1 ([`crate::debug::NATIVE_FRAME_LOCATION`], interpreter round i1 wave 15,
/// a thread suspended while blocked in a native method)? It has no locals:
/// `GetValues` / `SetValues` answer `OPAQUE_FRAME`, as HotSpot's `GetLocal*`
/// / `SetLocal*` do for a native frame. (`ThisObject` answers null there: JDI
/// never asks it of a native method, `StackFrame.thisObject()` returns null
/// for one.)
pub(crate) fn is_native_frame(state: &DebugState, tid: u64, frame_id: u64) -> bool {
    state.thread_frames.get(&tid).is_some_and(|frames| {
        frames
            .iter()
            .any(|f| f.frame_id == frame_id && f.offset == crate::debug::NATIVE_FRAME_LOCATION)
    })
}

/// Has frame `frame_id` of `tid` no locals a command may read or write: a
/// native method's ([`is_native_frame`]), a compiled activation's, or an
/// interpreter frame's whose body runs compiled (`DebugState::opaque_frames`,
/// interpreter round i1 wave 21, lane L3)? `GetValues` / `SetValues` answer
/// `OPAQUE_FRAME` for it, as HotSpot does for a frame it cannot describe;
/// the stale values such a frame's `Frame` holds are never answered.
pub(crate) fn frame_has_no_readable_locals(state: &DebugState, tid: u64, frame_id: u64) -> bool {
    // Interpreter round i6 wave 120, lane L2 (`CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL`):
    // a compiled activation stays in `opaque_frames` (it has no `Frame`, so no
    // write may reach it and the interpreter-frame indices skip it), but one
    // whose capture read its locals from their home slots published them
    // (`interpreter::publish_frame_snapshot`, `ListingRow::CompiledHome`), and
    // reads answer from them. No other opaque row ever publishes locals.
    (state.opaque_frames.contains_key(&(tid, frame_id))
        && !state.frame_locals.contains_key(&(tid, frame_id)))
        || is_native_frame(state, tid, frame_id)
}

/// Is frame `frame_id` of `tid` a compiled activation (no interpreter frame
/// at all; `DebugState::opaque_frames`)? A step's starting depth and line
/// count only the frames the interpreter's suspend point can see.
fn is_compiled_frame(state: &DebugState, tid: u64, frame_id: u64) -> bool {
    state
        .opaque_frames
        .get(&(tid, frame_id))
        .copied()
        .unwrap_or(false)
}

// ===========================================================================
// EventRequest command set (15)
// ===========================================================================

/// The event kinds an `InstanceOnly` modifier (11) may filter: JDWP allows it
/// on breakpoint, step, method entry and exit, exception, field watch and
/// monitor events (posted since interpreter round i5 wave 103, the monitor
/// object). Interpreter round i4 wave 85, lane L1; field watches since wave
/// 10.
fn instance_filter_allowed(kind: EventKind) -> bool {
    kind.is_monitor()
        || matches!(
            kind,
            EventKind::FieldAccess
                | EventKind::FieldModification
                | EventKind::Breakpoint
                | EventKind::SingleStep
                | EventKind::MethodEntry
                | EventKind::MethodExit
                | EventKind::MethodExitWithReturnValue
                | EventKind::Exception
        )
}

fn handle_er_set(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let event_kind_raw = match reader.read_u8() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };
    let suspend_policy_raw = match reader.read_u8() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };
    let modifier_count = match reader.read_u32_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    let kind = match EventKind::from_u8(event_kind_raw) {
        Some(k) => k,
        None => return CommandResult::error(ERR_INVALID_EVENT_TYPE),
    };
    // Wave 37 (lane L1): any byte, as HotSpot accepts it; a value JDWP does
    // not define suspends the event thread there, measured
    // (`SuspendPolicy::from_wire`). It was refused with `INTERNAL`.
    let suspend_policy = SuspendPolicy::from_wire(suspend_policy_raw);

    let mut modifiers = Vec::new();
    for _ in 0..modifier_count {
        let mod_kind = match reader.read_u8() {
            Ok(v) => v,
            Err(_) => return CommandResult::error(ERR_INTERNAL),
        };
        match mod_kind {
            1 => {
                // Count — used for hit-count filtering on conditional breakpoints.
                let count = reader.read_u32_be().unwrap_or(0) as i32;
                modifiers.push(EventModifier::Count(count));
            }
            2 => {
                // Conditional — expression ID for conditional breakpoints.
                let expr_id = reader.read_u32_be().unwrap_or(0);
                modifiers.push(EventModifier::ConditionalFilter { expr_id });
            }
            3 => {
                // ThreadOnly. Every id a modifier names is decoded from its
                // wire form here (wave 25, `ids::thread_from_wire`,
                // `ids::class_from_wire`: id 0 is JDWP's null).
                // Wave 104: with the `DestroyJavaVM` split (`vm_thread`).
                let thread_id = state.vm_thread(reader.read_u64_be().unwrap_or(0));
                modifiers.push(EventModifier::ThreadOnly { thread_id });
            }
            4 => {
                // ClassOnly
                let class_id =
                    crate::debug::ids::class_from_wire(reader.read_u64_be().unwrap_or(0));
                modifiers.push(EventModifier::ClassOnly { class_id });
            }
            5 => {
                // ClassMatch
                let pattern = reader.read_string().unwrap_or_default();
                modifiers.push(EventModifier::ClassMatch { pattern });
            }
            6 => {
                // ClassExclude
                let pattern = reader.read_string().unwrap_or_default();
                modifiers.push(EventModifier::ClassExclude { pattern });
            }
            7 => {
                // LocationOnly — tag(1) + classID(8) + methodID(8) + index(8)
                let _tag = reader.read_u8().unwrap_or(0);
                let class_id =
                    crate::debug::ids::class_from_wire(reader.read_u64_be().unwrap_or(0));
                let method_id = reader.read_u64_be().unwrap_or(0);
                let offset = reader.read_u64_be().unwrap_or(0);
                modifiers.push(EventModifier::LocationOnly {
                    class_id,
                    method_id,
                    offset,
                });
            }
            8 => {
                // ExceptionOnly — refTypeID(8) + caught(1) + uncaught(1).
                // Wave 10: stored and applied (`EventManager::
                // match_subject_events`); it was read and dropped, so a
                // `catch java.io.IOException` reported nothing at all (no
                // exception event was delivered) and would otherwise have
                // reported every exception, caught or not.
                let (Ok(class_id), Ok(caught), Ok(uncaught)) =
                    (reader.read_u64_be(), reader.read_u8(), reader.read_u8())
                else {
                    return CommandResult::error(ERR_INTERNAL);
                };
                modifiers.push(EventModifier::ExceptionOnly {
                    // 0 is "every exception class" here, not a class.
                    class_id: if class_id == 0 {
                        0
                    } else {
                        crate::debug::ids::class_from_wire(class_id)
                    },
                    caught: caught != 0,
                    uncaught: uncaught != 0,
                });
            }
            9 => {
                // FieldOnly — classID(8) + fieldID(8)
                let class_id =
                    crate::debug::ids::class_from_wire(reader.read_u64_be().unwrap_or(0));
                let field_id = reader.read_u64_be().unwrap_or(0);
                modifiers.push(EventModifier::FieldOnly { class_id, field_id });
            }
            10 => {
                // Step — thread_id(8) + size(4) + depth(4)
                // Wave 104: with the `DestroyJavaVM` split (`vm_thread`).
                let thread_id = state.vm_thread(reader.read_u64_be().unwrap_or(0));
                let size_raw = reader.read_u32_be().unwrap_or(0);
                let depth_raw = reader.read_u32_be().unwrap_or(0);
                // Undefined values, as the JDK back end treats them
                // (interpreter round i4 wave 86, lane L1, from
                // `stepControl.c`, which stores both unchecked): a size other
                // than MIN or LINE gets no line table (`initState` reads one
                // for LINE only), so `fromLine` is -1 and the step completes
                // at the next location it sees, as MIN does; a depth other
                // than INTO / OVER / OUT never enables single stepping
                // (`initEvents`), so the step never reports
                // (`StepDepth::Undefined`). It was read as INTO.
                let size = StepSize::from_u32(size_raw).unwrap_or(StepSize::Min);
                let depth = StepDepth::from_u32(depth_raw).unwrap_or(StepDepth::Undefined);
                modifiers.push(EventModifier::Step {
                    thread_id,
                    size,
                    depth,
                });
            }
            11 if instance_filter_allowed(kind) => {
                // InstanceOnly — objectID(8): on a field watch (wave 10) the
                // object whose field is accessed; on a breakpoint, step,
                // method entry or exit and exception request (interpreter
                // round i4 wave 85, lane L1) the location frame's active
                // `this`, which the suspend point and the exception hook now
                // read (`EventManager::match_location_events_with_instance`),
                // and `canUseInstanceFilters` answers true, as HotSpot's.
                let Ok(object_id) = reader.read_u64_be() else {
                    return CommandResult::error(ERR_INTERNAL);
                };
                modifiers.push(EventModifier::InstanceOnly { object_id });            }
            13 if matches!(kind, EventKind::ThreadStart | EventKind::ThreadDeath) => {
                // PlatformThreadsOnly — no payload (JDWP 21; wave 25). JDI
                // sends it only to a JDWP 19+ target (jdb's default thread
                // requests); JDWP allows it on thread start and death only.
                modifiers.push(EventModifier::PlatformThreadsOnly);
            }
            12 if kind == EventKind::ClassPrepare => {
                // SourceNameMatch — a string (wave 23). JDI offers
                // `addSourceNameFilter` to every JDWP 1.6+ target without a
                // capability check; JDWP allows it on `ClassPrepare` only.
                let Ok(pattern) = reader.read_string() else {
                    return CommandResult::error(ERR_INTERNAL);
                };
                modifiers.push(EventModifier::SourceNameMatch { pattern });            }
            _ => {
                // A modifier this server cannot read (11 InstanceOnly
                // outside a field watch, 12 SourceNameMatch outside a
                // `ClassPrepare` request, which HotSpot refuses too,
                // 13 PlatformThreadsOnly outside a thread start or death
                // request, or anything else): refuse
                // the request, as HotSpot's back end does
                // (`ILLEGAL_ARGUMENT`). Until wave 9 the request was
                // registered with the modifiers read so far, so it reported
                // events its missing filters would have suppressed.
                tracing::warn!(mod_kind, "unknown event modifier kind");
                return CommandResult::error(ERR_ILLEGAL_ARGUMENT);
            }
        }
    }

    if let Some(refusal) = event_request_refusal(kind, &modifiers, state) {
        return CommandResult::error(refusal);
    }

    let req_id = state
        .events
        .set_event_request(kind, suspend_policy, modifiers.clone());
    // Wave 38 (lane L1): an undefined byte is applied as `EVENT_THREAD` and
    // echoed in the event sets the request reports, as HotSpot does.
    state
        .events
        .note_wire_suspend_policy(req_id, suspend_policy_raw);

    // A step is measured from the frame depth its thread is suspended at
    // (the published frame snapshot), so the interpreter's suspend point can
    // tell a callee (OVER) and a return (OUT, and a LINE step finishing its
    // method). Not suspended, not published: the depth stays unknown and
    // every depth reports, the behaviour before the interpreter delivered
    // steps at all.
    if kind == EventKind::SingleStep {
        let step_thread = modifiers.iter().find_map(|m| match m {
            EventModifier::Step { thread_id, .. } => Some(*thread_id),
            _ => None,
        });
        let frames = step_thread.and_then(|t| state.thread_frames.get(&t));
        // Compiled activations a blocked thread's listing splices in (wave
        // 21) are not counted: the suspend point that completes the step
        // measures interpreter frames.
        let read_only: &DebugState = &*state;
        // Interpreter round i7 wave 138, lane L1 (`VmConfig::jdwp_lambda_proxy_steps`):
        // a lambda proxy method's row is a frame too, listed with its locals
        // or (a blocked thread's listing) without them, in
        // `opaque_frames` (`DebugState::proxy_frames`), as the suspend point
        // counts it (`interpreter::debugger_step_depth`).
        let interpreted = |f: &&crate::debug::FrameEntry| {
            !step_thread.is_some_and(|t| {
                is_compiled_frame(read_only, t, f.frame_id)
                    && !read_only.proxy_frames.contains(&(t, f.frame_id))
            })
        };
        let depth = frames.map(|frames| frames.iter().filter(interpreted).count());
        // The line the step starts on (wave 6): the top frame's (first
        // published) location against its `Method.LineTable`. A LINE step
        // then completes on a different line, not on the next line start.
        let start_line = frames
            .and_then(|frames| frames.iter().find(interpreted))
            .and_then(|top| {
                let table = state
                    .method_line_tables
                    .get(&(top.class_id, top.method_id))?;
                let line = table
                    .iter()
                    .filter(|(start, _)| *start <= top.offset)
                    .max_by_key(|(start, _)| *start)?
                    .1;
                Some((top.class_id, top.method_id, line))
            });
        if let Some(depth) = depth {
            state.events.set_initial_frame_depth(req_id, depth);
        }
        if let Some((class_id, method_id, line)) = start_line {
            state
                .events
                .set_initial_line(req_id, class_id, method_id, line);
        }
    }

    // Field watches (wave 10) live in the request table only: the
    // interpreter's field hooks match them there
    // (`EventManager::match_subject_events`). They were also copied into
    // `DebugState::field_*_watchpoints`, a second table nothing delivered
    // from. A breakpoint's `Count` likewise lives only in the request's
    // modifier list, applied in modifier order by
    // `events::location_request_reports` (wave 11 removed the unread
    // `DebugState::breakpoint_conditions` copy). `ConditionalFilter`
    // (modKind 2) is reserved "for the future" by the JDWP spec: accepted,
    // evaluated nowhere.

    let mut pw = PayloadWriter::new();
    pw.put_u32_be(req_id);
    CommandResult::ok(pw.into_bytes())
}

/// Why `EventRequest.Set` refuses a request of `kind` carrying `modifiers`,
/// once each modifier was read (interpreter round i1 wave 29; measured on
/// HotSpot 25.0.3 with `tools/probes/interp/L1/L1W29RawJdwpEventRequestErrors.java`):
///
/// * `ILLEGAL_ARGUMENT` for a modifier the JDWP specification does not allow
///   on the kind: `LocationOnly` outside a breakpoint, a step, a field watch
///   and an exception request; a class filter (`ClassOnly`, `ClassMatch`,
///   `ClassExclude`) on a thread start or death, and `ClassOnly` on a class
///   unload; `ThreadOnly` on a class unload; `ExceptionOnly` outside an exception
///   request; `FieldOnly` outside a field watch; `Step` outside a step;
/// * `INTERNAL` for a `Count` of zero or less, and for a breakpoint without a
///   `LocationOnly` or a field watch without a `FieldOnly` (HotSpot's back
///   end has nothing to install);
/// * `INVALID_LOCATION` for a breakpoint whose code index is not below its
///   method's code length, when the server knows that length.
///
/// All of them were accepted: a request that can never report (a breakpoint
/// without a location, past the end of its method) or whose filter was never
/// applied. JDI validates these itself, so only a raw JDWP client sees the
/// difference.
fn event_request_refusal(
    kind: EventKind,
    modifiers: &[EventModifier],
    state: &DebugState,
) -> Option<u16> {
    let field_watch = matches!(kind, EventKind::FieldAccess | EventKind::FieldModification);
    for modifier in modifiers {
        let allowed = match modifier {
            EventModifier::Count(count) => {
                if *count <= 0 {
                    return Some(ERR_INTERNAL);
                }
                true
            }
            EventModifier::LocationOnly { .. } => {
                field_watch
                    || matches!(
                        kind,
                        EventKind::Breakpoint | EventKind::SingleStep | EventKind::Exception
                    )
            }
            // A class unload names its class by pattern only.
            EventModifier::ClassOnly { .. } => !matches!(
                kind,
                EventKind::ThreadStart | EventKind::ThreadDeath | EventKind::ClassUnload
            ),
            EventModifier::ClassMatch { .. } | EventModifier::ClassExclude { .. } => {
                !matches!(kind, EventKind::ThreadStart | EventKind::ThreadDeath)
            }
            EventModifier::ThreadOnly { .. } => kind != EventKind::ClassUnload,
            EventModifier::ExceptionOnly { .. } => kind == EventKind::Exception,
            EventModifier::FieldOnly { .. } => field_watch,
            EventModifier::Step { .. } => kind == EventKind::SingleStep,
            // Scoped where they are read (`handle_er_set`).
            EventModifier::ConditionalFilter { .. }
            | EventModifier::InstanceOnly { .. }
            | EventModifier::SourceNameMatch { .. }
            | EventModifier::PlatformThreadsOnly => true,
        };
        if !allowed {
            return Some(ERR_ILLEGAL_ARGUMENT);
        }
        // A thread or class filter must name one (interpreter round i1 wave
        // 29; HotSpot 25.0.3 answers `INVALID_OBJECT`, measured with
        // `L1W29RawJdwpEventRequestErrors`): a request filtered to nothing
        // never reports, and the debugger would not learn of its mistake.
        let knows_classes = !state.class_signatures.is_empty();
        let unknown_class = |class_id: &u64| {
            !state.class_signatures.contains_key(class_id)
                && !state.class_methods.contains_key(class_id)
        };
        let names_nothing = match modifier {
            EventModifier::ThreadOnly { thread_id } | EventModifier::Step { thread_id, .. } => {
                thread_status_of(state, *thread_id).is_none()
            }
            EventModifier::ClassOnly { class_id } => unknown_class(class_id),
            // Interpreter round i5 wave 100, lane L1: every class a modifier
            // names is read by HotSpot's back end through
            // `inStream_readClassRef` (`eventFilter.c`), which answers
            // `INVALID_OBJECT` for an id naming nothing -- `ExceptionOnly`
            // (its 0 is "any exception", read as the null reference),
            // `LocationOnly` and `FieldOnly` as `ClassOnly` above. They were
            // accepted, and the request never reported. Only against a class
            // list: a bare `DebugState` (no VM behind it, the unit tests'
            // sessions) knows no class and cannot tell.
            EventModifier::ExceptionOnly { class_id, .. } => {
                *class_id != 0 && knows_classes && unknown_class(class_id)
            }
            EventModifier::LocationOnly { class_id, .. }
            | EventModifier::FieldOnly { class_id, .. } => {
                *class_id != crate::debug::ids::NO_VM_ID
                    && knows_classes
                    && unknown_class(class_id)
            }
            _ => false,
        };
        if names_nothing {
            return Some(ERR_INVALID_OBJECT);
        }
    }
    let location = modifiers.iter().find_map(|m| match m {
        EventModifier::LocationOnly {
            class_id,
            method_id,
            offset,
        } => Some((*class_id, *method_id, *offset)),
        _ => None,
    });
    if kind == EventKind::Breakpoint {
        let Some((class_id, method_id, offset)) = location else {
            return Some(ERR_INTERNAL);
        };
        let past_the_end = state
            .class_details
            .get(&class_id)
            .and_then(|details| details.code_lengths.get(&method_id))
            .is_some_and(|&length| offset >= length);
        if past_the_end {
            return Some(ERR_INVALID_LOCATION);
        }
    }
    if field_watch
        && !modifiers
            .iter()
            .any(|m| matches!(m, EventModifier::FieldOnly { .. }))
    {
        return Some(ERR_INTERNAL);
    }
    None
}

fn handle_er_clear(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let event_kind = reader.read_u8().unwrap_or(0);
    let request_id = match reader.read_u32_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };
    // Wave 29: an event kind JDWP does not define is `INVALID_EVENT_TYPE`,
    // as HotSpot answers it (measured, `L1W29RawJdwpEventRequestErrors`); a
    // request id that names nothing is success, as there.
    if EventKind::from_u8(event_kind).is_none() {
        return CommandResult::error(ERR_INVALID_EVENT_TYPE);
    }

    // Interpreter round i6 wave 119, lane L3: the last request at a location
    // whose breakpoint a redefinition cleared answers `NOT_FOUND`, as
    // HotSpot's back end does (`EventManager::clear_event_request_answer`).
    if !state.events.clear_event_request_answer(request_id) {
        return CommandResult::error(ERR_NOT_FOUND);
    }

    CommandResult::ok(Vec::new())
}

/// `EventRequest.ClearAllBreakpoints` (15/3, wave 9): every breakpoint
/// request goes. It answered `NOT_IMPLEMENTED`, which JDI's
/// `EventRequestManager.deleteAllBreakpoints` turns into an
/// `InternalException`.
fn handle_er_clear_all_breakpoints(state: &mut DebugState) -> CommandResult {
    // Wave 119: `NOT_FOUND` when a redefinition had cleared a breakpoint some
    // request stood at (measured); every request goes either way.
    let cleared = state.events.has_cleared_breakpoint_locations();
    state.events.clear_breakpoints();
    if cleared {
        return CommandResult::error(ERR_NOT_FOUND);
    }
    CommandResult::ok(Vec::new())
}

// ===========================================================================
// StackFrame command set (16)
// ===========================================================================

fn handle_sf_get_values(data: &[u8], state: &mut DebugState) -> CommandResult {
    sf_get_values_tagged(data, state, &|_, _| None)
}

/// The value tag of an object id, and the id to write with it, when the
/// caller can read the heap (`debug::inspect::object_tag` through the object
/// table); `None` keeps the signature byte the debugger sent (`L` or `[`) and
/// the id. The id differs for a live thread (interpreter round i1 wave 24):
/// its value is tagged `t` and carries the thread's id, not an object id.
pub(crate) type ObjectTagOf<'a> = &'a dyn Fn(&DebugState, u64) -> Option<(u8, u64)>;

/// `StackFrame.GetValues`, tagging each non-null object by `tag_of`. Wave 8:
/// the JDWP server serves it through `debug::inspect` with the object's real
/// tag, so a `String` local is `s` and JDI shows its text; it was always the
/// signature byte, `L`, which JDI prints as "instance of java.lang.String".
pub(crate) fn sf_get_values_tagged(
    data: &[u8],
    state: &mut DebugState,
    tag_of: ObjectTagOf<'_>,
) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let thread_id = reader.read_u64_be().unwrap_or(0);
    let frame_id = reader.read_u64_be().unwrap_or(0);
    let slot_count = reader.read_u32_be().unwrap_or(0);
    if let Some(refusal) = frame_refusal(state, thread_id, frame_id) {
        return CommandResult::error(refusal);
    }
    if frame_has_no_readable_locals(state, thread_id, frame_id) {
        return CommandResult::error(ERR_OPAQUE_FRAME);
    }

    // Look up the thread's frame snapshot in DebugState
    let frame_locals = state.get_frame_locals(ThreadId(thread_id), frame_id);
    // The frame's location, for its `LocalVariableTable` (checked above: the
    // thread's snapshot holds the frame).
    let location = state
        .thread_frames
        .get(&thread_id)
        .and_then(|frames| frames.iter().find(|f| f.frame_id == frame_id))
        .map(|f| (f.class_id, f.method_id, f.offset));

    let mut pw = PayloadWriter::new();
    // Object ids this reply hands out: each counts as one sent reference the
    // debugger will later dispose of (`ObjectTable::note_sent`, wave 7).
    let mut sent_ids = Vec::new();
    pw.put_u32_be(slot_count);
    for _ in 0..slot_count {
        let slot = reader.read_u32_be().unwrap_or(0) as usize;
        let sig_byte = reader.read_u8().unwrap_or(0);
        // Wave 29: a tag that names no value type (`V`, 0, any other byte) is
        // `INVALID_TAG`, as HotSpot answers it; it was answered `void`.
        if !is_primitive_tag(sig_byte) && !is_object_tag(sig_byte) {
            return CommandResult::error(ERR_INVALID_TAG);
        }
        // Every object tag reads the slot as a reference (HotSpot 25.0.3
        // answers `s` / `t` / `g` / `l` / `c` for a `String` local with its
        // object and the object's own tag; they were answered `void`).
        let sig_byte = if is_object_tag(sig_byte) && sig_byte != b'[' {
            b'L'
        } else {
            sig_byte
        };
        let held = frame_locals.as_ref().and_then(|locals| locals.get(slot));
        if let Some(refusal) = local_read_refusal(&*state, location, frame_locals, slot, sig_byte) {
            return CommandResult::error(refusal);
        }

        match held {
            // Wave 14: a slot outside the frame is `INVALID_SLOT`, as in
            // HotSpot (JVMTI `GetLocal*`); it was answered a typed zero.
            None if frame_locals.is_some_and(|locals| slot >= locals.len()) => {
                return CommandResult::error(ERR_INVALID_SLOT);
            }
            Some(local_value) => {
                // Write the value with proper type tag based on the signature byte
                match sig_byte {
                    b'I' => {
                        pw.put_u8(b'I'); // tag: int
                        pw.put_u32_be(local_value.as_int().unwrap_or(0) as u32);
                    }
                    b'Z' | b'B' | b'C' | b'S' => {
                        put_narrow_int(&mut pw, sig_byte, local_value.as_int().unwrap_or(0));
                    }
                    b'J' => {
                        pw.put_u8(b'J'); // tag: long
                        pw.put_u64_be(local_value.as_long().unwrap_or(0) as u64);
                    }
                    b'F' => {
                        pw.put_u8(b'F'); // tag: float
                        pw.put_u32_be(local_value.as_float_bits().unwrap_or(0));
                    }
                    b'D' => {
                        pw.put_u8(b'D'); // tag: double
                        pw.put_u64_be(local_value.as_double_bits().unwrap_or(0));
                    }
                    b'L' | b'[' => {
                        // Object or array reference
                        let obj_id = local_value.as_object_id().unwrap_or(0);
                        if obj_id == 0 {
                            pw.put_u8(b'L'); // tag: object (null)
                            pw.put_u64_be(0);
                        } else {
                            // The object's own tag when the heap can be read,
                            // else the signature byte (L or [); a live
                            // thread's id in place of its object's (wave 24),
                            // which hands out no object id.
                            let (tag, wire_id) =
                                tag_of(&*state, obj_id).unwrap_or((sig_byte, obj_id));
                            pw.put_u8(tag);
                            pw.put_u64_be(wire_id);
                            if wire_id == obj_id {
                                sent_ids.push(obj_id);
                            }
                        }
                    }
                    _ => {
                        // Unknown signature — return void as fallback
                        pw.put_u8(b'V');
                    }
                }
            }
            None => {
                // A frame published without its locals — return typed
                // zero/null based on signature
                match sig_byte {
                    b'I' => {
                        pw.put_u8(b'I');
                        pw.put_u32_be(0);
                    }
                    b'Z' | b'B' | b'C' | b'S' => put_narrow_int(&mut pw, sig_byte, 0),
                    b'J' => {
                        pw.put_u8(b'J');
                        pw.put_u64_be(0);
                    }
                    b'F' => {
                        pw.put_u8(b'F');
                        pw.put_u32_be(0);
                    }
                    b'D' => {
                        pw.put_u8(b'D');
                        pw.put_u64_be(0);
                    }
                    b'L' | b'[' => {
                        pw.put_u8(b'L');
                        pw.put_u64_be(0);
                    }
                    _ => {
                        pw.put_u8(b'V');
                    }
                }
            }
        }
    }
    for id in sent_ids {
        state.objects.note_sent(id);
    }
    CommandResult::ok(pw.into_bytes())
}

/// A JDWP value tag of a primitive type (`Z B C S I J F D`).
fn is_primitive_tag(tag: u8) -> bool {
    matches!(tag, b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D')
}

/// A JDWP value tag of a reference: object, array, string, thread, thread
/// group, class loader, class object (interpreter round i1 wave 29; each one
/// measured on HotSpot 25.0.3 by `L1W29RawJdwpThreadAndFrameErrors`).
fn is_object_tag(tag: u8) -> bool {
    matches!(tag, b'L' | b'[' | b's' | b't' | b'g' | b'l' | b'c')
}

/// Why `StackFrame.GetValues` refuses to read slot `slot` as JNI signature
/// byte `sig`: `INVALID_SLOT` or `TYPE_MISMATCH`, or `None` (a signature byte
/// that names no type is left to the caller). HotSpot's `GetLocal*` checks
/// in two stages, which interpreter round i1 wave 42 (lane L1) measured on
/// HotSpot 25.0.3 (`tools/probes/interp/L1/L1W42RawJdwpErrorAnswers.java`)
/// and follows here:
///
/// 1. From what the frame holds (`locals`, the published snapshot; an
///    interpreter slot is only "reference or not", and a slot never written
///    is not a reference): the slot, and a `long`'s or `double`'s second
///    slot, must lie in the frame, and that second slot must not hold a
///    reference (`INVALID_SLOT`: a `long` asked of an `int` parameter
///    followed by a `String` one); a reference asked of a slot that holds a
///    primitive, or a primitive of a slot that holds an object, is
///    `TYPE_MISMATCH`. A null is not taken for a reference here: the
///    snapshot does not tell it from other values.
/// 2. Against the `LocalVariableTable`, when the method has one: the entry
///    that covers the slot at the frame's location (`location`: class,
///    method, bytecode index) must exist (`INVALID_SLOT`: a variable not yet
///    in scope) and be of the kind asked (`TYPE_MISMATCH`, comparing
///    `inspect::local_kind_of_signature` kinds: the int family is one kind).
///    An entry whose scope ends exactly at the location covers the slot (wave
///    100: HotSpot reads the end inclusively; confirm with the page's recipe).
///
/// Wave 15 introduced the type check (a mismatch was answered with a typed
/// zero or null); until wave 42 it checked the table only when an entry
/// covered the slot, and the frame only when none did, so a variable not yet
/// in scope was read as a typed zero, and a `long` over an `int` and a
/// `String` was `TYPE_MISMATCH`.
fn local_read_refusal(
    state: &DebugState,
    location: Option<(u64, u64, u64)>,
    locals: Option<&Vec<crate::debug::LocalValue>>,
    slot: usize,
    sig: u8,
) -> Option<u16> {
    use crate::debug::{LocalValue, VariableInfo};
    if !matches!(
        sig,
        b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D' | b'L' | b'['
    ) {
        return None;
    }
    let want = crate::debug::inspect::local_kind_of_signature(sig);
    let wide = matches!(want, b'J' | b'D');
    let is_reference = |v: &LocalValue| matches!(v, LocalValue::ObjectRef(id) if *id != 0);
    if let Some(locals) = locals {
        if slot.saturating_add(usize::from(wide)) >= locals.len() {
            return Some(ERR_INVALID_SLOT);
        }
        if wide && locals.get(slot + 1).is_some_and(is_reference) {
            return Some(ERR_INVALID_SLOT);
        }
        match locals.get(slot) {
            Some(held) if is_reference(held) && want != b'L' => return Some(ERR_TYPE_MISMATCH),
            Some(
                LocalValue::Int(_)
                | LocalValue::Long(_)
                | LocalValue::Float(_)
                | LocalValue::Double(_),
            ) if want == b'L' => return Some(ERR_TYPE_MISMATCH),
            _ => {}
        }
    }
    let (class_id, method_id, pc) = location?;
    let vars = state.method_variables.get(&(class_id, method_id))?;
    // Widening: a scope length is at most 65535.
    let end = |v: &VariableInfo| v.code_index + v.length as u64;
    let mut in_slot = vars.iter().filter(|v| v.slot == slot && v.code_index <= pc);
    if let Some(covering) = in_slot.clone().find(|v| pc < end(*v)) {
        let declared = covering.signature.as_bytes().first().copied()?;
        return (crate::debug::inspect::local_kind_of_signature(declared) != want)
            .then_some(ERR_TYPE_MISMATCH);
    }
    // Interpreter round i5 wave 100, lane L1: an entry whose scope ends
    // exactly here covers the slot (HotSpot's `check_slot_type_lvt` reads the
    // end inclusively, as `jvmti::native_env::declared_local` now does), so
    // its kind is checked too; it was accepted unchecked.
    if let Some(ending) = in_slot.find(|v| pc == end(*v)) {
        let declared = ending.signature.as_bytes().first().copied()?;
        return (crate::debug::inspect::local_kind_of_signature(declared) != want)
            .then_some(ERR_TYPE_MISMATCH);
    }
    Some(ERR_INVALID_SLOT)
}

/// A `boolean` / `byte` / `char` / `short` local (JNI signature `sig`) as
/// `StackFrame.GetValues` answers it: its own tag and width, as HotSpot's
/// back end writes it (interpreter round i1 wave 12). They went out as an
/// `int` (tag `I`, four bytes), so JDI built an `IntegerValue` for a
/// `boolean` local and jdb printed `flag = 1`, not `true`.
fn put_narrow_int(pw: &mut PayloadWriter, sig: u8, v: i32) {
    pw.put_u8(sig);
    // Casts: JDWP's fixed-width, two's-complement wire encodings.
    match sig {
        b'Z' => pw.put_u8(u8::from(v != 0)),
        b'B' => pw.put_u8(v as u8),
        _ => pw.put_u16_be(v as u16),
    }
}

// ===========================================================================
// ClassType command set (3)
// ===========================================================================

/// `ClassType.Superclass` (3/1): the superclass, or null for
/// `java.lang.Object` and an interface. Interpreter round i1 wave 43: an id
/// naming no class the session knows is `INVALID_CLASS`, as HotSpot answers
/// for an object id (`tools/probes/interp/L1/L1W43RawJdwpObjectErrorAnswers.java`);
/// it answered null.
fn handle_ct_superclass(data: &[u8], state: &mut DebugState) -> CommandResult {
    let class_id = match read_known_class(data, state) {
        Ok(id) => id,
        Err(refused) => return refused,
    };

    // Look up the superclass in our class hierarchy map
    let mut pw = PayloadWriter::new();
    if let Some(&super_id) = state.class_superclass.get(&class_id) {
        pw.put_u64_be(crate::debug::ids::class_to_wire(super_id));
    } else {
        pw.put_u64_be(0); // java.lang.Object or unknown → null superclass
    }
    CommandResult::ok(pw.into_bytes())
}

/// T6.5 — `ClassType.InvokeMethod` (set 3, cmd 3).
///
/// Wire layout (input):
///   refTypeID (8) + threadID (8) + methodID (8)
///   + argCount (4)
///   + argCount × TaggedValue
///   + invokeOptions (4)
///
/// Wire layout (reply):
///   returnValue (TaggedValue) + exception (tag + objectID)
///
/// The method is dispatched via the installed [`DebuggerVmBridge`].  If
/// the bridge is absent we return `ERR_VM_DEAD` — a debugger connected
/// before the VM was fully wired up is a protocol misuse.
fn handle_ct_invoke_method(data: &[u8], state: &mut DebugState) -> CommandResult {
    match prepare_ct_invoke_method(data, state) {
        Ok(call) => call.run(),
        Err(refused) => refused,
    }
}

fn prepare_ct_invoke_method(data: &[u8], state: &DebugState) -> Result<BridgeCall, CommandResult> {
    let mut reader = PayloadReader::new(data);
    let internal = |_| CommandResult::error(ERR_INTERNAL);
    let class_id = reader.read_u64_be().map_err(internal)?;
    let thread_id = reader.read_u64_be().map_err(internal)?;
    let method_id = reader.read_u64_be().map_err(internal)?;
    let arg_count = reader.read_u32_be().map_err(internal)?;

    let mut args = Vec::with_capacity(arg_count as usize);
    for _ in 0..arg_count {
        args.push(read_tagged_value(&mut reader).map_err(internal)?);
    }
    // Wave 9: read, and `INVOKE_SINGLE_THREADED` honoured (it was ignored).
    let invoke_options = reader.read_u32_be().unwrap_or(0);

    // Derive a best-effort return signature from the stored method
    // metadata.  If the method isn't registered yet we fall back to `V`
    // (void) so the bridge is free to tag the result.
    let return_sig =
        return_signature_for(state, class_id, method_id).unwrap_or_else(|| "V".to_string());

    let bridge = state
        .vm_bridge
        .clone()
        .ok_or_else(|| CommandResult::error(ERR_VM_DEAD))?;
    Ok(BridgeCall::Static {
        bridge,
        class_id,
        method_id,
        thread_id,
        args,
        return_sig,
        single_threaded: (invoke_options & INVOKE_SINGLE_THREADED) != 0,
    })
}

/// `ClassType.NewInstance` (3/4, interpreter round i1 wave 24): `clazz`,
/// `thread`, `methodID` (a constructor), `arguments`, `options`; the reply
/// is `newObject` (tagged) and `exception` (tagged), one of them null — the
/// layout of `ClassType.InvokeMethod` with the new object in place of the
/// return value. Runs as an invocation on the named suspended thread
/// (`DebuggerVmBridge::new_instance`).
fn prepare_ct_new_instance(data: &[u8], state: &DebugState) -> Result<BridgeCall, CommandResult> {
    let mut reader = PayloadReader::new(data);
    let internal = |_| CommandResult::error(ERR_INTERNAL);
    let class_id = reader.read_u64_be().map_err(internal)?;
    let thread_id = reader.read_u64_be().map_err(internal)?;
    let method_id = reader.read_u64_be().map_err(internal)?;
    let arg_count = reader.read_u32_be().map_err(internal)?;
    let mut args = Vec::with_capacity(arg_count as usize);
    for _ in 0..arg_count {
        args.push(read_tagged_value(&mut reader).map_err(internal)?);
    }
    let invoke_options = reader.read_u32_be().unwrap_or(0);
    let bridge = state
        .vm_bridge
        .clone()
        .ok_or_else(|| CommandResult::error(ERR_VM_DEAD))?;
    Ok(BridgeCall::NewInstance {
        bridge,
        class_id,
        method_id,
        thread_id,
        args,
        single_threaded: (invoke_options & INVOKE_SINGLE_THREADED) != 0,
    })
}

/// A command that calls into the running VM through the
/// [`DebuggerVmBridge`] — it may run Java, allocate, and take the
/// debug-state lock itself — parsed and resolved against the debugger state
/// by [`prepare_bridge_command`], and [`run`](Self::run) with no lock held
/// (wave 7; see the JDWP server loop in `debug::run_jdwp_server`).
pub enum BridgeCall {
    Static {
        bridge: std::sync::Arc<dyn crate::debug::DebuggerVmBridge>,
        class_id: u64,
        method_id: u64,
        thread_id: u64,
        args: Vec<DebuggerValue>,
        return_sig: String,
        single_threaded: bool,
    },
    Instance {
        bridge: std::sync::Arc<dyn crate::debug::DebuggerVmBridge>,
        receiver_id: u64,
        class_id: u64,
        method_id: u64,
        thread_id: u64,
        args: Vec<DebuggerValue>,
        return_sig: String,
        non_virtual: bool,
        single_threaded: bool,
    },
    NewArray {
        bridge: std::sync::Arc<dyn crate::debug::DebuggerVmBridge>,
        array_type_id: u64,
        length: i32,
    },
    /// `ClassType.NewInstance` (wave 24).
    NewInstance {
        bridge: std::sync::Arc<dyn crate::debug::DebuggerVmBridge>,
        class_id: u64,
        method_id: u64,
        thread_id: u64,
        args: Vec<DebuggerValue>,
        single_threaded: bool,
    },
}

impl BridgeCall {
    /// Does the call run Java (an invocation)? The JDWP server answers such
    /// a call when it returns and serves other commands meanwhile (wave 9).
    pub fn runs_java(&self) -> bool {
        matches!(
            self,
            BridgeCall::Static { .. }
                | BridgeCall::Instance { .. }
                | BridgeCall::NewInstance { .. }
        )
    }

    /// Make the call and build the reply.
    pub fn run(self) -> CommandResult {
        let bridge_error = |e: BridgeError| {
            CommandResult::error(match e {
                BridgeError::InvalidThread => ERR_INVALID_THREAD,
                BridgeError::ThreadNotSuspended => ERR_THREAD_NOT_SUSPENDED,
                BridgeError::InvalidClass => ERR_INVALID_CLASS,
                BridgeError::InvalidMethod => ERR_INVALID_METHODID,
                BridgeError::InvalidObject => ERR_INVALID_OBJECT,
                BridgeError::IllegalArgument => ERR_ILLEGAL_ARGUMENT,
                BridgeError::AlreadyInvoking => ERR_ALREADY_INVOKING,
                BridgeError::Internal => ERR_INTERNAL,
            })
        };
        match self {
            BridgeCall::Static {
                bridge,
                class_id,
                method_id,
                thread_id,
                args,
                return_sig,
                single_threaded,
            } => match bridge.invoke_static(
                class_id,
                method_id,
                thread_id,
                &args,
                &return_sig,
                single_threaded,
            ) {
                Ok(outcome) => CommandResult::ok(encode_invoke_reply(&outcome)),
                Err(e) => bridge_error(e),
            },
            BridgeCall::Instance {
                bridge,
                receiver_id,
                class_id,
                method_id,
                thread_id,
                args,
                return_sig,
                non_virtual,
                single_threaded,
            } => match bridge.invoke_instance(
                receiver_id,
                class_id,
                method_id,
                thread_id,
                &args,
                &return_sig,
                non_virtual,
                single_threaded,
            ) {
                Ok(outcome) => CommandResult::ok(encode_invoke_reply(&outcome)),
                Err(e) => bridge_error(e),
            },
            BridgeCall::NewArray {
                bridge,
                array_type_id,
                length,
            } => match bridge.new_array(array_type_id, length) {
                Ok(array_id) => {
                    let mut pw = PayloadWriter::new();
                    pw.put_u8(b'['); // tag: array
                    pw.put_u64_be(array_id);
                    CommandResult::ok(pw.into_bytes())                }
                Err(e) => bridge_error(e),
            },
            BridgeCall::NewInstance {
                bridge,
                class_id,
                method_id,
                thread_id,
                args,
                single_threaded,
            } => {
                match bridge.new_instance(class_id, method_id, thread_id, &args, single_threaded) {
                    // `newObject` then `exception`: the invocation reply's layout.
                    Ok(outcome) => CommandResult::ok(encode_invoke_reply(&outcome)),
                    Err(e) => bridge_error(e),
                }
            }
        }
    }
}

/// The bridge commands (`ClassType.InvokeMethod`,
/// `ObjectReference.InvokeMethod`, `ArrayType.NewInstance`), prepared under
/// the debug-state lock: `None` for every other command (use [`dispatch`]),
/// `Some(Err(reply))` when the command is refused before any call.
pub fn prepare_bridge_command(
    command_set: u8,
    command: u8,
    data: &[u8],
    state: &DebugState,
) -> Option<Result<BridgeCall, CommandResult>> {
    match (command_set, command) {
        (CS_CLASS_TYPE, CMD_CT_INVOKE_METHOD) => Some(prepare_ct_invoke_method(data, state)),
        // Wave 24. `InterfaceType.InvokeMethod` has `ClassType.InvokeMethod`'s
        // layout, and runs the interface's static method the same way.
        (CS_CLASS_TYPE, CMD_CT_NEW_INSTANCE) => Some(prepare_ct_new_instance(data, state)),
        (CS_INTERFACE_TYPE, CMD_IT_INVOKE_METHOD) => Some(prepare_ct_invoke_method(data, state)),
        (CS_OBJECT_REF, CMD_OR_INVOKE_METHOD) => Some(prepare_or_invoke_method(data, state)),
        (CS_ARRAY_TYPE, CMD_AT_NEW_INSTANCE) => Some(prepare_at_new_instance(data, state)),
        _ => None,
    }
}

// ===========================================================================
// Method command set (6)
// ===========================================================================

/// `ACC_NATIVE` / `ACC_ABSTRACT` in a method's `modBits`.
const ACC_NATIVE: u32 = 0x0100;
const ACC_ABSTRACT: u32 = 0x0400;

/// The `(refType, methodID)` a `Method` command names, read off `data`: the
/// method's metadata, or the reply refusing the command — `INVALID_CLASS`
/// for a class the session does not know, `INVALID_METHODID` for a method
/// the class does not declare (interpreter round i1 wave 23; the commands
/// answered an empty table for both).
fn read_method<'s>(
    data: &[u8],
    state: &'s DebugState,
) -> Result<(u64, &'s MethodInfo), CommandResult> {
    let mut reader = PayloadReader::new(data);
    let internal = |_| CommandResult::error(ERR_INTERNAL);
    let ref_type_id = reader.read_u64_be().map_err(internal)?;
    let method_id = reader.read_u64_be().map_err(internal)?;
    match state.class_methods.get(&ref_type_id) {
        Some(methods) => methods
            .iter()
            .find(|m| m.method_id.0 == method_id)
            .map(|m| (ref_type_id, m))
            .ok_or_else(|| CommandResult::error(ERR_INVALID_METHODID)),
        None if state.class_signatures.contains_key(&ref_type_id) => {
            Err(CommandResult::error(ERR_INVALID_METHODID))
        }
        None => Err(CommandResult::error(ERR_INVALID_CLASS)),
    }
}

/// `Method.LineTable` (6/1): `start`, `end`, then the `LineNumberTable`
/// entries. As HotSpot's back end answers it (interpreter round i1 wave 23):
/// `start` 0 and `end` the index of the method's last bytecode — the range
/// JDI checks every location of the method against (`Location with invalid
/// code index`) — and no entries for a method without line information;
/// `-1`, `-1` and no entries for an abstract method; `NATIVE_METHOD` for a
/// native one. `start` and `end` were the first and LAST LINE ENTRY's
/// indices, so JDI rejected every location past the start of a method's last
/// line (a caller's frame mid-way through it, a step returning into it), and
/// a method without a line table was 0..0.
fn handle_method_line_table(data: &[u8], state: &mut DebugState) -> CommandResult {
    let (ref_type_id, method) = match read_method(data, state) {
        Ok(found) => found,
        Err(refused) => return refused,
    };
    if method.mod_bits & ACC_NATIVE != 0 {
        return CommandResult::error(ERR_NATIVE_METHOD);
    }
    let method_id = method.method_id.0;
    let lines = state
        .method_line_tables
        .get(&(ref_type_id, method_id))
        .map_or(&[][..], Vec::as_slice);
    let code_length = state
        .class_details
        .get(&ref_type_id)
        .and_then(|d| d.code_lengths.get(&method_id))
        .copied();
    // JDWP locations are longs: -1 for "no code".
    let (start, end) = match code_length {
        Some(len) if len > 0 => (0, len - 1),
        Some(_) => (u64::MAX, u64::MAX),
        None if method.mod_bits & ACC_ABSTRACT != 0 => (u64::MAX, u64::MAX),
        // A bare `DebugState` (no code lengths): the line entries' range.
        None => (
            lines.first().map_or(0, |l| l.0),
            lines.last().map_or(0, |l| l.0),
        ),
    };
    let mut pw = PayloadWriter::new();
    pw.put_u64_be(start);
    pw.put_u64_be(end);
    pw.put_u32_be(lines.len() as u32); // Cast: bounded by the code length
    for &(code_index, line_number) in lines {
        pw.put_u64_be(code_index);
        pw.put_u32_be(line_number as u32); // Cast: JDWP int, a u16 line number
    }
    CommandResult::ok(pw.into_bytes())
}

/// JDWP `Method.VariableTable`'s `argCnt`: the frame words the method's
/// arguments use — `this` for an instance method, two for a `long` or
/// `double`, one for anything else. JDI calls a variable an argument when its
/// slot is below it, so the old answer (the variable COUNT) listed every local
/// as a method argument. 0 for a method the metadata does not know.
fn method_arg_words(state: &DebugState, ref_type_id: u64, method_id: u64) -> u32 {
    let Some(m) = state
        .class_methods
        .get(&ref_type_id)
        .and_then(|ms| ms.iter().find(|m| m.method_id.0 == method_id))
    else {
        return 0;
    };
    let mut words: u32 = if m.mod_bits & 0x0008 != 0 { 0 } else { 1 }; // ACC_STATIC
    let params = m
        .signature
        .strip_prefix('(')
        .and_then(|s| s.split(')').next())
        .unwrap_or("");
    let mut chars = params.chars();
    while let Some(c) = chars.next() {
        let mut elem = c;
        while elem == '[' {
            match chars.next() {
                Some(next) => elem = next,
                None => break,
            }
        }
        if elem == 'L' {
            for skip in chars.by_ref() {
                if skip == ';' {
                    break;
                }
            }
        }
        words += if c == 'J' || c == 'D' { 2 } else { 1 };
    }
    words
}

fn handle_method_variable_table(data: &[u8], state: &mut DebugState) -> CommandResult {
    method_variable_table(data, state, false)
}

/// `Method.VariableTable` (6/2) and, with `generic`,
/// `VariableTableWithGeneric` (6/5): `argCnt`, then per `LocalVariableTable`
/// entry `codeIndex, name, signature, [genericSignature,] length, slot` —
/// the generic signature from the `LocalVariableTypeTable` entry of the same
/// slot and scope start, else the empty string. As HotSpot's back end
/// answers them (interpreter round i1 wave 23): `NATIVE_METHOD` for a native
/// method, `ABSENT_INFORMATION` for a method without a `LocalVariableTable`
/// (a class compiled without `-g`, an abstract method), which JDI turns into
/// `AbsentInformationException` — jdb's "Local variable information not
/// available". It answered an empty table, so JDI listed no variables and no
/// arguments and reported none missing; and every generic signature was
/// empty.
fn method_variable_table(data: &[u8], state: &DebugState, generic: bool) -> CommandResult {
    let (ref_type_id, method) = match read_method(data, state) {
        Ok(found) => found,
        Err(refused) => return refused,
    };
    if method.mod_bits & ACC_NATIVE != 0 {
        return CommandResult::error(ERR_NATIVE_METHOD);
    }
    let method_id = method.method_id.0;
    let Some(vars) = state
        .method_variables
        .get(&(ref_type_id, method_id))
        .filter(|vars| !vars.is_empty())
    else {
        return CommandResult::error(ERR_ABSENT_INFORMATION);
    };
    let generics = state
        .class_details
        .get(&ref_type_id)
        .and_then(|d| d.variable_generics.get(&method_id))
        .map_or(&[][..], Vec::as_slice);
    let mut pw = PayloadWriter::new();
    pw.put_u32_be(method_arg_words(state, ref_type_id, method_id)); // argCnt
    pw.put_u32_be(vars.len() as u32); // Cast: bounded by the u16 table length
    for var in vars {
        pw.put_u64_be(var.code_index);
        pw.put_string(&var.name);
        pw.put_string(&var.signature);
        if generic {
            let generic_sig = generics
                .iter()
                .find(|(slot, start, _)| *slot == var.slot && *start == var.code_index)
                .map_or("", |(_, _, sig)| sig.as_str());
            pw.put_string(generic_sig);
        }
        pw.put_u32_be(var.length as u32); // Cast: a u16 scope length
        pw.put_u32_be(var.slot as u32); // Cast: a u16 slot
    }
    CommandResult::ok(pw.into_bytes())
}

fn handle_method_bytecodes(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let ref_type_id = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };
    let method_id = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    let mut pw = PayloadWriter::new();
    if let Some(bytecodes) = state.method_bytecodes.get(&(ref_type_id, method_id)) {
        pw.put_u32_be(bytecodes.len() as u32);
        pw.put_bytes(bytecodes);
    } else {
        pw.put_u32_be(0);
    }
    CommandResult::ok(pw.into_bytes())
}

/// `Method.IsObsolete(refType, methodID)`. Every method id this server hands
/// out for a class names its CURRENT method of that name and descriptor, so
/// only the null id is obsolete: it is what a frame running a body its
/// class's redefinition replaced lists (`debug::frame_method_id`), and what
/// HotSpot's back end answers `true` for (`isMethodObsolete(NULL)`)
/// (interpreter round i1 wave 19, lane L3; it answered `false` always).
fn handle_method_is_obsolete(data: &[u8]) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    if reader.read_u64_be().is_err() {
        return CommandResult::error(ERR_INTERNAL);
    }
    let method_id = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };
    let mut pw = PayloadWriter::new();
    pw.put_u8(u8::from(method_id == super::OBSOLETE_METHOD_ID));
    CommandResult::ok(pw.into_bytes())
}

fn handle_method_variable_table_generic(data: &[u8], state: &mut DebugState) -> CommandResult {
    method_variable_table(data, state, true)
}

// ===========================================================================
// ObjectReference command set (9)
// ===========================================================================

/// T6.5 — `ObjectReference.InvokeMethod` (set 9, cmd 6).
///
/// Wire layout (input):
///   objectID (8) + threadID (8) + classID (8) + methodID (8)
///   + argCount (4) + argCount × TaggedValue + invokeOptions (4)
///
/// `invokeOptions`: [`INVOKE_NONVIRTUAL`] selects non-virtual dispatch,
/// [`INVOKE_SINGLE_THREADED`] keeps the other threads suspended (wave 9).
fn handle_or_invoke_method(data: &[u8], state: &mut DebugState) -> CommandResult {
    match prepare_or_invoke_method(data, state) {
        Ok(call) => call.run(),
        Err(refused) => refused,
    }
}

fn prepare_or_invoke_method(data: &[u8], state: &DebugState) -> Result<BridgeCall, CommandResult> {
    let mut reader = PayloadReader::new(data);
    let internal = |_| CommandResult::error(ERR_INTERNAL);
    let receiver_id = reader.read_u64_be().map_err(internal)?;
    let thread_id = reader.read_u64_be().map_err(internal)?;
    let class_id = reader.read_u64_be().map_err(internal)?;
    let method_id = reader.read_u64_be().map_err(internal)?;
    let arg_count = reader.read_u32_be().map_err(internal)?;

    let mut args = Vec::with_capacity(arg_count as usize);
    for _ in 0..arg_count {
        args.push(read_tagged_value(&mut reader).map_err(internal)?);
    }
    let invoke_options = reader.read_u32_be().unwrap_or(0);
    let non_virtual = (invoke_options & INVOKE_NONVIRTUAL) != 0;

    let return_sig =
        return_signature_for(state, class_id, method_id).unwrap_or_else(|| "V".to_string());

    let bridge = state
        .vm_bridge
        .clone()
        .ok_or_else(|| CommandResult::error(ERR_VM_DEAD))?;
    Ok(BridgeCall::Instance {
        bridge,
        receiver_id,
        class_id,
        method_id,
        thread_id,
        args,
        return_sig,
        non_virtual,
        single_threaded: (invoke_options & INVOKE_SINGLE_THREADED) != 0,
    })
}

// ===========================================================================
// ArrayType command set (4)
// ===========================================================================

/// T6.5 — `ArrayType.NewInstance` (set 4, cmd 1).
///
/// Wire layout (input):
///   arrayTypeID (8) + length (4)
///
/// Reply: newArray (tag byte `[` + objectID).
fn handle_at_new_instance(data: &[u8], state: &mut DebugState) -> CommandResult {
    match prepare_at_new_instance(data, state) {
        Ok(call) => call.run(),
        Err(refused) => refused,
    }
}

fn prepare_at_new_instance(data: &[u8], state: &DebugState) -> Result<BridgeCall, CommandResult> {
    let mut reader = PayloadReader::new(data);
    let internal = |_| CommandResult::error(ERR_INTERNAL);
    let array_type_id = reader.read_u64_be().map_err(internal)?;
    let length = reader.read_u32_be().map_err(internal)? as i32;
    // Interpreter round i1 wave 43 (lane L1): a negative length is
    // `OUT_OF_MEMORY`, as HotSpot 25.0.3 answers it (its JNI `New*Array`
    // throws `NegativeArraySizeException`;
    // `tools/probes/interp/L1/L1W43RawJdwpObjectErrorAnswers.java`). It was
    // `ILLEGAL_ARGUMENT`, from the bridge.
    if length < 0 {
        return Err(CommandResult::error(ERR_OUT_OF_MEMORY));
    }
    let bridge = state
        .vm_bridge
        .clone()
        .ok_or_else(|| CommandResult::error(ERR_VM_DEAD))?;
    Ok(BridgeCall::NewArray {
        bridge,
        array_type_id,
        length,
    })
}

// ===========================================================================
// ThreadGroupReference command set (12)
// ===========================================================================

/// `ThreadGroupReference.Name` (12/1) of the one `system` group a bare
/// `DebugState` models (the JDWP server reads the program's real groups,
/// `debug::inspect::thread_group_command`). Interpreter round i1 wave 24: any
/// other id is `INVALID_THREAD_GROUP`; every id answered `system`.
fn handle_tgr_name(data: &[u8], _state: &mut DebugState) -> CommandResult {
    match PayloadReader::new(data).read_u64_be() {
        Ok(SYSTEM_THREAD_GROUP_ID) => {
            let mut pw = PayloadWriter::new();
            pw.put_string("system");
            CommandResult::ok(pw.into_bytes())
        }
        Ok(_) => CommandResult::error(ERR_INVALID_THREAD_GROUP),
        Err(_) => CommandResult::error(ERR_INTERNAL),
    }
}

fn handle_tgr_parent() -> CommandResult {
    let mut pw = PayloadWriter::new();
    pw.put_u64_be(0); // null parent (top-level group)
    CommandResult::ok(pw.into_bytes())
}

fn handle_tgr_children(state: &mut DebugState) -> CommandResult {
    let mut pw = PayloadWriter::new();
    // Child threads (wave 49: virtual ones only when asked for). Interpreter
    // round i5 wave 102, lane L1: never virtual ones -- HotSpot's back end
    // lists JVMTI `GetThreadGroupChildren`'s threads, which are platform
    // threads only; `includevirtualthreads=y` adds virtual threads to
    // `AllThreads`, not to any group's children.
    let mut thread_ids = listed_thread_ids(state);
    thread_ids.retain(|tid| !state.is_virtual_thread(*tid));
    pw.put_u32_be(thread_ids.len() as u32);
    for tid in &thread_ids {
        pw.put_u64_be(state.wire_thread(*tid));
    }
    // Child thread groups: none
    pw.put_u32_be(0);
    CommandResult::ok(pw.into_bytes())
}

// ===========================================================================
// StackFrame extras (16)
// ===========================================================================

fn handle_sf_this_object(data: &[u8], state: &mut DebugState) -> CommandResult {
    sf_this_object_tagged(data, state, &|_, _| None)
}

/// `StackFrame.ThisObject`, tagging `this` by `tag_of` (see
/// [`sf_get_values_tagged`]).
pub(crate) fn sf_this_object_tagged(
    data: &[u8],
    state: &mut DebugState,
    tag_of: ObjectTagOf<'_>,
) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let thread_id = reader.read_u64_be().unwrap_or(0);
    let frame_id = reader.read_u64_be().unwrap_or(0);
    if let Some(refusal) = frame_refusal(state, thread_id, frame_id) {
        return CommandResult::error(refusal);
    }

    // Try to get slot 0 (this) from the frame. A static method has no
    // `this`: its slot 0 is its first argument (wave 7: that argument used to
    // be reported as `this`).
    // Interpreter round i6 wave 119, lane L1 (stage 2 of
    // `i15-L1-proposal-record-the-running-native-on-the-thread`): a native
    // method's frame answers a null `this`, instance method or not, as
    // HotSpot's back end does (MEASURED on HotSpot 25.0.3,
    // `L1W119RawJdwpNativeThisObject`: `Object.wait0`, an instance native,
    // answers null with no error). A native in the middle of a listing (a
    // spliced JNI native) is also an opaque row, and answered
    // `OPAQUE_FRAME` before.
    if is_native_frame(state, thread_id, frame_id) {
        let mut pw = PayloadWriter::new();
        pw.put_u8(b'L');
        pw.put_u64_be(0);
        return CommandResult::ok(pw.into_bytes());
    }
    let is_static = frame_method_is_static(state, thread_id, frame_id);
    // A compiled activation, or a frame whose body runs compiled (wave 21):
    // its receiver slot is not readable here, so `OPAQUE_FRAME` rather than
    // a stale value. (A static method's null needs no read.) Interpreter
    // round i6 wave 121, lane L1: an opaque row that published its locals --
    // a compiled home's (L2, `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL`) or a
    // stand-in level's (i119-L1, `CRATONVM_DEBUG_STANDIN_LEVEL_LOCALS`) --
    // answers its slot 0, as `GetValues` does
    // ([`frame_has_no_readable_locals`]).
    if !is_static && frame_has_no_readable_locals(state, thread_id, frame_id) {
        return CommandResult::error(ERR_OPAQUE_FRAME);
    }
    let frame_locals = if is_static {
        None
    } else {
        state.get_frame_locals(ThreadId(thread_id), frame_id)
    };
    let mut pw = PayloadWriter::new();
    let mut sent_id = None;
    if let Some(locals) = frame_locals {
        if let Some(local) = locals.first() {
            let obj_id = local.as_object_id().unwrap_or(0);
            if obj_id != 0 {
                // A `Thread` receiver is its thread id (wave 24).
                let (tag, wire_id) = tag_of(&*state, obj_id).unwrap_or((b'L', obj_id));
                pw.put_u8(tag);
                pw.put_u64_be(wire_id);
                if wire_id == obj_id {
                    sent_id = Some(obj_id);
                }
            } else {
                pw.put_u8(b'L');
                pw.put_u64_be(0); // null this (static method)
            }
        } else {
            pw.put_u8(b'L');
            pw.put_u64_be(0);
        }
    } else {
        pw.put_u8(b'L');
        pw.put_u64_be(0);
    }
    if let Some(id) = sent_id {
        state.objects.note_sent(id);
    }
    CommandResult::ok(pw.into_bytes())
}

/// Is the method of `thread_id`'s published frame `frame_id` static
/// (`ACC_STATIC` in the class metadata)? `false` when unknown.
fn frame_method_is_static(state: &DebugState, thread_id: u64, frame_id: u64) -> bool {
    let Some(entry) = state
        .thread_frames
        .get(&thread_id)
        .and_then(|frames| frames.iter().find(|f| f.frame_id == frame_id))
    else {
        return false;
    };
    state
        .class_methods
        .get(&entry.class_id)
        .and_then(|ms| ms.iter().find(|m| m.method_id.0 == entry.method_id))
        .is_some_and(|m| m.mod_bits & 0x0008 != 0) // ACC_STATIC
}

// ---------------------------------------------------------------------------
// T6.5 — Tagged value encode/decode for `InvokeMethod` payloads
// ---------------------------------------------------------------------------

/// Read a JDWP TaggedValue (tag byte + value bytes) from the payload.
pub(crate) fn read_tagged_value(r: &mut PayloadReader<'_>) -> std::io::Result<DebuggerValue> {
    let tag = r.read_u8()?;
    let v = match tag {
        b'V' => DebuggerValue::Void,
        b'Z' => DebuggerValue::Boolean(r.read_u8()?),
        b'B' => DebuggerValue::Byte(r.read_u8()? as i8),
        b'C' => DebuggerValue::Char(r.read_u16_be()?),
        b'S' => DebuggerValue::Short(r.read_u16_be()? as i16),
        b'I' => DebuggerValue::Int(r.read_u32_be()? as i32),
        b'J' => DebuggerValue::Long(r.read_u64_be()? as i64),
        b'F' => DebuggerValue::Float(r.read_u32_be()?),
        b'D' => DebuggerValue::Double(r.read_u64_be()?),
        b'L' => DebuggerValue::Object(r.read_u64_be()?),
        b'[' => DebuggerValue::Array(r.read_u64_be()?),
        b's' => DebuggerValue::String(r.read_u64_be()?),
        b't' => DebuggerValue::Thread(r.read_u64_be()?),
        b'g' => DebuggerValue::ThreadGroup(r.read_u64_be()?),
        b'l' => DebuggerValue::ClassLoader(r.read_u64_be()?),
        b'c' => DebuggerValue::ClassObject(r.read_u64_be()?),
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown JDWP value tag: 0x{:02x}", tag),
            ));
        }
    };
    Ok(v)
}

/// Write a JDWP TaggedValue (tag byte + value bytes) to the payload.
pub(crate) fn write_tagged_value(pw: &mut PayloadWriter, v: &DebuggerValue) {
    pw.put_u8(v.tag());
    match v {
        DebuggerValue::Void => {}
        DebuggerValue::Boolean(b) => pw.put_u8(*b),
        DebuggerValue::Byte(b) => pw.put_u8(*b as u8),
        DebuggerValue::Char(c) => pw.put_u16_be(*c),
        DebuggerValue::Short(s) => pw.put_u16_be(*s as u16),
        DebuggerValue::Int(i) => pw.put_u32_be(*i as u32),
        DebuggerValue::Long(l) => pw.put_u64_be(*l as u64),
        DebuggerValue::Float(f) => pw.put_u32_be(*f),
        DebuggerValue::Double(d) => pw.put_u64_be(*d),
        DebuggerValue::Object(o)
        | DebuggerValue::Array(o)
        | DebuggerValue::String(o)
        | DebuggerValue::Thread(o)
        | DebuggerValue::ThreadGroup(o)
        | DebuggerValue::ClassLoader(o)
        | DebuggerValue::ClassObject(o) => pw.put_u64_be(*o),
    }
}

/// Encode the `returnValue + exception` pair that terminates a JDWP
/// InvokeMethod reply.
fn encode_invoke_reply(outcome: &InvokeOutcome) -> Vec<u8> {
    let mut pw = PayloadWriter::new();
    // returnValue
    write_tagged_value(&mut pw, &outcome.return_value);
    // exception: TaggedObjectID = tag(1) + objectID(8).  Always use the
    // object tag `L`; ID 0 signals "no exception".
    pw.put_u8(b'L');
    pw.put_u64_be(outcome.exception);
    pw.into_bytes()
}

/// Extract the return signature from a method's stored JNI descriptor,
/// or `None` if the method isn't registered.
fn return_signature_for(state: &DebugState, class_id: u64, method_id: u64) -> Option<String> {
    let methods = state.class_methods.get(&class_id)?;
    let method = methods.iter().find(|m| m.method_id.0 == method_id)?;
    // JNI descriptor: "(args)ret" — find the closing ')' and take the rest.
    let paren = method.signature.rfind(')')?;
    Some(method.signature[paren + 1..].to_string())
}

// ---------------------------------------------------------------------------
// Data types used by DebugState for class/method/field metadata
// ---------------------------------------------------------------------------

/// Metadata about a field, stored in [`DebugState::class_fields`].
#[derive(Debug, Clone)]
pub struct FieldInfo {
    pub field_id: FieldId,
    pub name: String,
    pub signature: String,
    pub mod_bits: u32,
}

/// Metadata about a method, stored in [`DebugState::class_methods`].
#[derive(Debug, Clone)]
pub struct MethodInfo {
    pub method_id: MethodId,
    pub name: String,
    pub signature: String,
    pub mod_bits: u32,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
