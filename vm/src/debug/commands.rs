// SPDX-License-Identifier: Apache-2.0
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
pub const ERR_INVALID_CLASS: u16 = 21;
pub const ERR_INVALID_OBJECT: u16 = 20;
pub const ERR_NOT_IMPLEMENTED: u16 = 99;
pub const ERR_VM_DEAD: u16 = 112;
pub const ERR_INVALID_EVENT_TYPE: u16 = 500;
pub const ERR_INTERNAL: u16 = 113;

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
pub const CMD_VM_CREATE_STRING: u8 = 14;
pub const CMD_VM_CAPABILITIES_NEW: u8 = 17;
pub const CMD_VM_ALL_CLASSES_WITH_GENERIC: u8 = 20;

// ReferenceType (2)
pub const CS_REF_TYPE: u8 = 2;
pub const CMD_RT_SIGNATURE: u8 = 1;
pub const CMD_RT_CLASS_LOADER: u8 = 2;
pub const CMD_RT_FIELDS: u8 = 4;
pub const CMD_RT_METHODS: u8 = 5;
pub const CMD_RT_SOURCE_FILE: u8 = 7;

// ThreadReference (11)
pub const CS_THREAD_REF: u8 = 11;
pub const CMD_TR_NAME: u8 = 1;
pub const CMD_TR_SUSPEND: u8 = 2;
pub const CMD_TR_RESUME: u8 = 3;
pub const CMD_TR_STATUS: u8 = 4;
pub const CMD_TR_FRAMES: u8 = 6;
pub const CMD_TR_FRAME_COUNT: u8 = 7;

// EventRequest (15)
pub const CS_EVENT_REQUEST: u8 = 15;
pub const CMD_ER_SET: u8 = 1;
pub const CMD_ER_CLEAR: u8 = 2;

// ClassType (3)
pub const CS_CLASS_TYPE: u8 = 3;
pub const CMD_CT_SUPERCLASS: u8 = 1;
pub const CMD_CT_SET_VALUES: u8 = 2;
pub const CMD_CT_INVOKE_METHOD: u8 = 3;

// ArrayType (4)
pub const CS_ARRAY_TYPE: u8 = 4;
pub const CMD_AT_NEW_INSTANCE: u8 = 1;

// InterfaceType (5)
pub const CS_INTERFACE_TYPE: u8 = 5;

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
pub const CMD_OR_INVOKE_METHOD: u8 = 6;
pub const CMD_OR_DISABLE_COLLECTION: u8 = 7;
pub const CMD_OR_ENABLE_COLLECTION: u8 = 8;
pub const CMD_OR_IS_COLLECTED: u8 = 9;

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

// StackFrame (16)
pub const CS_STACK_FRAME: u8 = 16;
pub const CMD_SF_GET_VALUES: u8 = 1;
pub const CMD_SF_SET_VALUES: u8 = 2;
pub const CMD_SF_THIS_OBJECT: u8 = 3;

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
        (CS_VM, CMD_VM_CREATE_STRING) => handle_vm_create_string(data, state),
        (CS_VM, CMD_VM_CAPABILITIES_NEW) => handle_vm_capabilities_new(),
        (CS_VM, CMD_VM_ALL_CLASSES_WITH_GENERIC) => handle_vm_all_classes_generic(state),

        // -- ReferenceType -------------------------------------------------
        (CS_REF_TYPE, CMD_RT_SIGNATURE) => handle_rt_signature(data, state),
        (CS_REF_TYPE, CMD_RT_CLASS_LOADER) => handle_rt_class_loader(data, state),
        (CS_REF_TYPE, CMD_RT_FIELDS) => handle_rt_fields(data, state),
        (CS_REF_TYPE, CMD_RT_METHODS) => handle_rt_methods(data, state),
        (CS_REF_TYPE, CMD_RT_SOURCE_FILE) => handle_rt_source_file(data, state),

        // -- ThreadReference -----------------------------------------------
        (CS_THREAD_REF, CMD_TR_NAME) => handle_tr_name(data, state),
        (CS_THREAD_REF, CMD_TR_SUSPEND) => handle_tr_suspend(data, state),
        (CS_THREAD_REF, CMD_TR_RESUME) => handle_tr_resume(data, state),
        (CS_THREAD_REF, CMD_TR_STATUS) => handle_tr_status(data, state),
        (CS_THREAD_REF, CMD_TR_FRAMES) => handle_tr_frames(data, state),
        (CS_THREAD_REF, CMD_TR_FRAME_COUNT) => handle_tr_frame_count(data, state),

        // -- ClassType -----------------------------------------------------
        (CS_CLASS_TYPE, CMD_CT_SUPERCLASS) => handle_ct_superclass(data, state),
        (CS_CLASS_TYPE, CMD_CT_SET_VALUES) => CommandResult::ok(Vec::new()), // no-op for readonly debug
        (CS_CLASS_TYPE, CMD_CT_INVOKE_METHOD) => handle_ct_invoke_method(data, state),

        // -- ArrayType -----------------------------------------------------
        (CS_ARRAY_TYPE, CMD_AT_NEW_INSTANCE) => handle_at_new_instance(data, state),

        // -- Method --------------------------------------------------------
        (CS_METHOD, CMD_M_LINE_TABLE) => handle_method_line_table(data, state),
        (CS_METHOD, CMD_M_VARIABLE_TABLE) => handle_method_variable_table(data, state),
        (CS_METHOD, CMD_M_BYTECODES) => handle_method_bytecodes(data, state),
        (CS_METHOD, CMD_M_IS_OBSOLETE) => handle_method_is_obsolete(),
        (CS_METHOD, CMD_M_VARIABLE_TABLE_WITH_GENERIC) => {
            handle_method_variable_table_generic(data, state)
        }

        // -- ObjectReference -----------------------------------------------
        (CS_OBJECT_REF, CMD_OR_REFERENCE_TYPE) => handle_or_reference_type(data, state),
        (CS_OBJECT_REF, CMD_OR_GET_VALUES) => handle_or_get_values(data, state),
        (CS_OBJECT_REF, CMD_OR_SET_VALUES) => CommandResult::ok(Vec::new()),
        (CS_OBJECT_REF, CMD_OR_INVOKE_METHOD) => handle_or_invoke_method(data, state),
        (CS_OBJECT_REF, CMD_OR_DISABLE_COLLECTION) => CommandResult::ok(Vec::new()),
        (CS_OBJECT_REF, CMD_OR_ENABLE_COLLECTION) => CommandResult::ok(Vec::new()),
        (CS_OBJECT_REF, CMD_OR_IS_COLLECTED) => handle_or_is_collected(),

        // -- StringReference -----------------------------------------------
        (CS_STRING_REF, CMD_SR_VALUE) => handle_sr_value(data, state),

        // -- ThreadGroupReference ------------------------------------------
        (CS_THREAD_GROUP_REF, CMD_TGR_NAME) => handle_tgr_name(data, state),
        (CS_THREAD_GROUP_REF, CMD_TGR_PARENT) => handle_tgr_parent(),
        (CS_THREAD_GROUP_REF, CMD_TGR_CHILDREN) => handle_tgr_children(state),

        // -- ArrayReference ------------------------------------------------
        (CS_ARRAY_REF, CMD_AR_LENGTH) => handle_ar_length(data, state),
        (CS_ARRAY_REF, CMD_AR_GET_VALUES) => handle_ar_get_values(data, state),
        (CS_ARRAY_REF, CMD_AR_SET_VALUES) => CommandResult::ok(Vec::new()),

        // -- ClassLoaderReference ------------------------------------------
        (CS_CLASSLOADER_REF, CMD_CLR_VISIBLE_CLASSES) => handle_clr_visible_classes(data, state),

        // -- EventRequest --------------------------------------------------
        (CS_EVENT_REQUEST, CMD_ER_SET) => handle_er_set(data, state),
        (CS_EVENT_REQUEST, CMD_ER_CLEAR) => handle_er_clear(data, state),

        // -- StackFrame ----------------------------------------------------
        (CS_STACK_FRAME, CMD_SF_GET_VALUES) => handle_sf_get_values(data, state),
        (CS_STACK_FRAME, CMD_SF_SET_VALUES) => CommandResult::ok(Vec::new()),
        (CS_STACK_FRAME, CMD_SF_THIS_OBJECT) => handle_sf_this_object(data, state),

        // -- ClassObjectReference ------------------------------------------
        (CS_CLASS_OBJ_REF, CMD_COR_REFLECTED_TYPE) => handle_cor_reflected_type(data, state),

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

    // Look up in our class registry.
    let mut pw = PayloadWriter::new();
    if let Some(&(ref_type_id, type_tag)) = state.loaded_classes.get(&signature) {
        pw.put_u32_be(1); // count
        pw.put_u8(type_tag); // refTypeTag: 1=class, 2=interface, 3=array
        pw.put_u64_be(ref_type_id);
        pw.put_u32_be(3); // status: initialized
    } else {
        pw.put_u32_be(0); // not found
    }
    CommandResult::ok(pw.into_bytes())
}

fn handle_vm_all_threads(state: &mut DebugState) -> CommandResult {
    let ids = state.ids.all_thread_ids();
    let mut pw = PayloadWriter::new();
    pw.put_u32_be(ids.len() as u32);
    for tid in &ids {
        pw.put_u64_be(tid.0);
    }
    CommandResult::ok(pw.into_bytes())
}

fn handle_vm_top_level_thread_groups() -> CommandResult {
    let mut pw = PayloadWriter::new();
    pw.put_u32_be(1); // one top-level thread group
    pw.put_u64_be(1); // "system" thread group ID
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
    state.suspended = true;
    CommandResult::ok(Vec::new())
}

fn handle_vm_resume(state: &mut DebugState) -> CommandResult {
    state.suspended = false;
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
    // [0] canWatchFieldModification
    pw.put_u8(1); // true — T6.4.4
                  // [1] canWatchFieldAccess
    pw.put_u8(1); // true — T6.4.4
                  // [2] canGetBytecodes
    pw.put_u8(0);
    // [3] canGetSyntheticAttribute
    pw.put_u8(0);
    // [4] canGetOwnedMonitorInfo
    pw.put_u8(0);
    // [5] canGetCurrentContendedMonitor
    pw.put_u8(0);
    // [6] canGetMonitorInfo
    pw.put_u8(0);
    CommandResult::ok(pw.into_bytes())
}

fn handle_vm_create_string(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let value = match reader.read_string() {
        Ok(s) => s,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };
    // Assign a new object ID to represent this string.
    let id = state.ids.register_object(state.next_string_handle);
    state.string_values.insert(id.0, value);
    state.next_string_handle += 1;
    let mut pw = PayloadWriter::new();
    pw.put_u64_be(id.0);
    CommandResult::ok(pw.into_bytes())
}

fn handle_vm_capabilities_new() -> CommandResult {
    let mut pw = PayloadWriter::new();
    // 32 boolean capabilities (the "new" set).  All false for now.
    for _ in 0..32 {
        pw.put_u8(0);
    }
    CommandResult::ok(pw.into_bytes())
}

fn handle_vm_all_classes_generic(state: &mut DebugState) -> CommandResult {
    let mut pw = PayloadWriter::new();
    let count = state.loaded_classes.len() as u32;
    pw.put_u32_be(count);
    for (sig, &(ref_type_id, type_tag)) in &state.loaded_classes {
        pw.put_u8(type_tag); // refTypeTag
        pw.put_u64_be(ref_type_id); // typeID
        pw.put_string(sig); // signature
        pw.put_string(""); // genericSignature
        pw.put_u32_be(3); // status: initialized
    }
    CommandResult::ok(pw.into_bytes())
}

// ===========================================================================
// ReferenceType command set (2)
// ===========================================================================

fn handle_rt_signature(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let ref_id = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    if let Some(sig) = state.class_signatures.get(&ref_id) {
        let mut pw = PayloadWriter::new();
        pw.put_string(sig);
        CommandResult::ok(pw.into_bytes())
    } else {
        CommandResult::error(ERR_INVALID_CLASS)
    }
}

fn handle_rt_class_loader(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let ref_id = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    if state.class_signatures.contains_key(&ref_id) {
        let mut pw = PayloadWriter::new();
        pw.put_u64_be(0); // null classloader = bootstrap
        CommandResult::ok(pw.into_bytes())
    } else {
        CommandResult::error(ERR_INVALID_CLASS)
    }
}

fn handle_rt_fields(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let ref_id = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    if let Some(fields) = state.class_fields.get(&ref_id) {
        let mut pw = PayloadWriter::new();
        pw.put_u32_be(fields.len() as u32);
        for f in fields {
            pw.put_u64_be(f.field_id.0);
            pw.put_string(&f.name);
            pw.put_string(&f.signature);
            pw.put_u32_be(f.mod_bits);
        }
        CommandResult::ok(pw.into_bytes())
    } else {
        // Return empty list if unknown class.
        let mut pw = PayloadWriter::new();
        pw.put_u32_be(0);
        CommandResult::ok(pw.into_bytes())
    }
}

fn handle_rt_methods(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let ref_id = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    if let Some(methods) = state.class_methods.get(&ref_id) {
        let mut pw = PayloadWriter::new();
        pw.put_u32_be(methods.len() as u32);
        for m in methods {
            pw.put_u64_be(m.method_id.0);
            pw.put_string(&m.name);
            pw.put_string(&m.signature);
            pw.put_u32_be(m.mod_bits);
        }
        CommandResult::ok(pw.into_bytes())
    } else {
        let mut pw = PayloadWriter::new();
        pw.put_u32_be(0);
        CommandResult::ok(pw.into_bytes())
    }
}

fn handle_rt_source_file(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let ref_id = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    if let Some(src) = state.class_source_files.get(&ref_id) {
        let mut pw = PayloadWriter::new();
        pw.put_string(src);
        CommandResult::ok(pw.into_bytes())
    } else {
        CommandResult::error(ERR_INVALID_CLASS)
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

    state.suspended_threads.insert(tid);
    CommandResult::ok(Vec::new())
}

fn handle_tr_resume(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let tid = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    state.suspended_threads.remove(&tid);
    CommandResult::ok(Vec::new())
}

fn handle_tr_status(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let tid = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    let is_suspended = state.suspended_threads.contains(&tid);

    let mut pw = PayloadWriter::new();
    // threadStatus: 1 = RUNNING
    pw.put_u32_be(1);
    // suspendStatus: 1 = SUSPENDED, 0 = not
    pw.put_u32_be(if is_suspended { 1 } else { 0 });
    CommandResult::ok(pw.into_bytes())
}

fn handle_tr_frames(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let tid = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };
    let start_frame = reader.read_u32_be().unwrap_or(0) as usize;
    let length = reader.read_u32_be().unwrap_or(u32::MAX) as usize;

    let mut pw = PayloadWriter::new();

    if let Some(frames) = state.thread_frames.get(&tid) {
        // Clamp range
        let end = std::cmp::min(start_frame + length, frames.len());
        let slice = if start_frame < frames.len() {
            &frames[start_frame..end]
        } else {
            &[]
        };
        pw.put_u32_be(slice.len() as u32);
        for entry in slice {
            pw.put_u64_be(entry.frame_id);
            // Location: tag(1) + classID(8) + methodID(8) + index(8) = 25 bytes
            pw.put_u8(1); // TypeTag: CLASS
            pw.put_u64_be(entry.class_id);
            pw.put_u64_be(entry.method_id);
            pw.put_u64_be(entry.offset);
        }
    } else {
        pw.put_u32_be(0);
    }
    CommandResult::ok(pw.into_bytes())
}

fn handle_tr_frame_count(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let tid = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    let mut pw = PayloadWriter::new();
    let count = state.thread_frames.get(&tid).map_or(0, |f| f.len());
    pw.put_u32_be(count as u32);
    CommandResult::ok(pw.into_bytes())
}

// ===========================================================================
// EventRequest command set (15)
// ===========================================================================

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
    let suspend_policy = match SuspendPolicy::from_u8(suspend_policy_raw) {
        Some(sp) => sp,
        None => return CommandResult::error(ERR_INTERNAL),
    };

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
                // ThreadOnly
                let thread_id = reader.read_u64_be().unwrap_or(0);
                modifiers.push(EventModifier::ThreadOnly { thread_id });
            }
            4 => {
                // ClassOnly
                let class_id = reader.read_u64_be().unwrap_or(0);
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
                let class_id = reader.read_u64_be().unwrap_or(0);
                let method_id = reader.read_u64_be().unwrap_or(0);
                let offset = reader.read_u64_be().unwrap_or(0);
                modifiers.push(EventModifier::LocationOnly {
                    class_id,
                    method_id,
                    offset,
                });
            }
            8 => {
                // ExceptionOnly — refTypeID(8) + caught(1) + uncaught(1)
                let _exception_class = reader.read_u64_be().unwrap_or(0);
                let _caught = reader.read_u8().unwrap_or(0);
                let _uncaught = reader.read_u8().unwrap_or(0);
                // Not stored yet — skip.
            }
            9 => {
                // FieldOnly — classID(8) + fieldID(8)
                let class_id = reader.read_u64_be().unwrap_or(0);
                let field_id = reader.read_u64_be().unwrap_or(0);
                modifiers.push(EventModifier::FieldOnly { class_id, field_id });
            }
            10 => {
                // Step — thread_id(8) + size(4) + depth(4)
                let thread_id = reader.read_u64_be().unwrap_or(0);
                let size_raw = reader.read_u32_be().unwrap_or(0);
                let depth_raw = reader.read_u32_be().unwrap_or(0);
                let size = StepSize::from_u32(size_raw).unwrap_or(StepSize::Min);
                let depth = StepDepth::from_u32(depth_raw).unwrap_or(StepDepth::Into);
                modifiers.push(EventModifier::Step {
                    thread_id,
                    size,
                    depth,
                });
            }
            _ => {
                // Unknown modifier — skip.  In a real implementation we would
                // need to know the size, but for now we just break.
                tracing::warn!(mod_kind, "unknown event modifier kind");
                break;
            }
        }
    }

    let req_id = state
        .events
        .set_event_request(kind, suspend_policy, modifiers.clone());

    // -- T6.4.4: Store watchpoints for field access/modification events -------
    if kind == EventKind::FieldAccess || kind == EventKind::FieldModification {
        for m in &modifiers {
            if let EventModifier::FieldOnly { class_id, field_id } = m {
                let wp = crate::debug::FieldWatchpoint {
                    class_id: *class_id,
                    field_id: *field_id,
                    request_id: req_id,
                };
                if kind == EventKind::FieldAccess {
                    state.field_access_watchpoints.push(wp);
                } else {
                    state.field_modification_watchpoints.push(wp);
                }
            }
        }
    }

    // -- T6.4.5: Store conditional breakpoint info ---------------------------
    if kind == EventKind::Breakpoint {
        let mut condition: Option<String> = None;
        let mut hit_count_filter: Option<crate::debug::HitCountFilter> = None;

        for m in &modifiers {
            match m {
                EventModifier::Count(count) => {
                    // JDWP Count modifier: fire only on the Nth hit.
                    // Map to HitCountMode::Equal as the standard interpretation.
                    hit_count_filter = Some(crate::debug::HitCountFilter {
                        mode: crate::debug::HitCountMode::Equal,
                        count: *count as u32,
                    });
                }
                EventModifier::ConditionalFilter { expr_id } => {
                    condition = Some(format!("expr:{}", expr_id));
                }
                _ => {}
            }
        }

        if condition.is_some() || hit_count_filter.is_some() {
            state.breakpoint_conditions.insert(
                req_id,
                crate::debug::BreakpointCondition {
                    condition,
                    hit_count: 0,
                    hit_count_filter,
                },
            );
        }
    }

    let mut pw = PayloadWriter::new();
    pw.put_u32_be(req_id);
    CommandResult::ok(pw.into_bytes())
}

fn handle_er_clear(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let _event_kind = reader.read_u8().unwrap_or(0);
    let request_id = match reader.read_u32_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    state.events.clear_event_request(request_id);

    // T6.4.4: Clean up watchpoints associated with this request.
    state
        .field_access_watchpoints
        .retain(|w| w.request_id != request_id);
    state
        .field_modification_watchpoints
        .retain(|w| w.request_id != request_id);

    // T6.4.5: Clean up breakpoint conditions.
    state.breakpoint_conditions.remove(&request_id);

    CommandResult::ok(Vec::new())
}

// ===========================================================================
// StackFrame command set (16)
// ===========================================================================

fn handle_sf_get_values(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let thread_id = reader.read_u64_be().unwrap_or(0);
    let frame_id = reader.read_u64_be().unwrap_or(0);
    let slot_count = reader.read_u32_be().unwrap_or(0);

    // Look up the thread's frame snapshot in DebugState
    let frame_locals = state.get_frame_locals(ThreadId(thread_id), frame_id);

    let mut pw = PayloadWriter::new();
    pw.put_u32_be(slot_count);
    for _ in 0..slot_count {
        let slot = reader.read_u32_be().unwrap_or(0) as usize;
        let sig_byte = reader.read_u8().unwrap_or(0);

        match frame_locals.as_ref().and_then(|locals| locals.get(slot)) {
            Some(local_value) => {
                // Write the value with proper type tag based on the signature byte
                match sig_byte {
                    b'I' | b'Z' | b'B' | b'C' | b'S' => {
                        pw.put_u8(b'I'); // tag: int
                        pw.put_u32_be(local_value.as_int().unwrap_or(0) as u32);
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
                            pw.put_u8(sig_byte); // preserve L or [
                            pw.put_u64_be(obj_id);
                        }
                    }
                    _ => {
                        // Unknown signature — return void as fallback
                        pw.put_u8(b'V');
                    }
                }
            }
            None => {
                // Slot not available — return typed zero/null based on signature
                match sig_byte {
                    b'I' | b'Z' | b'B' | b'C' | b'S' => {
                        pw.put_u8(b'I');
                        pw.put_u32_be(0);
                    }
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
    CommandResult::ok(pw.into_bytes())
}

// ===========================================================================
// ClassType command set (3)
// ===========================================================================

fn handle_ct_superclass(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let class_id = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    // Look up the superclass in our class hierarchy map
    let mut pw = PayloadWriter::new();
    if let Some(&super_id) = state.class_superclass.get(&class_id) {
        pw.put_u64_be(super_id);
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
    let mut reader = PayloadReader::new(data);
    let class_id = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };
    let thread_id = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };
    let method_id = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };
    let arg_count = match reader.read_u32_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    let mut args = Vec::with_capacity(arg_count as usize);
    for _ in 0..arg_count {
        match read_tagged_value(&mut reader) {
            Ok(v) => args.push(v),
            Err(_) => return CommandResult::error(ERR_INTERNAL),
        }
    }
    let _invoke_options = reader.read_u32_be().unwrap_or(0);

    // Derive a best-effort return signature from the stored method
    // metadata.  If the method isn't registered yet we fall back to `V`
    // (void) so the bridge is free to tag the result.
    let return_sig =
        return_signature_for(state, class_id, method_id).unwrap_or_else(|| "V".to_string());

    let bridge = match state.vm_bridge.clone() {
        Some(b) => b,
        None => return CommandResult::error(ERR_VM_DEAD),
    };

    let outcome = match bridge.invoke_static(class_id, method_id, thread_id, &args, &return_sig) {
        Ok(o) => o,
        Err(BridgeError::InvalidThread) => return CommandResult::error(ERR_INVALID_THREAD),
        Err(BridgeError::InvalidClass) => return CommandResult::error(ERR_INVALID_CLASS),
        Err(BridgeError::InvalidObject) => return CommandResult::error(ERR_INVALID_OBJECT),
        Err(BridgeError::Internal) => return CommandResult::error(ERR_INTERNAL),
    };

    CommandResult::ok(encode_invoke_reply(&outcome))
}

// ===========================================================================
// Method command set (6)
// ===========================================================================

fn handle_method_line_table(data: &[u8], state: &mut DebugState) -> CommandResult {
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
    if let Some(lines) = state.method_line_tables.get(&(ref_type_id, method_id)) {
        let start = lines.first().map(|l| l.0).unwrap_or(0);
        let end = lines.last().map(|l| l.0).unwrap_or(0);
        pw.put_u64_be(start); // start
        pw.put_u64_be(end); // end
        pw.put_u32_be(lines.len() as u32);
        for &(code_index, line_number) in lines {
            pw.put_u64_be(code_index);
            pw.put_u32_be(line_number as u32);
        }
    } else {
        // Return a minimal line table (start=0, end=0, no entries)
        pw.put_u64_be(0);
        pw.put_u64_be(0);
        pw.put_u32_be(0);
    }
    CommandResult::ok(pw.into_bytes())
}

fn handle_method_variable_table(data: &[u8], state: &mut DebugState) -> CommandResult {
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
    if let Some(vars) = state.method_variables.get(&(ref_type_id, method_id)) {
        pw.put_u32_be(vars.len() as u32); // argCnt (approximate)
        pw.put_u32_be(vars.len() as u32); // slots
        for var in vars {
            pw.put_u64_be(var.code_index);
            pw.put_string(&var.name);
            pw.put_string(&var.signature);
            pw.put_u32_be(var.length as u32);
            pw.put_u32_be(var.slot as u32);
        }
    } else {
        pw.put_u32_be(0);
        pw.put_u32_be(0);
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

fn handle_method_is_obsolete() -> CommandResult {
    let mut pw = PayloadWriter::new();
    pw.put_u8(0); // isObsolete = false
    CommandResult::ok(pw.into_bytes())
}

fn handle_method_variable_table_generic(data: &[u8], state: &mut DebugState) -> CommandResult {
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
    if let Some(vars) = state.method_variables.get(&(ref_type_id, method_id)) {
        pw.put_u32_be(vars.len() as u32);
        pw.put_u32_be(vars.len() as u32);
        for var in vars {
            pw.put_u64_be(var.code_index);
            pw.put_string(&var.name);
            pw.put_string(&var.signature);
            pw.put_string(""); // genericSignature
            pw.put_u32_be(var.length as u32);
            pw.put_u32_be(var.slot as u32);
        }
    } else {
        pw.put_u32_be(0);
        pw.put_u32_be(0);
    }
    CommandResult::ok(pw.into_bytes())
}

// ===========================================================================
// ObjectReference command set (9)
// ===========================================================================

fn handle_or_reference_type(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let obj_id = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    let mut pw = PayloadWriter::new();
    if let Some(&class_id) = state.object_class_map.get(&obj_id) {
        pw.put_u8(1); // refTypeTag: CLASS
        pw.put_u64_be(class_id);
    } else {
        // Unknown object — return a generic class reference
        pw.put_u8(1);
        pw.put_u64_be(0);
    }
    CommandResult::ok(pw.into_bytes())
}

fn handle_or_get_values(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let _obj_id = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };
    let field_count = reader.read_u32_be().unwrap_or(0);

    // For each field, return a null/zero value since we don't have
    // direct heap access from the debug protocol layer
    let mut pw = PayloadWriter::new();
    pw.put_u32_be(field_count);
    for _ in 0..field_count {
        let _field_id = reader.read_u64_be().unwrap_or(0);
        pw.put_u8(b'L'); // tag: object (null)
        pw.put_u64_be(0);
    }
    CommandResult::ok(pw.into_bytes())
}

fn handle_or_is_collected() -> CommandResult {
    let mut pw = PayloadWriter::new();
    pw.put_u8(0); // isCollected = false
    CommandResult::ok(pw.into_bytes())
}

/// T6.5 — `ObjectReference.InvokeMethod` (set 9, cmd 6).
///
/// Wire layout (input):
///   objectID (8) + threadID (8) + classID (8) + methodID (8)
///   + argCount (4) + argCount × TaggedValue + invokeOptions (4)
///
/// `invokeOptions` bit 2 (value 2) selects non-virtual dispatch.
fn handle_or_invoke_method(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let object_id = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };
    let thread_id = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };
    let class_id = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };
    let method_id = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };
    let arg_count = match reader.read_u32_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    let mut args = Vec::with_capacity(arg_count as usize);
    for _ in 0..arg_count {
        match read_tagged_value(&mut reader) {
            Ok(v) => args.push(v),
            Err(_) => return CommandResult::error(ERR_INTERNAL),
        }
    }
    let invoke_options = reader.read_u32_be().unwrap_or(0);
    let non_virtual = (invoke_options & 2) != 0;

    let return_sig =
        return_signature_for(state, class_id, method_id).unwrap_or_else(|| "V".to_string());

    let bridge = match state.vm_bridge.clone() {
        Some(b) => b,
        None => return CommandResult::error(ERR_VM_DEAD),
    };

    let outcome = match bridge.invoke_instance(
        object_id,
        class_id,
        method_id,
        thread_id,
        &args,
        &return_sig,
        non_virtual,
    ) {
        Ok(o) => o,
        Err(BridgeError::InvalidThread) => return CommandResult::error(ERR_INVALID_THREAD),
        Err(BridgeError::InvalidClass) => return CommandResult::error(ERR_INVALID_CLASS),
        Err(BridgeError::InvalidObject) => return CommandResult::error(ERR_INVALID_OBJECT),
        Err(BridgeError::Internal) => return CommandResult::error(ERR_INTERNAL),
    };

    CommandResult::ok(encode_invoke_reply(&outcome))
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
    let mut reader = PayloadReader::new(data);
    let array_type_id = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };
    let length = match reader.read_u32_be() {
        Ok(v) => v as i32,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    let bridge = match state.vm_bridge.clone() {
        Some(b) => b,
        None => return CommandResult::error(ERR_VM_DEAD),
    };

    let array_id = match bridge.new_array(array_type_id, length) {
        Ok(id) => id,
        Err(BridgeError::InvalidClass) => return CommandResult::error(ERR_INVALID_CLASS),
        Err(BridgeError::InvalidObject) => return CommandResult::error(ERR_INVALID_OBJECT),
        Err(BridgeError::InvalidThread) => return CommandResult::error(ERR_INVALID_THREAD),
        Err(BridgeError::Internal) => return CommandResult::error(ERR_INTERNAL),
    };

    let mut pw = PayloadWriter::new();
    pw.put_u8(b'['); // tag: array
    pw.put_u64_be(array_id);
    CommandResult::ok(pw.into_bytes())
}

// ===========================================================================
// StringReference command set (10)
// ===========================================================================

fn handle_sr_value(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let string_id = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    let mut pw = PayloadWriter::new();
    if let Some(value) = state.string_values.get(&string_id) {
        pw.put_string(value);
    } else {
        pw.put_string(""); // empty string for unknown ID
    }
    CommandResult::ok(pw.into_bytes())
}

// ===========================================================================
// ThreadGroupReference command set (12)
// ===========================================================================

fn handle_tgr_name(_data: &[u8], _state: &mut DebugState) -> CommandResult {
    // We only have one thread group: "system"
    let mut pw = PayloadWriter::new();
    pw.put_string("system");
    CommandResult::ok(pw.into_bytes())
}

fn handle_tgr_parent() -> CommandResult {
    let mut pw = PayloadWriter::new();
    pw.put_u64_be(0); // null parent (top-level group)
    CommandResult::ok(pw.into_bytes())
}

fn handle_tgr_children(state: &mut DebugState) -> CommandResult {
    let mut pw = PayloadWriter::new();
    // Child threads
    let thread_ids = state.ids.all_thread_ids();
    pw.put_u32_be(thread_ids.len() as u32);
    for tid in &thread_ids {
        pw.put_u64_be(tid.0);
    }
    // Child thread groups: none
    pw.put_u32_be(0);
    CommandResult::ok(pw.into_bytes())
}

// ===========================================================================
// ArrayReference command set (13)
// ===========================================================================

fn handle_ar_length(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let arr_id = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    let mut pw = PayloadWriter::new();
    if let Some(&length) = state.array_lengths.get(&arr_id) {
        pw.put_u32_be(length as u32);
    } else {
        pw.put_u32_be(0);
    }
    CommandResult::ok(pw.into_bytes())
}

fn handle_ar_get_values(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let arr_id = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };
    let first_index = reader.read_u32_be().unwrap_or(0);
    let length = reader.read_u32_be().unwrap_or(0);

    let mut pw = PayloadWriter::new();
    // Return array region as typed values
    if let Some(elements) = state.array_elements.get(&arr_id) {
        let tag = state.array_type_tags.get(&arr_id).copied().unwrap_or(b'L');
        pw.put_u8(tag); // arrayregion tag
        pw.put_u32_be(length);
        for i in first_index..first_index + length {
            if let Some(val) = elements.get(i as usize) {
                pw.put_bytes(val);
            } else {
                // Out of bounds — write zero
                match tag {
                    b'I' | b'F' => pw.put_u32_be(0),
                    b'J' | b'D' => pw.put_u64_be(0),
                    b'B' | b'Z' => pw.put_u8(0),
                    b'S' | b'C' => {
                        pw.put_u8(0);
                        pw.put_u8(0);
                    }
                    _ => pw.put_u64_be(0), // object ref
                }
            }
        }
    } else {
        pw.put_u8(b'L');
        pw.put_u32_be(0);
    }
    CommandResult::ok(pw.into_bytes())
}

// ===========================================================================
// ClassLoaderReference command set (14)
// ===========================================================================

fn handle_clr_visible_classes(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let _loader_id = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    // Return all loaded classes as visible from this loader
    let mut pw = PayloadWriter::new();
    pw.put_u32_be(state.loaded_classes.len() as u32);
    for (_, &(ref_type_id, type_tag)) in &state.loaded_classes {
        pw.put_u8(type_tag);
        pw.put_u64_be(ref_type_id);
    }
    CommandResult::ok(pw.into_bytes())
}

// ===========================================================================
// StackFrame extras (16)
// ===========================================================================

fn handle_sf_this_object(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let thread_id = reader.read_u64_be().unwrap_or(0);
    let frame_id = reader.read_u64_be().unwrap_or(0);

    // Try to get slot 0 (this) from the frame
    let frame_locals = state.get_frame_locals(ThreadId(thread_id), frame_id);
    let mut pw = PayloadWriter::new();
    if let Some(locals) = frame_locals {
        if let Some(local) = locals.first() {
            let obj_id = local.as_object_id().unwrap_or(0);
            if obj_id != 0 {
                pw.put_u8(b'L'); // tag: object
                pw.put_u64_be(obj_id);
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
    CommandResult::ok(pw.into_bytes())
}

// ===========================================================================
// ClassObjectReference command set (17)
// ===========================================================================

fn handle_cor_reflected_type(data: &[u8], state: &mut DebugState) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let class_obj_id = match reader.read_u64_be() {
        Ok(v) => v,
        Err(_) => return CommandResult::error(ERR_INTERNAL),
    };

    let mut pw = PayloadWriter::new();
    // Map class object ID back to reference type
    if let Some(&ref_type_id) = state.class_object_to_ref_type.get(&class_obj_id) {
        pw.put_u8(1); // refTypeTag: CLASS
        pw.put_u64_be(ref_type_id);
    } else {
        pw.put_u8(1);
        pw.put_u64_be(class_obj_id); // fallback: assume object ID = ref type ID
    }
    CommandResult::ok(pw.into_bytes())
}

// ---------------------------------------------------------------------------
// T6.5 — Tagged value encode/decode for `InvokeMethod` payloads
// ---------------------------------------------------------------------------

/// Read a JDWP TaggedValue (tag byte + value bytes) from the payload.
fn read_tagged_value(r: &mut PayloadReader<'_>) -> std::io::Result<DebuggerValue> {
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
fn write_tagged_value(pw: &mut PayloadWriter, v: &DebuggerValue) {
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
mod tests {
    use super::*;
    use crate::debug::DebugState;

    fn fresh_state() -> DebugState {
        DebugState::new()
    }

    #[test]
    fn dispatch_vm_version() {
        let mut st = fresh_state();
        let res = dispatch(CS_VM, CMD_VM_VERSION, &[], &mut st);
        assert_eq!(res.error_code, ERR_NONE);

        // The payload should contain "CratonVM" somewhere.
        let payload = String::from_utf8_lossy(&res.data);
        assert!(payload.contains("CratonVM"));
    }

    #[test]
    fn dispatch_vm_id_sizes() {
        let mut st = fresh_state();
        let res = dispatch(CS_VM, CMD_VM_ID_SIZES, &[], &mut st);
        assert_eq!(res.error_code, ERR_NONE);
        // 5 x u32 = 20 bytes, all set to 8
        assert_eq!(res.data.len(), 20);
        for chunk in res.data.chunks(4) {
            let val = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            assert_eq!(val, 8);
        }
    }

    #[test]
    fn dispatch_vm_capabilities() {
        let mut st = fresh_state();
        let res = dispatch(CS_VM, CMD_VM_CAPABILITIES, &[], &mut st);
        assert_eq!(res.error_code, ERR_NONE);
        assert_eq!(res.data.len(), 7);
        // canWatchFieldModification and canWatchFieldAccess are enabled (T6.4.4)
        assert_eq!(res.data[0], 1); // canWatchFieldModification
        assert_eq!(res.data[1], 1); // canWatchFieldAccess
                                    // Remaining capabilities are false
        assert!(res.data[2..].iter().all(|&b| b == 0));
    }

    #[test]
    fn dispatch_vm_capabilities_new() {
        let mut st = fresh_state();
        let res = dispatch(CS_VM, CMD_VM_CAPABILITIES_NEW, &[], &mut st);
        assert_eq!(res.error_code, ERR_NONE);
        assert_eq!(res.data.len(), 32);
    }

    #[test]
    fn dispatch_vm_suspend_resume() {
        let mut st = fresh_state();
        assert!(!st.suspended);
        dispatch(CS_VM, CMD_VM_SUSPEND, &[], &mut st);
        assert!(st.suspended);
        dispatch(CS_VM, CMD_VM_RESUME, &[], &mut st);
        assert!(!st.suspended);
    }

    #[test]
    fn dispatch_vm_dispose() {
        let mut st = fresh_state();
        dispatch(CS_VM, CMD_VM_DISPOSE, &[], &mut st);
        assert!(st.disposed);
    }

    #[test]
    fn dispatch_vm_all_threads_empty() {
        let mut st = fresh_state();
        let res = dispatch(CS_VM, CMD_VM_ALL_THREADS, &[], &mut st);
        assert_eq!(res.error_code, ERR_NONE);
        // count = 0  (4 bytes)
        assert_eq!(res.data, [0, 0, 0, 0]);
    }

    #[test]
    fn dispatch_vm_all_threads_with_threads() {
        let mut st = fresh_state();
        let t1 = st.ids.register_thread(100);
        st.thread_names.insert(t1.0, "main".into());
        let res = dispatch(CS_VM, CMD_VM_ALL_THREADS, &[], &mut st);
        assert_eq!(res.error_code, ERR_NONE);
        // count(4) + 1 thread_id(8) = 12
        assert_eq!(res.data.len(), 12);
    }

    #[test]
    fn dispatch_unknown_command() {
        let mut st = fresh_state();
        let res = dispatch(255, 255, &[], &mut st);
        assert_eq!(res.error_code, ERR_NOT_IMPLEMENTED);
    }

    #[test]
    fn dispatch_thread_name() {
        let mut st = fresh_state();
        let tid = st.ids.register_thread(1);
        st.thread_names.insert(tid.0, "worker-0".into());

        // Build payload: thread_id as u64 BE
        let data = tid.0.to_be_bytes();
        let res = dispatch(CS_THREAD_REF, CMD_TR_NAME, &data, &mut st);
        assert_eq!(res.error_code, ERR_NONE);
        let payload = String::from_utf8_lossy(&res.data);
        assert!(payload.contains("worker-0"));
    }

    #[test]
    fn dispatch_event_request_set_and_clear() {
        let mut st = fresh_state();

        // Set a breakpoint event request.
        let mut set_data = PayloadWriter::new();
        set_data.put_u8(EventKind::Breakpoint as u8); // eventKind
        set_data.put_u8(SuspendPolicy::All as u8); // suspendPolicy
        set_data.put_u32_be(0); // modifiers = 0
        let res = dispatch(
            CS_EVENT_REQUEST,
            CMD_ER_SET,
            &set_data.into_bytes(),
            &mut st,
        );
        assert_eq!(res.error_code, ERR_NONE);
        let req_id = u32::from_be_bytes([res.data[0], res.data[1], res.data[2], res.data[3]]);
        assert!(req_id > 0);

        // Clear it.
        let mut clear_data = PayloadWriter::new();
        clear_data.put_u8(EventKind::Breakpoint as u8);
        clear_data.put_u32_be(req_id);
        let res = dispatch(
            CS_EVENT_REQUEST,
            CMD_ER_CLEAR,
            &clear_data.into_bytes(),
            &mut st,
        );
        assert_eq!(res.error_code, ERR_NONE);
        assert_eq!(st.events.request_count(), 0);
    }

    #[test]
    fn dispatch_vm_create_string() {
        let mut st = fresh_state();
        let mut data = PayloadWriter::new();
        data.put_string("hello");
        let res = dispatch(CS_VM, CMD_VM_CREATE_STRING, &data.into_bytes(), &mut st);
        assert_eq!(res.error_code, ERR_NONE);
        assert_eq!(res.data.len(), 8); // one object ID
    }

    #[test]
    fn dispatch_top_level_thread_groups() {
        let mut st = fresh_state();
        let res = dispatch(CS_VM, CMD_VM_TOP_LEVEL_THREAD_GROUPS, &[], &mut st);
        assert_eq!(res.error_code, ERR_NONE);
        // count(4) + group_id(8) = 12
        assert_eq!(res.data.len(), 12);
    }

    // -----------------------------------------------------------------------
    // T6.4 wire-level smoke test: build real JDWP packet bytes, dispatch, and
    // assert the exact reply bytes, same as an IDE like IntelliJ would.
    // -----------------------------------------------------------------------

    use crate::debug::protocol::{read_packet, write_packet, JdwpPacket, REPLY_FLAG};
    use std::io::Cursor;

    fn wire_roundtrip(cmd_set: u8, cmd: u8, payload: &[u8], st: &mut DebugState) -> JdwpPacket {
        // --- Client side: build a command packet and serialise it. ------
        let client_pkt = JdwpPacket::Command {
            id: 0xCAFEBABE,
            flags: 0,
            command_set: cmd_set,
            command: cmd,
            data: payload.to_vec(),
        };
        let mut wire = Vec::new();
        write_packet(&mut wire, &client_pkt).unwrap();

        // --- Server side: parse from the wire, dispatch, produce reply. -
        let parsed = read_packet(&mut Cursor::new(&wire[..])).unwrap();
        let (id, cs, c, data) = match parsed {
            JdwpPacket::Command {
                id,
                command_set,
                command,
                data,
                ..
            } => (id, command_set, command, data),
            _ => panic!("expected command"),
        };
        let result = dispatch(cs, c, &data, st);
        let reply = if result.error_code == 0 {
            JdwpPacket::ok_reply(id, result.data)
        } else {
            JdwpPacket::error_reply(id, result.error_code)
        };
        let mut reply_wire = Vec::new();
        write_packet(&mut reply_wire, &reply).unwrap();

        // --- Client side again: parse the reply bytes. -----------------
        read_packet(&mut Cursor::new(&reply_wire[..])).unwrap()
    }

    #[test]
    fn t64_wire_vm_version() {
        let mut st = fresh_state();
        let reply = wire_roundtrip(CS_VM, CMD_VM_VERSION, &[], &mut st);
        match reply {
            JdwpPacket::Reply {
                id,
                error_code,
                data,
            } => {
                assert_eq!(id, 0xCAFEBABE);
                assert_eq!(error_code, 0);
                assert!(String::from_utf8_lossy(&data).contains("CratonVM"));
            }
            _ => panic!("expected reply"),
        }
    }

    #[test]
    fn t64_wire_vm_id_sizes() {
        let mut st = fresh_state();
        let reply = wire_roundtrip(CS_VM, CMD_VM_ID_SIZES, &[], &mut st);
        match reply {
            JdwpPacket::Reply {
                id,
                error_code,
                data,
            } => {
                assert_eq!(id, 0xCAFEBABE);
                assert_eq!(error_code, 0);
                // Five u32 BE fields, each = 8 (our ID size).
                assert_eq!(data.len(), 20);
                for chunk in data.chunks(4) {
                    let v = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                    assert_eq!(v, 8);
                }
            }
            _ => panic!("expected reply"),
        }
    }

    #[test]
    fn t64_wire_event_request_set_breakpoint_then_clear() {
        let mut st = fresh_state();

        // EventRequest/Set, BREAKPOINT, SuspendPolicy::All, 0 modifiers.
        let mut body = PayloadWriter::new();
        body.put_u8(EventKind::Breakpoint as u8);
        body.put_u8(SuspendPolicy::All as u8);
        body.put_u32_be(0);
        let reply = wire_roundtrip(CS_EVENT_REQUEST, CMD_ER_SET, &body.into_bytes(), &mut st);
        let req_id = match reply {
            JdwpPacket::Reply {
                error_code, data, ..
            } => {
                assert_eq!(error_code, 0);
                assert_eq!(data.len(), 4);
                u32::from_be_bytes([data[0], data[1], data[2], data[3]])
            }
            _ => panic!("expected reply"),
        };
        assert!(req_id > 0);
        assert_eq!(st.events.request_count(), 1);

        // EventRequest/Clear same request.
        let mut clr = PayloadWriter::new();
        clr.put_u8(EventKind::Breakpoint as u8);
        clr.put_u32_be(req_id);
        let reply = wire_roundtrip(CS_EVENT_REQUEST, CMD_ER_CLEAR, &clr.into_bytes(), &mut st);
        match reply {
            JdwpPacket::Reply {
                error_code, data, ..
            } => {
                assert_eq!(error_code, 0);
                assert!(data.is_empty());
            }
            _ => panic!("expected reply"),
        };
        assert_eq!(st.events.request_count(), 0);
    }

    #[test]
    fn t64_wire_event_request_set_step_over() {
        let mut st = fresh_state();

        // EventRequest/Set, SINGLE_STEP, with one Step modifier for step-over
        // (size=LINE, depth=OVER).
        let mut body = PayloadWriter::new();
        body.put_u8(EventKind::SingleStep as u8);
        body.put_u8(SuspendPolicy::EventThread as u8);
        body.put_u32_be(1); // one modifier
        body.put_u8(10); // mod kind = Step
        body.put_u64_be(1); // thread id
        body.put_u32_be(StepSize::Line as u32);
        body.put_u32_be(StepDepth::Over as u32);

        let reply = wire_roundtrip(CS_EVENT_REQUEST, CMD_ER_SET, &body.into_bytes(), &mut st);
        match reply {
            JdwpPacket::Reply {
                error_code, data, ..
            } => {
                assert_eq!(error_code, 0);
                assert_eq!(data.len(), 4);
                let req_id = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                assert!(req_id > 0);
            }
            _ => panic!("expected reply"),
        }
        assert_eq!(st.events.request_count(), 1);
    }

    #[test]
    fn t64_reply_flag_round_trips_over_wire() {
        // Double check that the reply we emit actually has REPLY_FLAG set on
        // byte 8 of the header — this is what clients parse.
        let mut st = fresh_state();
        let client_pkt = JdwpPacket::Command {
            id: 1,
            flags: 0,
            command_set: CS_VM,
            command: CMD_VM_VERSION,
            data: Vec::new(),
        };
        let mut wire = Vec::new();
        write_packet(&mut wire, &client_pkt).unwrap();
        let parsed = read_packet(&mut Cursor::new(&wire[..])).unwrap();
        let (id, cs, c, data) = match parsed {
            JdwpPacket::Command {
                id,
                command_set,
                command,
                data,
                ..
            } => (id, command_set, command, data),
            _ => panic!(),
        };
        let result = dispatch(cs, c, &data, &mut st);
        let reply = JdwpPacket::ok_reply(id, result.data);
        let mut reply_wire = Vec::new();
        write_packet(&mut reply_wire, &reply).unwrap();
        // Byte 8 of the header carries the flags (reply_flag).
        assert_eq!(reply_wire[8] & REPLY_FLAG, REPLY_FLAG);
    }

    // =======================================================================
    // T6.5 — Debugger-initiated method invocation
    // =======================================================================

    use crate::debug::{BridgeError, DebuggerValue, DebuggerVmBridge, InvokeOutcome};
    use std::sync::{Arc, Mutex};

    /// Scriptable mock bridge used to drive the wire-level tests for the
    /// three invocation commands without spinning up a full VM.
    #[derive(Default)]
    struct MockBridge {
        /// Queued outcomes for `invoke_static`.
        static_outcomes: Mutex<Vec<Result<InvokeOutcome, BridgeError>>>,
        /// Queued outcomes for `invoke_instance`.
        instance_outcomes: Mutex<Vec<Result<InvokeOutcome, BridgeError>>>,
        /// Queued outcomes for `new_array`.
        array_outcomes: Mutex<Vec<Result<u64, BridgeError>>>,
        /// Log of everything the handler actually passed us.
        calls: Mutex<Vec<MockCall>>,
    }

    #[derive(Debug, Clone)]
    enum MockCall {
        Static {
            class_id: u64,
            method_id: u64,
            thread_id: u64,
            args: Vec<DebuggerValue>,
            return_sig: String,
        },
        Instance {
            receiver_id: u64,
            class_id: u64,
            method_id: u64,
            thread_id: u64,
            args: Vec<DebuggerValue>,
            return_sig: String,
            non_virtual: bool,
        },
        NewArray {
            array_type_id: u64,
            length: i32,
        },
    }

    impl DebuggerVmBridge for MockBridge {
        fn invoke_static(
            &self,
            class_id: u64,
            method_id: u64,
            thread_id: u64,
            args: &[DebuggerValue],
            return_sig: &str,
        ) -> Result<InvokeOutcome, BridgeError> {
            self.calls.lock().unwrap().push(MockCall::Static {
                class_id,
                method_id,
                thread_id,
                args: args.to_vec(),
                return_sig: return_sig.to_string(),
            });
            self.static_outcomes
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(Err(BridgeError::Internal))
        }

        fn invoke_instance(
            &self,
            receiver_id: u64,
            class_id: u64,
            method_id: u64,
            thread_id: u64,
            args: &[DebuggerValue],
            return_sig: &str,
            non_virtual: bool,
        ) -> Result<InvokeOutcome, BridgeError> {
            self.calls.lock().unwrap().push(MockCall::Instance {
                receiver_id,
                class_id,
                method_id,
                thread_id,
                args: args.to_vec(),
                return_sig: return_sig.to_string(),
                non_virtual,
            });
            self.instance_outcomes
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(Err(BridgeError::Internal))
        }

        fn new_array(&self, array_type_id: u64, length: i32) -> Result<u64, BridgeError> {
            self.calls.lock().unwrap().push(MockCall::NewArray {
                array_type_id,
                length,
            });
            self.array_outcomes
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(Err(BridgeError::Internal))
        }
    }

    /// Register a method with a known return signature on the state and
    /// install a fresh mock bridge.
    fn state_with_bridge(
        class_id: u64,
        method_id: u64,
        descriptor: &str,
    ) -> (DebugState, Arc<MockBridge>) {
        let mut st = DebugState::new();
        // Register a minimal method so `return_signature_for` can resolve
        // the descriptor and hand the bridge the right return signature.
        st.class_methods.insert(
            class_id,
            vec![MethodInfo {
                method_id: crate::debug::ids::MethodId(method_id),
                name: "mock".to_string(),
                signature: descriptor.to_string(),
                mod_bits: 0,
            }],
        );
        let bridge: Arc<MockBridge> = Arc::new(MockBridge::default());
        st.vm_bridge = Some(bridge.clone() as Arc<dyn DebuggerVmBridge>);
        (st, bridge)
    }

    fn build_ct_invoke_payload(
        class_id: u64,
        thread_id: u64,
        method_id: u64,
        args: &[DebuggerValue],
        invoke_options: u32,
    ) -> Vec<u8> {
        let mut pw = PayloadWriter::new();
        pw.put_u64_be(class_id);
        pw.put_u64_be(thread_id);
        pw.put_u64_be(method_id);
        pw.put_u32_be(args.len() as u32);
        for a in args {
            write_tagged_value(&mut pw, a);
        }
        pw.put_u32_be(invoke_options);
        pw.into_bytes()
    }

    fn build_or_invoke_payload(
        object_id: u64,
        thread_id: u64,
        class_id: u64,
        method_id: u64,
        args: &[DebuggerValue],
        invoke_options: u32,
    ) -> Vec<u8> {
        let mut pw = PayloadWriter::new();
        pw.put_u64_be(object_id);
        pw.put_u64_be(thread_id);
        pw.put_u64_be(class_id);
        pw.put_u64_be(method_id);
        pw.put_u32_be(args.len() as u32);
        for a in args {
            write_tagged_value(&mut pw, a);
        }
        pw.put_u32_be(invoke_options);
        pw.into_bytes()
    }

    // --- ClassType.InvokeMethod --------------------------------------------

    #[test]
    fn t65_ct_invoke_method_happy_path_int_return() {
        let (mut st, bridge) = state_with_bridge(10, 42, "(II)I");
        bridge
            .static_outcomes
            .lock()
            .unwrap()
            .push(Ok(InvokeOutcome::returned(DebuggerValue::Int(7))));

        let payload = build_ct_invoke_payload(
            10,
            1,
            42,
            &[DebuggerValue::Int(3), DebuggerValue::Int(4)],
            0,
        );
        let res = dispatch(CS_CLASS_TYPE, CMD_CT_INVOKE_METHOD, &payload, &mut st);
        assert_eq!(res.error_code, ERR_NONE);

        // Reply: tag(1)=I + int(4) + excTag(1)=L + excId(8) = 14 bytes.
        assert_eq!(res.data.len(), 14);
        assert_eq!(res.data[0], b'I');
        let ret = i32::from_be_bytes([res.data[1], res.data[2], res.data[3], res.data[4]]);
        assert_eq!(ret, 7);
        // Exception: tag L, id 0.
        assert_eq!(res.data[5], b'L');
        assert_eq!(
            u64::from_be_bytes([
                res.data[6],
                res.data[7],
                res.data[8],
                res.data[9],
                res.data[10],
                res.data[11],
                res.data[12],
                res.data[13]
            ]),
            0
        );

        // Verify handler forwarded the right things to the bridge.
        let calls = bridge.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::Static {
                class_id,
                method_id,
                thread_id,
                args,
                return_sig,
            } => {
                assert_eq!(*class_id, 10);
                assert_eq!(*method_id, 42);
                assert_eq!(*thread_id, 1);
                assert_eq!(args, &vec![DebuggerValue::Int(3), DebuggerValue::Int(4)]);
                assert_eq!(return_sig, "I");
            }
            c => panic!("expected Static call, got {:?}", c),
        }
    }

    #[test]
    fn t65_ct_invoke_method_exception_path() {
        let (mut st, bridge) = state_with_bridge(10, 42, "()V");
        // Simulate the method throwing — exception ID 99.
        bridge
            .static_outcomes
            .lock()
            .unwrap()
            .push(Ok(InvokeOutcome::threw(99)));

        let payload = build_ct_invoke_payload(10, 1, 42, &[], 0);
        let res = dispatch(CS_CLASS_TYPE, CMD_CT_INVOKE_METHOD, &payload, &mut st);
        assert_eq!(res.error_code, ERR_NONE);

        // Reply: returnValue = null object (tag L + 8-byte zero), per JDWP
        // convention when the method throws.  Then excTag L + excId 8.
        // Total: 1 + 8 + 1 + 8 = 18 bytes.
        assert_eq!(res.data.len(), 18);
        assert_eq!(res.data[0], b'L');
        let ret_id = u64::from_be_bytes([
            res.data[1],
            res.data[2],
            res.data[3],
            res.data[4],
            res.data[5],
            res.data[6],
            res.data[7],
            res.data[8],
        ]);
        assert_eq!(ret_id, 0); // null return
        assert_eq!(res.data[9], b'L');
        let exc = u64::from_be_bytes([
            res.data[10],
            res.data[11],
            res.data[12],
            res.data[13],
            res.data[14],
            res.data[15],
            res.data[16],
            res.data[17],
        ]);
        assert_eq!(exc, 99);
    }

    #[test]
    fn t65_ct_invoke_method_invalid_thread() {
        let (mut st, bridge) = state_with_bridge(10, 42, "()V");
        bridge
            .static_outcomes
            .lock()
            .unwrap()
            .push(Err(BridgeError::InvalidThread));

        let payload = build_ct_invoke_payload(10, 0xDEAD, 42, &[], 0);
        let res = dispatch(CS_CLASS_TYPE, CMD_CT_INVOKE_METHOD, &payload, &mut st);
        assert_eq!(res.error_code, ERR_INVALID_THREAD);
        assert!(res.data.is_empty());
    }

    // --- ObjectReference.InvokeMethod --------------------------------------

    #[test]
    fn t65_or_invoke_method_happy_path_object_return() {
        let (mut st, bridge) = state_with_bridge(5, 17, "()Ljava/lang/String;");
        bridge
            .instance_outcomes
            .lock()
            .unwrap()
            .push(Ok(InvokeOutcome::returned(DebuggerValue::Object(0xBEEF))));

        let payload = build_or_invoke_payload(100, 1, 5, 17, &[], 0);
        let res = dispatch(CS_OBJECT_REF, CMD_OR_INVOKE_METHOD, &payload, &mut st);
        assert_eq!(res.error_code, ERR_NONE);

        // Reply: tag(1)=L + objId(8) + excTag(1)=L + excId(8) = 18 bytes.
        assert_eq!(res.data.len(), 18);
        assert_eq!(res.data[0], b'L');
        let obj_id = u64::from_be_bytes([
            res.data[1],
            res.data[2],
            res.data[3],
            res.data[4],
            res.data[5],
            res.data[6],
            res.data[7],
            res.data[8],
        ]);
        assert_eq!(obj_id, 0xBEEF);
        assert_eq!(res.data[9], b'L');

        let calls = bridge.calls.lock().unwrap();
        match &calls[0] {
            MockCall::Instance {
                receiver_id,
                class_id,
                method_id,
                thread_id,
                args,
                return_sig,
                non_virtual,
            } => {
                assert_eq!(*receiver_id, 100);
                assert_eq!(*class_id, 5);
                assert_eq!(*method_id, 17);
                assert_eq!(*thread_id, 1);
                assert!(args.is_empty());
                assert_eq!(return_sig, "Ljava/lang/String;");
                assert!(!non_virtual);
            }
            c => panic!("expected Instance call, got {:?}", c),
        }
    }

    #[test]
    fn t65_or_invoke_method_non_virtual_dispatch() {
        let (mut st, bridge) = state_with_bridge(5, 17, "()V");
        bridge
            .instance_outcomes
            .lock()
            .unwrap()
            .push(Ok(InvokeOutcome::returned(DebuggerValue::Void)));

        // invokeOptions = 2 => non-virtual
        let payload = build_or_invoke_payload(100, 1, 5, 17, &[], 2);
        let res = dispatch(CS_OBJECT_REF, CMD_OR_INVOKE_METHOD, &payload, &mut st);
        assert_eq!(res.error_code, ERR_NONE);

        let calls = bridge.calls.lock().unwrap();
        match &calls[0] {
            MockCall::Instance { non_virtual, .. } => assert!(*non_virtual),
            c => panic!("expected Instance call, got {:?}", c),
        }
    }

    #[test]
    fn t65_or_invoke_method_exception_path() {
        let (mut st, bridge) = state_with_bridge(5, 17, "()I");
        bridge
            .instance_outcomes
            .lock()
            .unwrap()
            .push(Ok(InvokeOutcome::threw(0xCAFE)));

        let payload = build_or_invoke_payload(100, 1, 5, 17, &[], 0);
        let res = dispatch(CS_OBJECT_REF, CMD_OR_INVOKE_METHOD, &payload, &mut st);
        assert_eq!(res.error_code, ERR_NONE);

        // returnValue = null object (tag L + id 0) since method threw.
        // Then excTag L + excId 8.
        assert_eq!(res.data.len(), 18);
        assert_eq!(res.data[0], b'L');
        let ret_id = u64::from_be_bytes([
            res.data[1],
            res.data[2],
            res.data[3],
            res.data[4],
            res.data[5],
            res.data[6],
            res.data[7],
            res.data[8],
        ]);
        assert_eq!(ret_id, 0); // null return
        assert_eq!(res.data[9], b'L');
        let exc = u64::from_be_bytes([
            res.data[10],
            res.data[11],
            res.data[12],
            res.data[13],
            res.data[14],
            res.data[15],
            res.data[16],
            res.data[17],
        ]);
        assert_eq!(exc, 0xCAFE);
    }

    #[test]
    fn t65_or_invoke_method_invalid_thread() {
        let (mut st, bridge) = state_with_bridge(5, 17, "()V");
        bridge
            .instance_outcomes
            .lock()
            .unwrap()
            .push(Err(BridgeError::InvalidThread));

        let payload = build_or_invoke_payload(100, 0xDEAD, 5, 17, &[], 0);
        let res = dispatch(CS_OBJECT_REF, CMD_OR_INVOKE_METHOD, &payload, &mut st);
        assert_eq!(res.error_code, ERR_INVALID_THREAD);
        assert!(res.data.is_empty());
    }

    // --- ArrayType.NewInstance ---------------------------------------------

    #[test]
    fn t65_at_new_instance_happy_path() {
        let (mut st, bridge) = state_with_bridge(0, 0, "()V");
        bridge.array_outcomes.lock().unwrap().push(Ok(0x4242));

        let mut pw = PayloadWriter::new();
        pw.put_u64_be(77); // arrayTypeID
        pw.put_u32_be(16); // length
        let res = dispatch(
            CS_ARRAY_TYPE,
            CMD_AT_NEW_INSTANCE,
            &pw.into_bytes(),
            &mut st,
        );
        assert_eq!(res.error_code, ERR_NONE);

        // Reply: tag(1)=[ + id(8) = 9 bytes.
        assert_eq!(res.data.len(), 9);
        assert_eq!(res.data[0], b'[');
        let id = u64::from_be_bytes([
            res.data[1],
            res.data[2],
            res.data[3],
            res.data[4],
            res.data[5],
            res.data[6],
            res.data[7],
            res.data[8],
        ]);
        assert_eq!(id, 0x4242);

        let calls = bridge.calls.lock().unwrap();
        match &calls[0] {
            MockCall::NewArray {
                array_type_id,
                length,
            } => {
                assert_eq!(*array_type_id, 77);
                assert_eq!(*length, 16);
            }
            c => panic!("expected NewArray call, got {:?}", c),
        }
    }

    #[test]
    fn t65_at_new_instance_invalid_class() {
        let (mut st, bridge) = state_with_bridge(0, 0, "()V");
        bridge
            .array_outcomes
            .lock()
            .unwrap()
            .push(Err(BridgeError::InvalidClass));

        let mut pw = PayloadWriter::new();
        pw.put_u64_be(0xDEAD);
        pw.put_u32_be(10);
        let res = dispatch(
            CS_ARRAY_TYPE,
            CMD_AT_NEW_INSTANCE,
            &pw.into_bytes(),
            &mut st,
        );
        assert_eq!(res.error_code, ERR_INVALID_CLASS);
        assert!(res.data.is_empty());
    }

    #[test]
    fn t65_at_new_instance_no_bridge_vm_dead() {
        let mut st = DebugState::new(); // no bridge installed
        let mut pw = PayloadWriter::new();
        pw.put_u64_be(77);
        pw.put_u32_be(16);
        let res = dispatch(
            CS_ARRAY_TYPE,
            CMD_AT_NEW_INSTANCE,
            &pw.into_bytes(),
            &mut st,
        );
        assert_eq!(res.error_code, ERR_VM_DEAD);
    }

    // Wire-level round-trip for the three new handlers: serialises the full
    // JDWP command, dispatches, and parses the reply bytes back through the
    // decoder, confirming the full header/flags/length math is consistent.

    #[test]
    fn t65_wire_ct_invoke_method_roundtrip() {
        let (mut st, bridge) = state_with_bridge(10, 42, "(I)I");
        bridge
            .static_outcomes
            .lock()
            .unwrap()
            .push(Ok(InvokeOutcome::returned(DebuggerValue::Int(99))));

        let payload = build_ct_invoke_payload(10, 1, 42, &[DebuggerValue::Int(1)], 0);
        let reply = wire_roundtrip(CS_CLASS_TYPE, CMD_CT_INVOKE_METHOD, &payload, &mut st);
        match reply {
            JdwpPacket::Reply {
                error_code, data, ..
            } => {
                assert_eq!(error_code, 0);
                // Reply should decode tag=I, i32=99, excTag=L, excId=0.
                assert_eq!(data[0], b'I');
                let ret = i32::from_be_bytes([data[1], data[2], data[3], data[4]]);
                assert_eq!(ret, 99);
                assert_eq!(data[5], b'L');
            }
            _ => panic!("expected reply"),
        }
    }
}
