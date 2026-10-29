// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The JDWP commands that read or write the heap (interpreter round i1
//! wave 8): `ObjectReference.ReferenceType` / `GetValues` / `SetValues` /
//! `DisableCollection` / `EnableCollection` / `IsCollected`,
//! `ReferenceType.GetValues`, `ClassType.SetValues`, `StringReference.Value`,
//! `ArrayReference.Length` / `GetValues` / `SetValues`,
//! `ClassObjectReference.ReflectedType` and `VirtualMachine.CreateString`;
//! and (wave 9) `StackFrame.SetValues`, which writes a suspended frame's
//! locals on the frame's own parked thread.
//!
//! Until wave 8 their handlers (`commands.rs`) saw only the debug state and
//! answered from side tables nothing filled: class 0 for every object, `null`
//! for every field, empty arrays, `""` for every string, and success for
//! writes that wrote nothing. They now read the objects.
//!
//! **Where they run.** On a thread parked at an interpreter suspend point
//! ([`super::run_on_parked_thread`]) when one exists: a registered mutator, so
//! no collection runs while it reads, its barriers are a mutator's, and it
//! can allocate (`CreateString`). With no thread parked (the VM is running,
//! or its suspended threads are blocked in native or compiled code), they run
//! on the session's heap service (wave 10, `super::start_heap_service`), a
//! daemon thread attached to the VM for exactly this. Without one (a VM whose
//! server could not start it), the reads and writes run on the JDWP server
//! thread inside `GcBarrier::run_if_no_stw_requested`, which keeps a
//! stop-the-world pause from starting while they run, and `CreateString`
//! answers `THREAD_NOT_SUSPENDED`. Lock order: class manager, (GC barrier),
//! debug state, JNI global references.
//!
//! **Ids.** A field id is [`super::jdwp_field_id`] (declaring class and
//! index); an object id goes through the session's object table
//! ([`super::object_for_id`], [`super::export_object`]).
//!
//! A value the debugger stores into a reference field or array element is
//! not type-checked against the declared class (JDI checks before sending;
//! HotSpot's back end stores fields through JNI, which does not check);
//! JDWP's `TYPE_MISMATCH` is answered only for a non-array stored where the
//! descriptor requires an array. A frame LOCAL is checked against its
//! `LocalVariableTable` signature (wave 84, [`object_fits_local_signature`]),
//! as HotSpot's back end writes it through JVMTI `SetLocalObject`.

use super::commands::{
    CommandResult, CMD_AR_GET_VALUES, CMD_AR_LENGTH, CMD_AR_SET_VALUES, CMD_CLR_VISIBLE_CLASSES,
    CMD_COR_REFLECTED_TYPE, CMD_CT_SET_VALUES, CMD_OR_DISABLE_COLLECTION, CMD_OR_ENABLE_COLLECTION,
    CMD_OR_GET_VALUES, CMD_OR_IS_COLLECTED, CMD_OR_REFERENCE_TYPE, CMD_OR_SET_VALUES,
    CMD_RT_CLASS_LOADER, CMD_RT_CLASS_OBJECT, CMD_RT_GET_VALUES, CMD_SF_GET_VALUES,
    CMD_SF_SET_VALUES, CMD_SF_THIS_OBJECT, CMD_SR_VALUE, CMD_TGR_CHILDREN, CMD_TGR_NAME,
    CMD_TGR_PARENT, CMD_TR_INTERRUPT, CMD_TR_STOP, CMD_TR_THREAD_GROUP, CMD_VM_CREATE_STRING,
    CMD_VM_TOP_LEVEL_THREAD_GROUPS, CS_ARRAY_REF, CS_CLASSLOADER_REF, CS_CLASS_OBJ_REF,
    CS_CLASS_TYPE, CS_OBJECT_REF, CS_REF_TYPE, CS_STACK_FRAME, CS_STRING_REF, CS_THREAD_GROUP_REF,
    CS_THREAD_REF, CS_VM, ERR_INTERNAL, ERR_INVALID_ARRAY, ERR_INVALID_CLASS, ERR_INVALID_FIELDID,
    ERR_INVALID_FRAMEID, ERR_INVALID_INDEX, ERR_INVALID_LENGTH, ERR_INVALID_OBJECT,
    ERR_INVALID_SLOT, ERR_INVALID_STRING, ERR_INVALID_THREAD, ERR_OPAQUE_FRAME,
    ERR_THREAD_NOT_SUSPENDED, ERR_TYPE_MISMATCH,
};
use super::ids::ObjectExport;
use super::protocol::{PayloadReader, PayloadWriter};
use super::{BridgeError, DebugState, DebuggerValue};
use crate::classloading::{ClassId, ClassManager};
use crate::memory::heap::{ArrayElementType, ObjectKind};
use crate::threading::jvm_thread::JvmThread;
use crate::types::{ObjectRef, Value};
use crate::vm::SharedVm;
use cratonvm_native_api::{
    NativeHeapAccess as _, NativeInvokeAccess as _, NativeThreadAccess as _,
};

/// The deepest superclass chain the walks below follow (a guard against a
/// corrupt chain, far above any real one).
const MAX_CHAIN: usize = 1024;

/// Is `(command_set, command)` one of the heap commands this module serves?
/// The JDWP server routes them to [`run_heap_command`];
/// `commands::dispatch`, which has no VM, refuses them.
pub(crate) fn is_heap_command(command_set: u8, command: u8) -> bool {
    matches!(
        (command_set, command),
        (CS_VM, CMD_VM_CREATE_STRING)
            | (
                CS_REF_TYPE,
                CMD_RT_GET_VALUES | CMD_RT_CLASS_LOADER | CMD_RT_CLASS_OBJECT
            )
            | (CS_CLASS_TYPE, CMD_CT_SET_VALUES)
            | (
                CS_OBJECT_REF,
                CMD_OR_REFERENCE_TYPE
                    | CMD_OR_GET_VALUES
                    | CMD_OR_SET_VALUES
                    | CMD_OR_DISABLE_COLLECTION
                    | CMD_OR_ENABLE_COLLECTION
                    | CMD_OR_IS_COLLECTED
            )
            | (CS_STRING_REF, CMD_SR_VALUE)
            | (
                CS_ARRAY_REF,
                CMD_AR_LENGTH | CMD_AR_GET_VALUES | CMD_AR_SET_VALUES
            )
            | (CS_CLASS_OBJ_REF, CMD_COR_REFLECTED_TYPE)
            // Interpreter round i1 wave 24.
            | (CS_THREAD_REF, CMD_TR_INTERRUPT | CMD_TR_STOP)
            | (CS_CLASSLOADER_REF, CMD_CLR_VISIBLE_CLASSES)
            // Interpreter round i5 wave 103 (lane L1): `canGetMonitorInfo`.
            | (CS_OBJECT_REF, super::commands::CMD_OR_MONITOR_INFO)
    ) || is_monitor_command(command_set, command)
        || is_module_command(command_set, command)
        || is_heap_walk_command(command_set, command)
}

/// The commands that walk the heap (interpreter round i5 wave 103, lane L1;
/// `canGetInstanceInfo`): `VirtualMachine.InstanceCounts`,
/// `ReferenceType.Instances` and `ObjectReference.ReferringObjects`. Served
/// on a registered mutator, which requests the walk's non-moving pause
/// ([`heap_walk_command`]): a parked thread, else the session's heap service
/// ([`super::run_on_parked_thread`] with no thread named), so they answer
/// whatever the threads do, as HotSpot's (interpreter round i5 wave 104, lane
/// L1: the wave-103 note that they needed a parked thread was wrong; only a
/// session whose heap service could not start answers
/// `THREAD_NOT_SUSPENDED`, from [`run_quiesced`]).
pub(crate) fn is_heap_walk_command(command_set: u8, command: u8) -> bool {
    use super::commands::{CMD_OR_REFERRING_OBJECTS, CMD_RT_INSTANCES, CMD_VM_INSTANCE_COUNTS};
    matches!(
        (command_set, command),
        (CS_VM, CMD_VM_INSTANCE_COUNTS)
            | (CS_REF_TYPE, CMD_RT_INSTANCES)
            | (CS_OBJECT_REF, CMD_OR_REFERRING_OBJECTS)
    )
}

/// The module commands (JDWP 9; interpreter round i1 wave 25):
/// `VirtualMachine.AllModules`, `ReferenceType.Module` and
/// `ModuleReference.Name` / `ClassLoader`, answered on a mutator
/// ([`module_command`]). JDI sends them only to a JDWP 9+ target, which the
/// server answers since the same wave (`debug::vm_version`).
pub(crate) fn is_module_command(command_set: u8, command: u8) -> bool {
    use super::commands::{
        CMD_MR_CLASS_LOADER, CMD_MR_NAME, CMD_RT_MODULE, CMD_VM_ALL_MODULES, CS_MODULE_REF,
    };
    matches!(
        (command_set, command),
        (CS_VM, CMD_VM_ALL_MODULES)
            | (CS_REF_TYPE, CMD_RT_MODULE)
            | (CS_MODULE_REF, CMD_MR_NAME | CMD_MR_CLASS_LOADER)
    )
}

/// The owned- and contended-monitor commands (interpreter round i1 wave 25):
/// `ThreadReference.OwnedMonitors`, `CurrentContendedMonitor` and
/// `OwnedMonitorsStackDepthInfo`. Heap commands (they export the monitors'
/// object ids), served first on the thread they name when it is parked
/// ([`monitor_command_on_its_thread`]).
pub(crate) fn is_monitor_command(command_set: u8, command: u8) -> bool {
    use super::commands::{
        CMD_TR_CURRENT_CONTENDED_MONITOR, CMD_TR_OWNED_MONITORS,
        CMD_TR_OWNED_MONITORS_STACK_DEPTH_INFO,
    };
    command_set == CS_THREAD_REF
        && matches!(
            command,
            CMD_TR_OWNED_MONITORS
                | CMD_TR_CURRENT_CONTENDED_MONITOR
                | CMD_TR_OWNED_MONITORS_STACK_DEPTH_INFO
        )
}

/// The commands the JDWP server serves through [`run_heap_command`]: the heap
/// commands, `StackFrame.GetValues` / `ThisObject`, which read the frame
/// snapshot (`commands::dispatch` serves them without a VM) but tag each
/// object by its class ([`object_tag`]) when the heap can be read, and
/// (wave 9) `StackFrame.SetValues`, which writes the frame on its thread;
/// and (wave 24) the thread-group commands, which read the program's
/// `ThreadGroup` objects on a mutator ([`thread_group_command`]) and fall
/// back to `commands::dispatch`'s one `system` group without one.
pub(crate) fn served_with_the_heap(command_set: u8, command: u8) -> bool {
    is_heap_command(command_set, command)
        || matches!(
            (command_set, command),
            (
                CS_STACK_FRAME,
                CMD_SF_GET_VALUES | CMD_SF_THIS_OBJECT | CMD_SF_SET_VALUES
            )
        )
        || is_thread_group_command(command_set, command)
}

/// The commands that name or list thread groups (interpreter round i1 wave
/// 24): `VirtualMachine.TopLevelThreadGroups`, `ThreadReference.ThreadGroup`
/// and `ThreadGroupReference.Name` / `Parent` / `Children`.
pub(crate) fn is_thread_group_command(command_set: u8, command: u8) -> bool {
    matches!(
        (command_set, command),
        (CS_VM, CMD_VM_TOP_LEVEL_THREAD_GROUPS)
            | (CS_THREAD_REF, CMD_TR_THREAD_GROUP)
            | (
                CS_THREAD_GROUP_REF,
                CMD_TGR_NAME | CMD_TGR_PARENT | CMD_TGR_CHILDREN
            )
    )
}

/// Serve a heap command from the JDWP server thread (see the module doc for
/// where it runs). Called with no debug-state lock held.
pub(crate) fn run_heap_command(
    shared: &SharedVm,
    command_set: u8,
    command: u8,
    data: &[u8],
) -> CommandResult {
    if (command_set, command) == (CS_STACK_FRAME, CMD_SF_SET_VALUES) {
        return sf_set_values(shared, data);
    }
    if is_monitor_command(command_set, command) {
        if let Some(answer) = monitor_command_on_its_thread(shared, command, data) {
            return answer;
        }
    }
    let owned = data.to_vec();
    let on_parked = super::run_on_parked_thread(shared, None, move |shared, thread| {
        run_on_mutator(shared, thread, command_set, command, &owned)
    });
    match on_parked {
        Ok(result) => result,
        // `Busy` needs a thread named; the work above names none.
        Err(super::ParkedError::NotParked | super::ParkedError::Busy) => {
            if (command_set, command) == (CS_VM, CMD_VM_CREATE_STRING) {
                CommandResult::error(ERR_THREAD_NOT_SUSPENDED)
            } else {
                run_quiesced(shared, command_set, command, data)
            }
        }
        Err(super::ParkedError::Lost) => CommandResult::error(ERR_INTERNAL),
    }
}

/// `StackFrame.SetValues` (16/2, wave 9): `thread`, `frame`, then `count` ×
/// (`slot`, tagged value). Runs on the frame's own thread, parked at its
/// suspend point ([`set_frame_values`]); `THREAD_NOT_SUSPENDED` when that
/// thread is not parked there (running, or blocked in native or compiled
/// code), `INVALID_THREAD` for an id naming no thread. Until wave 8 it
/// answered success and wrote nothing; wave 8 made it `NOT_IMPLEMENTED`.
/// Since wave 41 a thread suspended while blocked in a native, whose frames
/// its inspection window published, takes the write too, deferred to its
/// wake ([`defer_blocked_frame_write`]).
fn sf_set_values(shared: &SharedVm, data: &[u8]) -> CommandResult {
    let Ok(thread_id) = PayloadReader::new(data).read_u64_be() else {
        return CommandResult::error(ERR_INTERNAL);
    };
    let owned = data.to_vec();
    let written = super::run_on_parked_thread(shared, Some(thread_id), move |shared, thread| {
        set_frame_values(shared, thread, &owned)
    });
    match written {
        Ok(Ok(())) => CommandResult::ok(Vec::new()),
        Ok(Err(code)) => CommandResult::error(code),
        Err(super::ParkedError::NotParked) if defer_candidate(shared, thread_id) => {
            // Wave 41: suspended while blocked in a native, its frames
            // published through its inspection window — the write waits for
            // the thread ([`defer_blocked_frame_write`]).
            let owned = data.to_vec();
            let deferred = super::run_on_parked_thread(shared, None, move |shared, _| {
                defer_blocked_frame_write(shared, &owned)
            });
            match deferred {
                Ok(Some(Ok(()))) => CommandResult::ok(Vec::new()),
                Ok(Some(Err(code))) => CommandResult::error(code),
                Ok(None) | Err(_) => {
                    let ds = shared.debug.debug_state.lock();
                    CommandResult::error(match super::unparked_thread_refusal(&ds, thread_id) {
                        BridgeError::InvalidThread => ERR_INVALID_THREAD,
                        _ => ERR_THREAD_NOT_SUSPENDED,
                    })
                }
            }
        }
        Err(super::ParkedError::NotParked | super::ParkedError::Busy) => {
            let ds = shared.debug.debug_state.lock();
            CommandResult::error(match super::unparked_thread_refusal(&ds, thread_id) {
                BridgeError::InvalidThread => ERR_INVALID_THREAD,
                _ => ERR_THREAD_NOT_SUSPENDED,
            })
        }
        Err(super::ParkedError::Lost) => CommandResult::error(ERR_INTERNAL),
    }
}

// ---------------------------------------------------------------------------
// `StackFrame.SetValues` on a thread blocked in a native (interpreter round i1
// wave 41, lane L1)
// ---------------------------------------------------------------------------
//
// HotSpot writes the local of any interpreted frame of a suspended thread
// (JVMTI `SetLocal*`), whether the thread is at a bytecode or inside a native
// such as `Object.wait0`; this server wrote only on a thread parked at an
// interpreter suspend point and answered `THREAD_NOT_SUSPENDED` for the rest
// (item 3 of
// docs/feature-designs/jvmti-delivery-threading.md).
// A blocked thread's frames are not the server's to write: a collection it
// sleeps through folds its moves into the thread's blocked deposit, not the
// frames, and the thread's wake applies them (`check_post_block_gc_refs`), so
// a reference stored into a frame meanwhile would be neither a root nor safe
// from that remap. So the write is deferred: checked and recorded against the
// snapshot the thread's inspection window published (which answers
// `GetValues` with the new value at once), and applied by the thread itself
// at the end of its wake, when its frames are its own again
// ([`apply_deferred_frame_writes_if_any`]). An object value is held by its id,
// collection disabled, until then.

/// Can a `SetValues` refused on the frame's own thread (`NotParked`) be
/// deferred: is thread `tid` blocked, with a snapshot of its frames standing
/// in its inspection window? The lock-free pre-check; the record re-checks
/// under the debug-state lock.
fn defer_candidate(shared: &SharedVm, tid: u64) -> bool {
    crate::runtime::interpreter::blocked_snapshot_published(shared, tid)
}

/// The kind ([`local_kind_of_signature`]) of a value a published snapshot
/// holds for a local.
fn local_kind_of_snapshot(v: &super::LocalValue) -> u8 {
    match v {
        super::LocalValue::Int(_) => b'I',
        super::LocalValue::Long(_) => b'J',
        super::LocalValue::Float(_) => b'F',
        super::LocalValue::Double(_) => b'D',
        super::LocalValue::ObjectRef(_) => b'L',
    }
}

/// Is `entry` (a row of thread `tid`'s published listing) an interpreter
/// frame: neither the native method heading a blocked thread's listing nor a
/// compiled activation it stands under? The same test [`set_frame_values`]
/// counts frames with.
fn is_interpreter_row(ds: &DebugState, tid: u64, entry: &super::FrameEntry) -> bool {
    entry.offset != super::NATIVE_FRAME_LOCATION
        && !ds
            .opaque_frames
            .get(&(tid, entry.frame_id))
            .copied()
            .unwrap_or(false)
        // Wave 138 (L1): nor a lambda proxy method's row (`DebugState::proxy_frames`).
        && !ds.proxy_frames.contains(&(tid, entry.frame_id))
}

/// Record a `StackFrame.SetValues` (`data`: `thread`, `frame`, `count` ×
/// (`slot`, tagged value)) for a thread suspended while blocked in a native
/// (wave 41). Runs on a registered mutator between two polls (the heap
/// service, or any parked thread: `debug::run_on_parked_thread`), so no
/// collection moves an object while an id is resolved and exported.
///
/// `None` when the write cannot be deferred — the thread is not suspended,
/// no snapshot of this suspension stands, or its inspection window no longer
/// holds one (it is leaving the native) — and the caller refuses it as
/// before. Otherwise the JDWP answer: every write is checked before any is
/// recorded, as in [`set_frame_values`] (`INVALID_FRAMEID`, `OPAQUE_FRAME`
/// for the native method's frame and a compiled one, `INVALID_SLOT`,
/// `TYPE_MISMATCH`, `INVALID_OBJECT`), the kind of a slot with no declared
/// variable coming from the snapshot. The snapshot is refreshed with the new
/// values.
///
/// The record cannot miss the thread's leave: the thread closes its window
/// before it withdraws the snapshot under the debug-state lock
/// (`interpreter::close_blocked_inspection`), and this checks the window and
/// records under that lock, so either the window was already closed (`None`)
/// or the withdrawal, and the wake's apply after it, come after the record.
fn defer_blocked_frame_write(shared: &SharedVm, data: &[u8]) -> Option<Result<(), u16>> {
    let mut r = PayloadReader::new(data);
    let (tid, frame_id, count) = match read_set_values_head(&mut r) {
        Ok(h) => h,
        Err(code) => return Some(Err(code)),
    };
    let mut ds = shared.debug.debug_state.lock();
    if !ds.is_thread_suspended(tid)
        || !ds.has_current_frames(tid)
        || !crate::runtime::interpreter::blocked_snapshot_published(shared, tid)
    {
        return None;
    }
    let listing = ds.thread_frames.get(&tid)?;
    let Some(entry) = listing.iter().find(|e| e.frame_id == frame_id).cloned() else {
        return Some(Err(ERR_INVALID_FRAMEID));
    };
    if entry.offset == super::NATIVE_FRAME_LOCATION
        || ds.opaque_frames.contains_key(&(tid, frame_id))
    {
        return Some(Err(ERR_OPAQUE_FRAME));
    }
    // The frame's place among the interpreter frames, bottom first, as the
    // thread will find it.
    let frame_count = listing
        .iter()
        .filter(|e| is_interpreter_row(&ds, tid, e))
        .count();
    let from_top = listing
        .iter()
        .take_while(|e| e.frame_id != frame_id)
        .filter(|e| is_interpreter_row(&ds, tid, e))
        .count();
    let Some(frame_index) = frame_count.checked_sub(from_top.saturating_add(1)) else {
        return Some(Err(ERR_INVALID_FRAMEID));
    };
    let Some(snapshot) = ds.frame_locals.get(&(tid, frame_id)).cloned() else {
        return Some(Err(ERR_INVALID_FRAMEID));
    };
    // Checked, all of them, before anything is held or recorded.
    let mut writes: Vec<(u16, Value, super::DeferredLocal)> = Vec::new();
    for _ in 0..count {
        match check_deferred_write(shared, &ds, &entry, &snapshot, &mut r) {
            Ok(w) => writes.push(w),
            Err(code) => return Some(Err(code)),
        }
    }
    // Hold every object until the thread applies (or drops) its write: its
    // collection disabled — the handle strong, so it is a root and follows
    // moves — and the id pinned, so a debugger that disposes of it frees
    // nothing. A thread id's object is the thread's and needs no hold.
    let mut held: Vec<u64> = Vec::new();
    for (_, _, deferred) in &writes {
        if let super::DeferredLocal::Object(id) = *deferred {
            if ds.objects.handle_of(id).is_none() {
                continue;
            }
            if !super::set_collection_enabled(shared, &mut ds, id, false) {
                for id in held {
                    release_deferred_object(shared, &mut ds, id);
                }
                super::release_disposed_objects(shared, &mut ds);
                return Some(Err(ERR_INVALID_OBJECT));
            }
            ds.objects.note_export(id, ObjectExport::Pinned);
            held.push(id);
        }
    }
    // The snapshot answers the new values at once, pinned like the rest of
    // it; the replaced ones are unpinned after, so an id both name survives.
    let mut replaced = Vec::new();
    let mut recorded = Vec::with_capacity(writes.len());
    for (slot, value, deferred) in writes {
        let local = super::snapshot_local(shared, &mut ds, value);
        if let Some(locals) = ds.frame_locals.get_mut(&(tid, frame_id)) {
            if let Some(old) = locals.get_mut(usize::from(slot)) {
                replaced.extend(old.as_object_id().filter(|&id| id != 0));
                *old = local;
            }
        }
        // A thread id (not in the object table, so not held) is resolved
        // again when applied: its live thread keeps the object.
        recorded.push(super::DeferredFrameWrite {
            frame_index,
            frame_count,
            class_id: entry.class_id,
            method_id: entry.method_id,
            slot,
            value: deferred,
        });
    }
    for id in replaced {
        ds.objects.unpin(id);
    }
    let slots = recorded.len();
    ds.deferred_frame_writes
        .entry(tid)
        .or_default()
        .extend(recorded);
    shared.debug.debugger_gates.set_frame_writes_pending(true);
    super::release_disposed_objects(shared, &mut ds);
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[DEFERRED_SETVALUES] recorded tid={tid} slots={slots}");
    }
    Some(Ok(()))
}

/// `thread`, `frame` and `count` of a `StackFrame.SetValues` payload.
fn read_set_values_head(r: &mut PayloadReader<'_>) -> Result<(u64, u64, u32), u16> {
    Ok((
        wire(r.read_u64_be())?,
        wire(r.read_u64_be())?,
        wire(r.read_u32_be())?,
    ))
}

/// Read and check the next (`slot`, tagged value) of a `SetValues` deferred
/// by [`defer_blocked_frame_write`] against the frame's listing `entry` and
/// its published `snapshot`, as [`set_frame_values`] checks a parked
/// thread's: the slot in range, the value of the declared variable's kind
/// (or, with none declared, of the kind the snapshot holds there), an array
/// for an array variable, an object id that names a live object. Answers
/// the slot, the value as the VM sees it now, and what to record.
fn check_deferred_write(
    shared: &SharedVm,
    ds: &DebugState,
    entry: &super::FrameEntry,
    snapshot: &[super::LocalValue],
    r: &mut PayloadReader<'_>,
) -> Result<(u16, Value, super::DeferredLocal), u16> {
    let pc = entry.offset;
    let slot = wire(r.read_u32_be())?;
    let sent = wire(super::commands::read_tagged_value(r))?;
    let kind = local_kind_of_value(&sent).ok_or(ERR_TYPE_MISMATCH)?;
    let wide = matches!(kind, b'J' | b'D');
    let slot16 = u16::try_from(slot).map_err(|_| ERR_INVALID_SLOT)?;
    let last = usize::from(slot16) + usize::from(wide);
    if last >= snapshot.len() {
        return Err(ERR_INVALID_SLOT);
    }
    let declared = ds
        .method_variables
        .get(&(entry.class_id, entry.method_id))
        .and_then(|vars| {
            vars.iter().find(|v| {
                v.slot == usize::from(slot16)
                    && v.code_index <= pc
                    // Widening: a scope length is at most 65535.
                    && pc < v.code_index + v.length as u64
            })
        })
        .map(|v| v.signature.as_bytes().first().copied().unwrap_or(b'I'));
    let expected = match declared {
        Some(sig) => local_kind_of_signature(sig),
        None => local_kind_of_snapshot(&snapshot[usize::from(slot16)]),
    };
    if kind != expected {
        return Err(ERR_TYPE_MISMATCH);
    }
    let value = debugger_value_to_vm(shared, ds, &sent).map_err(|_| ERR_INVALID_OBJECT)?;
    let deferred = match value {
        Value::Object(Some(o)) => {
            if declared == Some(b'[') && !is_array(shared, o) {
                return Err(ERR_TYPE_MISMATCH);
            }
            // Wave 84 (lane L1), as `set_frame_values`.
            refuse_unfitting_object(shared, ds, entry.class_id, entry.method_id, slot16, pc, o)?;
            let id = match sent {
                DebuggerValue::Object(id)
                | DebuggerValue::Array(id)
                | DebuggerValue::String(id)
                | DebuggerValue::Thread(id)
                | DebuggerValue::ThreadGroup(id)
                | DebuggerValue::ClassLoader(id)
                | DebuggerValue::ClassObject(id) => id,
                _ => return Err(ERR_TYPE_MISMATCH),
            };
            super::DeferredLocal::Object(id)
        }
        other => super::DeferredLocal::Plain(other),
    };
    Ok((slot16, value, deferred))
}

/// Did [`defer_blocked_frame_write`] hold object id `id`? Every id of the
/// object table it names: the hold pins it there until released. A thread
/// id (never in the table) is not held.
fn held_by_deferral(ds: &DebugState, id: u64) -> bool {
    ds.objects.handle_of(id).is_some()
}

/// Undo the hold [`defer_blocked_frame_write`] took on an object id: its
/// collection enabled again and the pin dropped. The caller then frees the
/// released handles (`debug::release_disposed_objects`).
fn release_deferred_object(shared: &SharedVm, ds: &mut DebugState, id: u64) {
    if !held_by_deferral(ds, id) {
        return;
    }
    let _ = super::set_collection_enabled(shared, ds, id, true);
    ds.objects.unpin(id);
}

/// Apply the `StackFrame.SetValues` writes a debugger left for this thread
/// while it was blocked in a native (wave 41, [`defer_blocked_frame_write`]).
/// Called by the thread itself at the end of its blocking region's wake
/// (`NativeContextImpl::check_post_block_gc_refs`), after the collections it
/// slept through are folded into its frames and before its root snapshot is
/// refreshed: a registered mutator between two polls, so an object id's
/// referent does not move between its resolution and the store. One load
/// unless a write waits for some thread.
#[inline(always)]
pub(crate) fn apply_deferred_frame_writes_if_any(shared: &SharedVm, thread: &mut JvmThread) {
    if shared.debug.debugger_gates.frame_writes_pending() {
        apply_deferred_frame_writes(shared, thread);
    }
}

/// [`apply_deferred_frame_writes_if_any`] with a write waiting somewhere. A
/// write whose frame is not the one it was recorded against (the thread's
/// stack changed: a region left without a wake) is dropped, never applied to
/// another frame.
#[cold]
#[inline(never)]
fn apply_deferred_frame_writes(shared: &SharedVm, thread: &mut JvmThread) {
    let tid = thread.thread_id.0;
    let mut ds = shared.debug.debug_state.lock();
    // Interpreter round i2 wave 61, lane L1: a C agent's `NotifyFramePop` /
    // `ClearAllFramePops` made while this thread was blocked.
    if let Some(pops) = ds.deferred_frame_pops.remove(&tid) {
        apply_deferred_frame_pops(thread, &pops);
        // Interpreter round i6 wave 134, lane L1 (i133-L1): a `PopFrame` /
        // `ForceEarlyReturn*` made while it was suspended blocked entering a
        // monitor, which it now holds.
        if let Some(control) = pops.control {
            super::early_return::adopt_blocked_frame_control(shared, &mut ds, thread, control);
        }
        shared
            .debug
            .debugger_gates
            .set_frame_writes_pending(ds.frame_writes_waiting());
    }
    let Some(writes) = ds.deferred_frame_writes.remove(&tid) else {
        return;
    };
    shared
        .debug
        .debugger_gates
        .set_frame_writes_pending(ds.frame_writes_waiting());
    let depth = thread.frames.len();
    let (mut applied, mut dropped) = (0usize, 0usize);
    for w in &writes {
        let value = deferred_value(shared, &ds, w.value);
        let wide = matches!(value, Some(Value::Long(_) | Value::Double(_)));
        let frame = if depth == w.frame_count {
            thread.frames.get_mut(w.frame_index)
        } else {
            None
        };
        let stored = match (frame, value) {
            (Some(frame), Some(value)) => {
                let same_frame = u64::from(frame.class_id.as_u32()) == w.class_id
                    && super::frame_method_id(frame) == w.method_id
                    && usize::from(w.slot) + usize::from(wide) < frame.locals_len();
                if same_frame {
                    frame.set_local(w.slot, value);
                }
                same_frame
            }
            _ => false,
        };
        if stored {
            applied += 1;
        } else {
            dropped += 1;
        }
    }
    for w in &writes {
        release_deferred_value(shared, &mut ds, w.value);
    }
    super::release_disposed_objects(shared, &mut ds);
    drop(ds);
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[DEFERRED_SETVALUES] applied tid={tid} writes={applied} dropped={dropped}");
    }
}

/// The value a deferred write stores, as the VM sees it now: an object id or
/// a JNI global reference resolved (`None`: it names nothing any more).
fn deferred_value(
    shared: &SharedVm,
    ds: &DebugState,
    value: super::DeferredLocal,
) -> Option<Value> {
    match value {
        super::DeferredLocal::Plain(v) => Some(v),
        super::DeferredLocal::Object(id) => {
            super::object_for_id(shared, ds, id).map(|o| Value::Object(Some(o)))
        }
        super::DeferredLocal::Global(global) => shared
            .natives
            .jni_global_refs
            .lock()
            .resolve(global)
            .map(|o| Value::Object(Some(o))),
    }
}

/// Undo the hold a deferred write took on its object: an id's
/// ([`release_deferred_object`]) or a JNI global reference's (deleted; the
/// debug state, then the reference table, is the lock order).
fn release_deferred_value(shared: &SharedVm, ds: &mut DebugState, value: super::DeferredLocal) {
    match value {
        super::DeferredLocal::Object(id) => release_deferred_object(shared, ds, id),
        super::DeferredLocal::Global(global) => {
            let _ = shared.natives.jni_global_refs.lock().remove(global);
        }
        super::DeferredLocal::Plain(_) => {}
    }
}

/// Record a C JVMTI agent's `SetLocalObject` of `slot` in frame `frame_index`
/// (bottom first, of `frame_count`; its class id and JDWP method id) of
/// thread `tid`, blocked in a native region, for the thread's wake to apply
/// (interpreter round i1 wave 47, lane L1; item 1 of
/// `docs/feature-designs/jvmti-delivery-threading.md`):
/// the JDWP deferral of wave 41 ([`defer_blocked_frame_write`]) with the
/// object held by `global`, a JNI global reference this record owns, since a
/// C agent has no JDWP object id (`None`: `null`). The caller holds the thread's inspection
/// window (`interpreter::with_blocked_frames`), so the thread cannot leave
/// its region before the record stands, and its wake, which applies the
/// records after it folded the collections it slept through into its frames
/// (`apply_deferred_frame_writes_if_any`), finds it. `CRATONVM_FRAME_TRACE=1`
/// prints `[DEFERRED_SETVALUES] recorded tid=<n> slots=1 jvmti=true`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn defer_jvmti_object_write(
    shared: &SharedVm,
    tid: u64,
    frame_index: usize,
    frame_count: usize,
    class_id: u64,
    method_id: u64,
    slot: u16,
    global: Option<u64>,
) {
    // `None`: `null`, stored at the wake too (the wake's write-back of the
    // slot's recorded reference would otherwise undo a store made now).
    let value = match global {
        Some(global) => super::DeferredLocal::Global(global),
        None => super::DeferredLocal::Plain(Value::Object(None)),
    };
    let mut ds = shared.debug.debug_state.lock();
    ds.deferred_frame_writes
        .entry(tid)
        .or_default()
        .push(super::DeferredFrameWrite {
            frame_index,
            frame_count,
            class_id,
            method_id,
            slot,
            value,
        });
    shared.debug.debugger_gates.set_frame_writes_pending(true);
    drop(ds);
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[DEFERRED_SETVALUES] recorded tid={tid} slots=1 jvmti=true");
    }
}

/// Record a C JVMTI agent's `NotifyFramePop` of frame `frame_index` (bottom
/// first, of `frame_count`; its class id and JDWP method id) of thread `tid`,
/// suspended while blocked in a native region, for the thread's wake to
/// apply (interpreter round i2 wave 61, lane L1; item 1 of the proposal
/// `docs/feature-designs/jvmti-delivery-threading.md`):
/// the request list is the thread's own, which only it writes. The caller
/// holds the thread's inspection window, as for
/// [`defer_jvmti_object_write`]. Answers `false` for a frame already
/// recorded (`JVMTI_ERROR_DUPLICATE`, as HotSpot answers a second request:
/// MEASURED, `L1W61JvmtiFrameControl`'s `sleeper` rows).
/// `CRATONVM_FRAME_TRACE=1` prints `[DEFERRED_FRAME_POP] recorded tid=<n>
/// frame=<index>`.
pub(crate) fn defer_jvmti_frame_pop(
    shared: &SharedVm,
    tid: u64,
    frame_index: usize,
    frame_count: usize,
    class_id: u64,
    method_id: u64,
) -> bool {
    let mut ds = shared.debug.debug_state.lock();
    let pops = ds.deferred_frame_pops.entry(tid).or_default();
    if pops
        .frames
        .iter()
        .any(|p| p.frame_index == frame_index && p.frame_count == frame_count)
    {
        return false;
    }
    pops.frames.push(super::DeferredFrameWrite {
        frame_index,
        frame_count,
        class_id,
        method_id,
        slot: 0,
        value: super::DeferredLocal::Plain(Value::Int(0)),
    });
    shared.debug.debugger_gates.set_frame_writes_pending(true);
    drop(ds);
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[DEFERRED_FRAME_POP] recorded tid={tid} frame={frame_index}");
    }
    true
}

/// Record a C JVMTI agent's `ClearAllFramePops` of thread `tid`, suspended
/// while blocked in a native region (wave 61): the thread drops its requests
/// at its wake, and the ones deferred before this.
pub(crate) fn defer_jvmti_clear_frame_pops(shared: &SharedVm, tid: u64) {
    let mut ds = shared.debug.debug_state.lock();
    let pops = ds.deferred_frame_pops.entry(tid).or_default();
    pops.clear = true;
    pops.frames.clear();
    shared.debug.debugger_gates.set_frame_writes_pending(true);
}

/// Apply `pops` on the thread that leaves its blocking region (wave 61): its
/// requests dropped for a `ClearAllFramePops`, then each `NotifyFramePop`
/// whose frame is still the one it was recorded against (as a deferred write
/// is applied) added, once.
fn apply_deferred_frame_pops(thread: &mut JvmThread, pops: &super::DeferredFramePops) {
    if pops.clear {
        thread.frame_pop_requests.clear();
    }
    let depth = thread.frames.len();
    let mut added = 0usize;
    for p in &pops.frames {
        let same_frame = depth == p.frame_count
            && thread.frames.get(p.frame_index).is_some_and(|frame| {
                u64::from(frame.class_id.as_u32()) == p.class_id
                    && super::frame_method_id(frame) == p.method_id
            });
        let Ok(index) = u32::try_from(p.frame_index) else {
            continue;
        };
        if same_frame && !thread.frame_pop_requests.contains(&index) {
            thread.frame_pop_requests.push(index);
            added += 1;
        }
    }
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[DEFERRED_FRAME_POP] applied tid={} clear={} added={added}",
            thread.thread_id.0, pops.clear
        );
    }
}

/// The value a write deferred for blocked thread `tid` puts in local `slot`
/// of frame `frame_index` (bottom first) of `frames` (wave 47): what a read
/// of that local answers before the thread wakes, as HotSpot answers the
/// value just written ([`deferred_frame_values`]).
pub(crate) fn deferred_local_value(
    shared: &SharedVm,
    tid: u64,
    frames: &[crate::runtime::frame::Frame],
    frame_index: usize,
    slot: u16,
) -> Option<Value> {
    if !shared.debug.debugger_gates.frame_writes_pending() {
        return None;
    }
    let ds = shared.debug.debug_state.lock();
    deferred_frame_values(shared, &ds, tid, frames)
        .get(&(frame_index, slot))
        .copied()
}

/// The values the writes waiting for blocked thread `tid` put in its frames
/// (wave 41), keyed by (frame index bottom first, slot), for a listing
/// published while it is still blocked (`interpreter::read_blocked_frames`,
/// after a resume and a new suspension): without them that listing would
/// show the frame's old values until the thread wakes. Only writes whose
/// frame is still the one they were recorded against; a later write to a
/// slot wins. On a registered mutator between two polls, with the
/// debug-state lock held (an object id is resolved to its current address).
pub(crate) fn deferred_frame_values(
    shared: &SharedVm,
    ds: &DebugState,
    tid: u64,
    frames: &[crate::runtime::frame::Frame],
) -> std::collections::HashMap<(usize, u16), Value> {
    let mut out = std::collections::HashMap::new();
    let Some(writes) = ds.deferred_frame_writes.get(&tid) else {
        return out;
    };
    for w in writes {
        if w.frame_count != frames.len() {
            continue;
        }
        let Some(frame) = frames.get(w.frame_index) else {
            continue;
        };
        if u64::from(frame.class_id.as_u32()) != w.class_id
            || super::frame_method_id(frame) != w.method_id
        {
            continue;
        }
        if let Some(v) = deferred_value(shared, ds, w.value) {
            out.insert((w.frame_index, w.slot), v);
        }
    }
    out
}

/// Drop the writes left for thread `tid`, which is leaving the VM without
/// another wake (wave 41): their objects' holds are released. One load
/// unless a write waits for some thread.
#[inline(always)]
pub(crate) fn discard_deferred_frame_writes_if_any(shared: &SharedVm, tid: u64) {
    if shared.debug.debugger_gates.frame_writes_pending() {
        discard_deferred_frame_writes(shared, tid);
    }
    // Interpreter round i6 wave 123, lane L2: and a write recorded for one of
    // its compiled activations' post-call exits.
    if shared.debug.debugger_gates.compiled_home_writes_pending() {
        let mut ds = shared.debug.debug_state.lock();
        discard_compiled_home_writes(shared, &mut ds, tid);
        super::release_disposed_objects(shared, &mut ds);
    }
}

/// [`discard_deferred_frame_writes_if_any`] with a write waiting somewhere.
#[cold]
#[inline(never)]
fn discard_deferred_frame_writes(shared: &SharedVm, tid: u64) {
    let mut ds = shared.debug.debug_state.lock();
    // Wave 61: and a C agent's frame-pop requests (wave 134: and its frame
    // control, whose object is released).
    if let Some(pops) = ds.deferred_frame_pops.remove(&tid) {
        if let Some(control) = pops.control.as_ref() {
            super::early_return::release_blocked_frame_control(shared, control);
        }
        shared
            .debug
            .debugger_gates
            .set_frame_writes_pending(ds.frame_writes_waiting());
    }
    let Some(writes) = ds.deferred_frame_writes.remove(&tid) else {
        return;
    };
    shared
        .debug
        .debugger_gates
        .set_frame_writes_pending(ds.frame_writes_waiting());
    for w in writes {
        release_deferred_value(shared, &mut ds, w.value);
    }
    super::release_disposed_objects(shared, &mut ds);
}

/// The kind of value a local of JNI signature byte `sig` holds: `I` for the
/// int family (`Z B C S I`, which the JVM keeps as ints), `J`, `F`, `D`, or
/// `L` for any reference.
pub(crate) fn local_kind_of_signature(sig: u8) -> u8 {
    match sig {
        b'Z' | b'B' | b'C' | b'S' | b'I' => b'I',
        b'J' | b'F' | b'D' => sig,
        _ => b'L',
    }
}

/// May an object of class `class_id` be stored into a local whose
/// `LocalVariableTable` signature is `signature` (`Lp/X;` or an array
/// descriptor)? HotSpot's `is_assignable` (`jvmtiImpl.cpp`), which
/// `VM_BaseGetOrSetLocal::check_slot_type_lvt` applies to a non-null value
/// of `SetLocalObject` (and so to JDWP's `StackFrame.SetValues`), answering
/// `TYPE_MISMATCH` when it fails: by NAME, against the class itself, its
/// superclasses and its interfaces, an array also against `Cloneable`,
/// `Serializable` and the arrays of its component's supertypes. A class the
/// class manager cannot find is not refused. Interpreter round i4 wave 84,
/// lane L1: no write path checked it, so a debugger could store a `String`
/// into an `Integer` local and the method's next `getfield` or
/// `invokevirtual` ran on an object of the wrong class.
pub(crate) fn object_fits_local_signature(
    cm: &ClassManager,
    class_id: ClassId,
    signature: &str,
) -> bool {
    class_fits_name(cm, class_id, reference_name_of_signature(signature), 0)
}

/// `Lp/X;` as `p/X`; an array descriptor as itself.
fn reference_name_of_signature(signature: &str) -> &str {
    signature
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
        .unwrap_or(signature)
}

fn class_fits_name(cm: &ClassManager, class_id: ClassId, target: &str, depth: u8) -> bool {
    if target == "java/lang/Object" {
        return true;
    }
    let Some(class) = cm.get_class(class_id) else {
        return true;
    };
    if &*class.name == target {
        return true;
    }
    if !class.name.starts_with('[') {
        return cm.is_assignable_to_name(class_id, target);
    }
    if target == "java/lang/Cloneable" || target == "java/io/Serializable" {
        return true;
    }
    // An array of references fits an array whose component its component
    // fits; a primitive array fits only its own name (matched above).
    let Some(target_component) = target.strip_prefix('[') else {
        return false;
    };
    if !(target_component.starts_with('L') || target_component.starts_with('[')) {
        return false;
    }
    match &class.array_info {
        Some(info) if depth < 255 => class_fits_name(
            cm,
            info.component_class_id,
            reference_name_of_signature(target_component),
            depth + 1,
        ),
        _ => false,
    }
}

/// The `LocalVariableTable` signature of the variable in `slot` of the
/// method `method_id` of class `class_id` in scope at bytecode `pc`, from the
/// debugger's per-method variable table; `None` when there is none.
fn declared_local_signature(
    ds: &DebugState,
    class_id: u64,
    method_id: u64,
    slot: u16,
    pc: u64,
) -> Option<String> {
    ds.method_variables
        .get(&(class_id, method_id))?
        .iter()
        .find(|v| {
            v.slot == usize::from(slot)
                && v.code_index <= pc
                // Widening: a scope length is at most 65535.
                && pc < v.code_index + v.length as u64
        })
        .map(|v| v.signature.clone())
}

/// `TYPE_MISMATCH` when object `o` does not fit the variable declared in
/// `slot` at `pc` ([`object_fits_local_signature`]); nothing when no
/// variable is declared there or `o` fits.
///
/// The callers hold the debug state, which the lock order puts AFTER the
/// class manager: the class manager is only TRIED here
/// (`try_read_untracked`, which cannot block, so cannot deadlock), and a
/// write that finds it write-locked (a class being defined that instant)
/// is let through unchecked, as every write was before.
fn refuse_unfitting_object(
    shared: &SharedVm,
    ds: &DebugState,
    class_id: u64,
    method_id: u64,
    slot: u16,
    pc: u64,
    o: ObjectRef,
) -> Result<(), u16> {
    let Some(signature) = declared_local_signature(ds, class_id, method_id, slot, pc) else {
        return Ok(());
    };
    let object_class = shared.mem.heap.class_id_of(o);
    let Some(cm) = shared.classes.class_manager.try_read_untracked() else {
        return Ok(());
    };
    if object_fits_local_signature(&cm, object_class, &signature) {
        Ok(())
    } else {
        Err(ERR_TYPE_MISMATCH)
    }
}

/// The kind ([`local_kind_of_signature`]) of a value the debugger sent;
/// `None` for `void`.
fn local_kind_of_value(v: &DebuggerValue) -> Option<u8> {
    Some(match v {
        DebuggerValue::Void => return None,
        DebuggerValue::Boolean(_)
        | DebuggerValue::Byte(_)
        | DebuggerValue::Char(_)
        | DebuggerValue::Short(_)
        | DebuggerValue::Int(_) => b'I',
        DebuggerValue::Long(_) => b'J',
        DebuggerValue::Float(_) => b'F',
        DebuggerValue::Double(_) => b'D',
        DebuggerValue::Object(_)
        | DebuggerValue::Array(_)
        | DebuggerValue::String(_)
        | DebuggerValue::Thread(_)
        | DebuggerValue::ThreadGroup(_)
        | DebuggerValue::ClassLoader(_)
        | DebuggerValue::ClassObject(_) => b'L',
    })
}

/// The kind ([`local_kind_of_signature`]) of the value a frame local holds
/// now; `None` for a slot that holds nothing typed.
fn local_kind_of_current(v: Value) -> Option<u8> {
    match v {
        Value::Int(_) => Some(b'I'),
        Value::Long(_) => Some(b'J'),
        Value::Float(_) => Some(b'F'),
        Value::Double(_) => Some(b'D'),
        Value::Object(_) => Some(b'L'),
        _ => None,
    }
}

/// `StackFrame.SetValues` on the parked thread that owns the frame (wave 9).
///
/// The frame id names an entry of the snapshot the thread published when it
/// parked (`interpreter::publish_debugger_frames`). Since wave 21 that
/// snapshot also lists the compiled activations between interpreted frames;
/// they, and an interpreter frame whose body runs compiled, have no locals
/// to write (`OPAQUE_FRAME`), and the frame is found among the interpreter
/// frames by skipping them. Every write is checked before any is made (JDWP):
///
/// * the slot must lie in the frame (`INVALID_SLOT`; a `long` / `double`
///   takes two slots);
/// * its kind must match the variable's: the `LocalVariableTable` entry that
///   covers the slot at the frame's location, else — no entry, a class
///   compiled without `-g` — the kind of the value the slot holds now
///   (`TYPE_MISMATCH`; `INVALID_SLOT` for a slot holding nothing typed). A
///   reference stored where the frame holds a primitive, or the reverse,
///   would break the collector's reading of the frame; that is what this
///   check stands guard over. JDI checks a reference's class against the
///   variable's declared type before it sends; an array variable must get an
///   array here (`TYPE_MISMATCH`).
///
/// The locals are then stored through `Frame::set_local` (the second slot of
/// a `long` / `double` is overwritten as `lstore` / `dstore` leave it) and
/// the published snapshot is refreshed, so a following `GetValues` sees the
/// new values. The frame is not executing while its thread is parked; the
/// bytecode after the stop reads the new value. No allocation happens here,
/// so no collection can run between resolving an object id and the store.
fn set_frame_values(shared: &SharedVm, thread: &mut JvmThread, data: &[u8]) -> Result<(), u16> {
    let mut r = PayloadReader::new(data);
    let tid = wire(r.read_u64_be())?;
    let frame_id = wire(r.read_u64_be())?;
    let count = wire(r.read_u32_be())?;
    let mut ds = shared.debug.debug_state.lock();
    let entry = ds
        .thread_frames
        .get(&tid)
        .and_then(|frames| frames.iter().find(|e| e.frame_id == frame_id))
        .cloned()
        .ok_or(ERR_INVALID_FRAMEID)?;
    // Interpreter round i6 wave 123, lane L2 (stage 3b of i43-L1,
    // `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL`): a compiled activation whose
    // locals were read from their homes may take the write, recorded for
    // its post-call exit ([`set_compiled_home_values`]).
    if entry.offset != super::NATIVE_FRAME_LOCATION {
        if let Some(facts) = ds.compiled_home_rows.get(&(tid, frame_id)).copied() {
            return set_compiled_home_values(
                shared, ds, tid, frame_id, &entry, facts, &mut r, count,
            );
        }
    }
    // A native method's frame (wave 15) has no locals to write; nor has a
    // compiled activation, or a frame whose body runs compiled (wave 21).
    // Interpreter round i7 wave 138, lane L1 (`DebugState::proxy_frames`):
    // nor has a lambda proxy method's row a `Frame` to write (its locals are
    // the call's record; HotSpot writes them, see
    // `docs/feature-designs/jvmti-delivery-threading.md`).
    if entry.offset == super::NATIVE_FRAME_LOCATION
        || ds.opaque_frames.contains_key(&(tid, frame_id))
        || ds.proxy_frames.contains(&(tid, frame_id))
    {
        return Err(ERR_OPAQUE_FRAME);
    }
    let depth = thread.frames.len();
    // The frame's place among the interpreter frames, top first. The listing
    // may interleave frames that have no `Frame`: a native method's, and
    // (wave 21) the compiled activations between interpreted callers, so the
    // frame id is not the depth from the top. Wave 138 (L1): nor has a
    // lambda proxy method's row; counting it made every write to a frame
    // below a lambda's proxy `INVALID_FRAMEID` (the frame found was the one
    // under it, whose method differs).
    let from_top = ds.thread_frames.get(&tid).map_or(0, |frames| {
        frames
            .iter()
            .take_while(|e| e.frame_id != frame_id)
            .filter(|e| {
                e.offset != super::NATIVE_FRAME_LOCATION
                    && !ds
                        .opaque_frames
                        .get(&(tid, e.frame_id))
                        .copied()
                        .unwrap_or(false)
                    && !ds.proxy_frames.contains(&(tid, e.frame_id))
            })
            .count()
    });
    let index = depth
        .checked_sub(from_top.saturating_add(1))
        .ok_or(ERR_INVALID_FRAMEID)?;
    let frame = thread.frames.get(index).ok_or(ERR_INVALID_FRAMEID)?;
    // The snapshot must describe this frame (it does while the thread stays
    // parked at the level that published it).
    if u64::from(frame.class_id.as_u32()) != entry.class_id
        || super::frame_method_id(frame) != entry.method_id
    {
        return Err(ERR_INVALID_FRAMEID);
    }
    let pc = entry.offset;
    let mut writes: Vec<(u16, Value)> = Vec::new();
    for _ in 0..count {
        let slot = wire(r.read_u32_be())?;
        let sent = wire(super::commands::read_tagged_value(&mut r))?;
        let kind = local_kind_of_value(&sent).ok_or(ERR_TYPE_MISMATCH)?;
        let wide = matches!(kind, b'J' | b'D');
        let slot16 = u16::try_from(slot).map_err(|_| ERR_INVALID_SLOT)?;
        let last = usize::from(slot16) + usize::from(wide);
        if last >= frame.locals_len() {
            return Err(ERR_INVALID_SLOT);
        }
        let declared = ds
            .method_variables
            .get(&(entry.class_id, entry.method_id))
            .and_then(|vars| {
                vars.iter().find(|v| {
                    v.slot == usize::from(slot16)
                        && v.code_index <= pc
                        // Widening: a scope length is at most 65535.
                        && pc < v.code_index + v.length as u64
                })
            })
            .map(|v| v.signature.as_bytes().first().copied().unwrap_or(b'I'));
        let expected = match declared {
            Some(sig) => local_kind_of_signature(sig),
            None => local_kind_of_current(super::debugger_local(frame, slot16))
                .ok_or(ERR_INVALID_SLOT)?,
        };
        if kind != expected {
            return Err(ERR_TYPE_MISMATCH);
        }
        let value = debugger_value_to_vm(shared, &ds, &sent).map_err(|_| ERR_INVALID_OBJECT)?;
        if declared == Some(b'[') {
            if let Value::Object(Some(o)) = value {
                if !is_array(shared, o) {
                    return Err(ERR_TYPE_MISMATCH);
                }
            }
        }
        // Wave 84 (lane L1): the object's class against the variable's, as
        // HotSpot's `SetLocalObject` checks it under the back end.
        if let Value::Object(Some(o)) = value {
            refuse_unfitting_object(shared, &ds, entry.class_id, entry.method_id, slot16, pc, o)?;
        }
        writes.push((slot16, value));
    }
    let Some(frame) = thread.frames.get_mut(index) else {
        return Err(ERR_INVALID_FRAMEID);
    };
    let mut touched = Vec::with_capacity(writes.len() * 2);
    for (slot, value) in writes {
        frame.set_local(slot, value);
        touched.push(slot);
        if matches!(value, Value::Long(_) | Value::Double(_)) {
            // In range: checked against `locals_len` above.
            touched.push(slot + 1);
        }
    }
    // Refresh the snapshot: the new values, pinned like the rest of it, and
    // the replaced ones unpinned (after, so an id both name survives).
    let fresh: Vec<(usize, super::LocalValue)> = touched
        .into_iter()
        .map(|slot| {
            let v = super::debugger_local(frame, slot);
            (usize::from(slot), super::snapshot_local(shared, &mut ds, v))
        })
        .collect();
    let object_id = |local: &super::LocalValue| local.as_object_id().filter(|&id| id != 0);
    let mut replaced = Vec::new();
    match ds.frame_locals.get_mut(&(tid, frame_id)) {
        Some(locals) => {
            for (slot, local) in fresh {
                match locals.get_mut(slot) {
                    Some(old) => {
                        replaced.extend(object_id(&*old));
                        *old = local;
                    }
                    None => replaced.extend(object_id(&local)),
                }
            }
        }
        None => replaced.extend(fresh.iter().filter_map(|(_, local)| object_id(local))),
    }
    for id in replaced {
        ds.objects.unpin(id);
    }
    super::release_disposed_objects(shared, &mut ds);
    Ok(())
}

// ---------------------------------------------------------------------------
// `StackFrame.SetValues` on a compiled activation (interpreter round i6 wave
// 123, lane L2; stage 3b of
// `docs/feature-designs/jvmti-delivery-threading.md`)
// ---------------------------------------------------------------------------
//
// A compiled activation's home slots are store-only: after the call it is
// suspended in returns, its body reads a register-homed local from its
// register, so a value written into the slot would be lost. The write is
// therefore recorded, and the body is forced to leave for the interpreter at
// the post-call exit right after that call
// (`jit_bridge::force_compiled_home_exit`): the exit's resume rebuilds the
// frame as an interpreter frame, and the recorded values are stored into it
// before it runs a bytecode ([`apply_compiled_home_writes`]). Only with
// `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL`; anything the force refuses keeps
// `OPAQUE_FRAME`, as before.

/// `StackFrame.SetValues` on the published compiled-home row `frame_id` of
/// `tid` (this parked thread's own), with `facts` its body. Every write is
/// checked before anything is forced or recorded, as
/// [`defer_blocked_frame_write`] checks against the published snapshot
/// (`INVALID_SLOT`, `TYPE_MISMATCH`, `INVALID_OBJECT`); then the body is
/// forced (the debug-state lock released: the withdrawal patches code under
/// the code cache's own locks), and a refused force answers `OPAQUE_FRAME`.
/// On success the writes are recorded with their objects held, and the
/// snapshot answers the new values at once. `CRATONVM_FRAME_TRACE=1` prints
/// `[COMPILED_SETVALUES] recorded|refused ...`.
#[allow(clippy::too_many_arguments)]
fn set_compiled_home_values(
    shared: &SharedVm,
    mut ds: parking_lot::MutexGuard<'_, DebugState>,
    tid: u64,
    frame_id: u64,
    entry: &super::FrameEntry,
    facts: super::CompiledHomeRowFacts,
    r: &mut PayloadReader<'_>,
    count: u32,
) -> Result<(), u16> {
    if !cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL")
        || facts.body.cm_ptr == 0
        || facts.body.frame_rbp == 0
    {
        return Err(ERR_OPAQUE_FRAME);
    }
    let snapshot = ds
        .frame_locals
        .get(&(tid, frame_id))
        .cloned()
        .ok_or(ERR_OPAQUE_FRAME)?;
    let mut writes: Vec<(u16, super::DeferredLocal)> = Vec::new();
    for _ in 0..count {
        let (slot, _, deferred) = check_deferred_write(shared, &ds, entry, &snapshot, r)?;
        writes.push((slot, deferred));
    }
    drop(ds);
    // SAFETY: the row stands in this thread's published snapshot
    // (`DebugState::compiled_home_rows` is withdrawn with it), and this runs
    // on that thread, parked at its suspend point: the activation, and so the
    // body it runs, is live and does not move until the thread resumes.
    let cm = unsafe { &*(facts.body.cm_ptr as *const cratonvm_jit::CompiledMethod) };
    let verdict = crate::runtime::interpreter::jit_bridge::force_compiled_home_exit(
        shared,
        cm,
        facts.call_bci,
        facts.locals,
    );
    let cratonvm_jit::DebuggerForceVerdict::Leaves { exit_bci } = verdict else {
        if crate::runtime::env_cache::frame_trace() {
            eprintln!(
                "[COMPILED_SETVALUES] refused tid={tid} {} bci={}: {}",
                cm.method_label,
                facts.call_bci,
                verdict.name()
            );
        }
        return Err(ERR_OPAQUE_FRAME);
    };
    let mut ds = shared.debug.debug_state.lock();
    // The snapshot must still stand (no resume can have run: this thread is
    // the one that would resume).
    if !ds.compiled_home_rows.contains_key(&(tid, frame_id)) {
        return Err(ERR_INVALID_FRAMEID);
    }
    // Hold every object until the exit applies (or a drop releases) the
    // write, as `defer_blocked_frame_write` holds its own.
    let mut held: Vec<u64> = Vec::new();
    for (_, deferred) in &writes {
        if let super::DeferredLocal::Object(id) = *deferred {
            if ds.objects.handle_of(id).is_none() {
                continue;
            }
            if !super::set_collection_enabled(shared, &mut ds, id, false) {
                for id in held {
                    release_deferred_object(shared, &mut ds, id);
                }
                super::release_disposed_objects(shared, &mut ds);
                return Err(ERR_INVALID_OBJECT);
            }
            ds.objects.note_export(id, ObjectExport::Pinned);
            held.push(id);
        }
    }
    let mut replaced = Vec::new();
    let mut recorded = Vec::with_capacity(writes.len());
    for (slot, deferred) in writes {
        if let Some(value) = deferred_value(shared, &ds, deferred) {
            let local = super::snapshot_local(shared, &mut ds, value);
            if let Some(locals) = ds.frame_locals.get_mut(&(tid, frame_id)) {
                if let Some(old) = locals.get_mut(usize::from(slot)) {
                    replaced.extend(old.as_object_id().filter(|&id| id != 0));
                    *old = local;
                }
            }
        }
        recorded.push(super::CompiledHomeWrite {
            body: facts.body,
            exit_bci,
            class_id: entry.class_id,
            method_id: entry.method_id,
            slot,
            value: deferred,
        });
    }
    for id in replaced {
        ds.objects.unpin(id);
    }
    let slots = recorded.len();
    let pending = ds.compiled_home_writes.entry(tid).or_default();
    // A later write to the same slot of the same activation replaces the
    // earlier one; its hold is released below.
    let mut superseded = Vec::new();
    pending.retain(|w| {
        let same = w.body == facts.body && recorded.iter().any(|n| n.slot == w.slot);
        if same {
            superseded.push(w.value);
        }
        !same
    });
    pending.extend(recorded);
    for value in superseded {
        release_deferred_value(shared, &mut ds, value);
    }
    shared
        .debug
        .debugger_gates
        .set_compiled_home_writes_pending(true);
    super::release_disposed_objects(shared, &mut ds);
    // Wave 124 (L2, i124-L9): this is the owner thread, and a pop it made
    // before this record describes some earlier activation: forgotten, so
    // only the exit the force sends this activation through can match.
    cratonvm_jit::deopt::clear_last_taken_frame();
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[COMPILED_SETVALUES] recorded tid={tid} {} call-bci={} exit-bci={exit_bci} slots={slots}",
            cm.method_label, facts.call_bci,
        );
    }
    Ok(())
}

/// The values the pending writes of thread `tid` hold for the compiled
/// activation `body` ([`DebugState::compiled_home_writes`]), by slot, as the
/// VM sees them now: what a re-published listing of that activation answers
/// in place of its homes (interpreter round i6 wave 123, lane L2). Under the
/// debug-state lock.
pub(crate) fn pending_compiled_home_values(
    shared: &SharedVm,
    ds: &DebugState,
    tid: u64,
    body: crate::threading::jvm_thread::CompiledHomeBody,
) -> Vec<(u16, Value)> {
    let Some(writes) = ds.compiled_home_writes.get(&tid) else {
        return Vec::new();
    };
    writes
        .iter()
        .filter(|w| w.body == body)
        .filter_map(|w| Some((w.slot, deferred_value(shared, ds, w.value)?)))
        .collect()
}

/// Does the pending write `w` belong to the frame a post-call exit just
/// rebuilt: the trampoline's frame base `rbp`, the resume bci `bci`, the
/// frame's class and method, and, when the stash named it, the body that left
/// (`body`, `cratonvm_jit::deopt::take_last_taken_frame`)? Wave 124 (L2,
/// `i124-L9-compiled-home-write-matches-a-stale-take`): the class and method
/// are compared always, not only for an unnamed body -- the frame being
/// built is the authority, and a take that described another activation
/// must never put a write into another method's locals.
fn compiled_home_write_matches(
    w: &super::CompiledHomeWrite,
    rbp: u64,
    body: Option<usize>,
    bci: u32,
    class_id: u64,
    method_id: u64,
) -> bool {
    // Widening: an address.
    w.body.frame_rbp as u64 == rbp
        && rbp != 0
        && w.exit_bci == bci
        && w.class_id == class_id
        && w.method_id == method_id
        && body.is_none_or(|cm_ptr| cm_ptr == w.body.cm_ptr)
}

/// Store the `SetValues` writes recorded for the compiled activation whose
/// post-call exit the deopt resume just rebuilt as `frame`
/// (`deopt_resume::build_deopt_frame_or_refusal`), before it runs a bytecode
/// (interpreter round i6 wave 123, lane L2). The activation is named by what
/// this thread last took from the deopt stash
/// (`cratonvm_jit::deopt::take_last_taken_frame`): its frame base, its body, and
/// the frame's resume bci. Matched writes are stored and dropped, their holds
/// released; nothing else is touched. No allocation: an object id resolves
/// to its current address, and the caller pushes the frame before anything
/// can collect (its locals' own window). `CRATONVM_FRAME_TRACE=1` prints
/// `[COMPILED_SETVALUES] applied tid=<n> writes=<k>`.
#[cold]
#[inline(never)]
pub(crate) fn apply_compiled_home_writes(
    shared: &SharedVm,
    thread: &JvmThread,
    frame: &mut crate::runtime::frame::Frame,
) {
    let tid = thread.thread_id.0;
    // Wave 124 (L2, i124-L9): consumed as it is read, so a later build on
    // this thread that popped nothing cannot match this take.
    let (rbp, body, bci) = cratonvm_jit::deopt::take_last_taken_frame();
    // Widening: a frame's pc is a bytecode index.
    if rbp == 0 || u64::try_from(frame.pc).ok() != Some(u64::from(bci)) {
        return;
    }
    let body = body.map(|b| std::sync::Arc::as_ptr(&b) as usize);
    let class_id = u64::from(frame.class_id.as_u32());
    let method_id = super::frame_method_id(frame);
    let mut ds = shared.debug.debug_state.lock();
    let Some(writes) = ds.compiled_home_writes.get_mut(&tid) else {
        return;
    };
    let mut matched = Vec::new();
    writes.retain(|w| {
        let hit = compiled_home_write_matches(w, rbp, body, bci, class_id, method_id);
        if hit {
            matched.push(*w);
        }
        !hit
    });
    if writes.is_empty() {
        ds.compiled_home_writes.remove(&tid);
    }
    shared
        .debug
        .debugger_gates
        .set_compiled_home_writes_pending(!ds.compiled_home_writes.is_empty());
    if matched.is_empty() {
        return;
    }
    let mut applied = 0usize;
    for w in &matched {
        let Some(value) = deferred_value(shared, &ds, w.value) else {
            continue;
        };
        let wide = matches!(value, Value::Long(_) | Value::Double(_));
        if usize::from(w.slot) + usize::from(wide) < frame.locals_len() {
            frame.set_local(w.slot, value);
            applied += 1;
        }
    }
    for w in &matched {
        release_deferred_value(shared, &mut ds, w.value);
    }
    super::release_disposed_objects(shared, &mut ds);
    drop(ds);
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[COMPILED_SETVALUES] applied tid={tid} writes={applied} of {}",
            matched.len()
        );
    }
}

/// Drop thread `tid`'s pending compiled-home writes whose activation the
/// listing it just published no longer holds (interpreter round i6 wave 124,
/// lane L2; fix 3 of `i124-L9-compiled-home-write-matches-a-stale-take`).
/// `listed` are the bodies and frame bases of the compiled activations that
/// capture read homes for (`BlockedCompiledView::homes`). Called only by a
/// publish whose capture reads homes on the owner thread
/// (`interpreter::publish_debugger_frames_under`): every activation that took
/// a write was read that way, deterministically, so an activation still on
/// the stack is listed again, and one missing has left without its exit (an
/// exception unwound it, or a pop): its write can never be applied, and its
/// held object is released now rather than at thread exit. A publish that
/// reads no homes (a native-exit park) drops nothing. Under the debug-state
/// lock.
pub(crate) fn drop_unlisted_compiled_home_writes(
    shared: &SharedVm,
    ds: &mut DebugState,
    tid: u64,
    listed: &[crate::threading::jvm_thread::CompiledHomeBody],
) {
    let Some(writes) = ds.compiled_home_writes.get_mut(&tid) else {
        return;
    };
    let mut dropped = Vec::new();
    writes.retain(|w| {
        let keep = listed.contains(&w.body);
        if !keep {
            dropped.push(w.value);
        }
        keep
    });
    if dropped.is_empty() {
        return;
    }
    if writes.is_empty() {
        ds.compiled_home_writes.remove(&tid);
    }
    shared
        .debug
        .debugger_gates
        .set_compiled_home_writes_pending(!ds.compiled_home_writes.is_empty());
    let n = dropped.len();
    for value in dropped {
        release_deferred_value(shared, ds, value);
    }
    super::release_disposed_objects(shared, ds);
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[COMPILED_SETVALUES] dropped tid={tid} writes={n}: activation no longer listed");
    }
}

/// Drop the `SetValues` writes recorded for thread `tid`'s compiled
/// activations (wave 123, lane L2): the thread is leaving the VM. Their
/// objects' holds are released. One load unless such a write waits.
fn discard_compiled_home_writes(shared: &SharedVm, ds: &mut DebugState, tid: u64) {
    let Some(writes) = ds.compiled_home_writes.remove(&tid) else {
        return;
    };
    shared
        .debug
        .debugger_gates
        .set_compiled_home_writes_pending(!ds.compiled_home_writes.is_empty());
    for w in writes {
        release_deferred_value(shared, ds, w.value);
    }
}

/// A heap command on a parked (registered) thread.
fn run_on_mutator(
    shared: &SharedVm,
    thread: &mut JvmThread,
    command_set: u8,
    command: u8,
    data: &[u8],
) -> CommandResult {
    if (command_set, command) == (CS_VM, CMD_VM_CREATE_STRING) {
        return create_string(shared, thread, data);
    }
    if (command_set, command) == (CS_REF_TYPE, CMD_RT_CLASS_OBJECT) {
        return class_object(shared, data);
    }
    // Interpreter round i1 wave 24: the commands that ask the VM's own
    // natives (a class's loader as `Class.getClassLoader0` answers it, a
    // field read by name, `Thread.interrupt0`'s work) on this mutator, with
    // no lock held.
    match (command_set, command) {
        (CS_REF_TYPE, CMD_RT_CLASS_LOADER) => return class_loader_command(shared, thread, data),
        (CS_CLASSLOADER_REF, CMD_CLR_VISIBLE_CLASSES) => {
            return visible_classes(shared, thread, data)
        }
        (CS_THREAD_REF, CMD_TR_INTERRUPT) => return interrupt_thread(shared, thread, data),
        (CS_THREAD_REF, CMD_TR_STOP) => return stop_thread(shared, thread, data),
        // Wave 25: an array's reference type is its array class.
        (CS_OBJECT_REF, CMD_OR_REFERENCE_TYPE) => {
            if let Some(answer) = array_reference_type(shared, data) {
                return answer;
            }
        }
        (set, cmd) if is_module_command(set, cmd) => {
            return module_command(shared, thread, set, cmd, data)
        }
        (set, cmd) if is_thread_group_command(set, cmd) => {
            return thread_group_command(shared, thread, set, cmd, data)
        }
        // Interpreter round i5 wave 103 (lane L1).
        (set, cmd) if is_heap_walk_command(set, cmd) => {
            return heap_walk_command(shared, thread, set, cmd, data)
        }
        _ => {}
    }
    let cm = shared.classes.class_manager.read();
    let mut ds = shared.debug.debug_state.lock();
    execute(shared, &cm, &mut ds, command_set, command, data)
}

// ---------------------------------------------------------------------------
// Class loaders (interpreter round i1 wave 24)
// ---------------------------------------------------------------------------

/// The longest `ClassLoader.parent` chain [`visible_classes`] follows (a
/// guard against a corrupt one, far above any real one).
const MAX_LOADER_CHAIN: usize = 64;

/// The loader of class `class_id` as the program sees it: what
/// `Class.getClassLoader0` answers for the class's mirror — the VM's native
/// when one is registered, else the mirror's `classLoader` field, which the
/// JDK method reads. `None` for the bootstrap loader. On a mutator with no
/// lock held: the mirror may be created here.
pub(crate) fn class_loader_of(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
) -> Option<ObjectRef> {
    use cratonvm_native_api::NativeContext as _;
    // A class a user loader defined answers that loader's object from its
    // namespace (interpreter round i2 wave 53, lane L1), without the mirror:
    // the C table's `ClassLoad` asks while the define is still in flight,
    // before `ClassLoader.defineClass1` writes the mirror's `classLoader`
    // field and registers the defining loader (`--jdk-only` read `null`
    // there, `L1W53JvmtiClassHooks`' `Defined` load row), and a mirror
    // minted before that registration is rooted for good
    // (`lang_system`'s define natives).
    let user_namespace = shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .and_then(|class| match class.loader_id {
            cratonvm_types::ClassLoaderId::UserDefined(id) => Some(id),
            _ => None,
        });
    if let Some(id) = user_namespace {
        let ctx = crate::vm::NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        if let Some(loader) = cratonvm_native_builtins::classloader::user_loader_object(&ctx, id) {
            return Some(loader);
        }
    }
    let mirror = crate::vm::try_get_or_create_class_mirror(shared, class_id)?;
    let native = shared.natives.native_methods.find(
        "java/lang/Class",
        "getClassLoader0",
        "()Ljava/lang/ClassLoader;",
    );
    let mut ctx = crate::vm::NativeContextImpl {
        shared,
        thread: &mut *thread,
    };
    match native {
        Some(callback) => match callback(&mut ctx, &[Value::Object(Some(mirror))]) {
            Ok(Some(Value::Object(loader))) => loader,
            _ => None,
        },
        None => match ctx.get_field_by_name(mirror, "classLoader") {
            Value::Object(loader) => loader,
            _ => None,
        },
    }
}

/// `ReferenceType.ClassLoader` (2/2) on a mutator (interpreter round i1 wave
/// 24): the class's loader as the program sees it ([`class_loader_of`]). It
/// read only the side table of the loaders `ClassLoader.defineClass`
/// registers, so every class the application class loader defines — the
/// program's own — answered the bootstrap loader (`null`): JDI's
/// `classLoader()` was null for them, and `ClassLoaderReference`
/// commands could not be reached from a class. The quiesced path
/// ([`rt_class_loader`]) still answers from the side table.
/// The class whose defining loader is `class_id`'s: itself when the class
/// store holds it, the host class for a lambda proxy class (a hidden class
/// is defined by its host's loader; the i54-L1 retirement, MEASURED
/// `L1K54JdiLambdaProxyFrames` `loader: the host's`); `None` for neither.
fn loader_holder(shared: &SharedVm, class_id: ClassId) -> Option<ClassId> {
    if shared
        .classes
        .class_manager
        .read()
        .class_store
        .get(class_id)
        .is_some()
    {
        return Some(class_id);
    }
    let (host, name) = crate::runtime::interpreter::lambda_proxy_host(shared, class_id)?;
    host.or_else(|| {
        shared
            .classes
            .class_manager
            .read()
            .resolve_fast_path_class_id(&name)
    })
}

/// [`loader_holder`] of a lambda proxy class under a class-manager lock the
/// caller holds (the quiesced server-thread path): the recorded host, else
/// the implementation method's owner by name.
fn proxy_host_under(shared: &SharedVm, cm: &ClassManager, class_id: ClassId) -> Option<ClassId> {
    if let Some(host) = shared
        .classes
        .lambda_proxy_hosts
        .read()
        .get(&class_id)
        .copied()
    {
        return Some(host);
    }
    let owner = shared
        .classes
        .lambda_proxies
        .read()
        .get(&class_id)
        .map(|site| site.impl_handle.class_name.to_string())?;
    cm.resolve_fast_path_class_id(&owner)
}

fn class_loader_command(shared: &SharedVm, thread: &mut JvmThread, data: &[u8]) -> CommandResult {
    let Ok(raw) = PayloadReader::new(data).read_u64_be() else {
        return CommandResult::error(ERR_INTERNAL);
    };
    let Ok(raw) = u32::try_from(raw) else {
        return CommandResult::error(ERR_INVALID_CLASS);
    };
    let Some(class_id) = loader_holder(shared, ClassId::new(raw)) else {
        return CommandResult::error(ERR_INVALID_CLASS);
    };
    let loader = class_loader_of(shared, thread, class_id);
    // No safepoint between the answer and the export.
    let mut ds = shared.debug.debug_state.lock();
    let id = loader.map_or(0, |l| {
        super::export_object(shared, &mut ds, l, ObjectExport::Sent)
    });
    let mut pw = PayloadWriter::new();
    pw.put_u64_be(id);
    CommandResult::ok(pw.into_bytes())
}

/// `ClassLoaderReference.VisibleClasses` (14/1) on a mutator (interpreter
/// round i1 wave 24): the classes whose loader ([`class_loader_of`]) is the
/// loader named or one of its ancestors (`ClassLoader.parent`, up to the
/// bootstrap loader) — every class that loader can name through delegation.
/// HotSpot answers the classes the loader is an INITIATING loader of (JVMTI
/// `GetClassLoaderClasses`), a subset of these: the ones it has actually
/// been asked for. JDI resolves a field's or a variable's type through this
/// list (`ClassLoaderReferenceImpl.findType`) and lists `definedClasses()`
/// from it; the old answer, every class of every loader, put classes of
/// unrelated loaders into both. `INVALID_OBJECT` for an id naming no object.
fn visible_classes(shared: &SharedVm, thread: &mut JvmThread, data: &[u8]) -> CommandResult {
    use cratonvm_native_api::NativeContext as _;
    let Ok(loader_id) = PayloadReader::new(data).read_u64_be() else {
        return CommandResult::error(ERR_INTERNAL);
    };
    // Wave 25: the primitive array classes, which HotSpot lists for every
    // loader (`visible int[]=true` in `L1W24JdiSurface`) and this VM makes
    // only on demand: made (and known to the session) first, before any
    // object is read, since making one may allocate.
    for desc in PRIMITIVE_ARRAY_CLASSES {
        let _ = known_array_class(shared, desc, None);
    }
    let (loader, classes) = {
        let ds = shared.debug.debug_state.lock();
        let loader = object(shared, &ds, loader_id);
        let classes: Vec<(u64, u8)> = ds
            .known_classes()
            .into_iter()
            .map(|(id, tag, _)| (id, tag))
            .collect();
        (loader, classes)
    };
    let loader = match loader {
        Ok(l) => l,
        Err(code) => return CommandResult::error(code),
    };
    // Interpreter round i1 wave 43 (lane L1): an object that is not a class
    // loader is `INVALID_CLASS_LOADER`, as HotSpot 25.0.3 answers
    // (`L1W43RawJdwpObjectErrorAnswers`); its `parent` field was read and
    // the bootstrap loader's classes answered. No safepoint since the id was
    // resolved.
    if object_tag(shared, &shared.classes.class_manager.read(), loader) != b'l' {
        return CommandResult::error(super::commands::ERR_INVALID_CLASS_LOADER);
    }
    // Interpreter round i6 wave 118, lane L1
    // (`CRATONVM_JDWP_VISIBLE_CLASSES_INITIATED`, default ON since the i6 wave-126 flip, `=0` off; i41-L1): the
    // classes the loader initiated, HotSpot's `GetClassLoaderClasses` list
    // (`instrument::initiated_class_ids`: its defined classes, their direct
    // supertypes, the classes its classes resolved, the primitive arrays),
    // in the session's tags. The loader's namespace is read before anything
    // that may allocate (`loader_namespace_id` reads one field).
    if shared.debug.debugger_gates.visible_classes_initiated() {
        let ids = {
            let mut ctx = crate::vm::NativeContextImpl {
                shared,
                thread: &mut *thread,
            };
            crate::runtime::instrument::initiated_class_ids(&mut ctx, Some(loader))
        };
        let tags: std::collections::HashMap<u64, u8> = classes.iter().copied().collect();
        let mut pw = PayloadWriter::new();
        let visible: Vec<(u8, u64)> = ids
            .iter()
            .filter_map(|id| {
                let raw = u64::from(id.as_u32());
                tags.get(&raw).map(|&tag| (tag, raw))
            })
            .collect();
        // Cast: bounded by the class count.
        pw.put_u32_be(visible.len() as u32);
        for (tag, id) in visible {
            pw.put_u8(tag);
            pw.put_u64_be(super::ids::class_to_wire(id));
        }
        return CommandResult::ok(pw.into_bytes());
    }
    // The loader and its ancestors, pinned: resolving a class's loader may
    // create its mirror, and a collection that allocation starts may move
    // them.
    let mut chain = vec![loader];
    {
        let ctx = crate::vm::NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        let mut current = loader;
        for _ in 0..MAX_LOADER_CHAIN {
            match ctx.get_field_by_name(current, "parent") {
                Value::Object(Some(parent)) => {
                    chain.push(parent);
                    current = parent;
                }
                _ => break,
            }
        }
    }
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.extend(chain);
    let mut visible = Vec::new();
    for (id, tag) in classes {
        let Ok(raw) = u32::try_from(id) else {
            continue;
        };
        let seen = match class_loader_of(shared, thread, ClassId::new(raw)) {
            None => true,
            Some(l) => thread
                .native_pin_roots
                .get(pin_base..)
                .is_some_and(|chain| chain.iter().any(|c| c.as_ptr() == l.as_ptr())),
        };
        if seen {
            visible.push((tag, id));
        }
    }
    thread.native_pin_roots.truncate(pin_base);
    let mut pw = PayloadWriter::new();
    // Cast: bounded by the class count.
    pw.put_u32_be(visible.len() as u32);
    for (tag, id) in visible {
        pw.put_u8(tag);
        pw.put_u64_be(super::ids::class_to_wire(id));
    }
    CommandResult::ok(pw.into_bytes())
}

/// [`visible_classes`] where no mutator can run it (the server quiesced):
/// every class the session knows, the answer before wave 24.
fn visible_classes_quiesced(
    shared: &SharedVm,
    cm: &ClassManager,
    ds: &DebugState,
    r: &mut PayloadReader<'_>,
) -> Reply {
    let loader = object(shared, ds, wire(r.read_u64_be())?)?;
    if object_tag(shared, cm, loader) != b'l' {
        return Err(super::commands::ERR_INVALID_CLASS_LOADER);
    }
    let classes = ds.known_classes();
    let mut pw = PayloadWriter::new();
    // Cast: bounded by the class count.
    pw.put_u32_be(classes.len() as u32);
    for (id, tag, _) in classes {
        pw.put_u8(tag);
        pw.put_u64_be(super::ids::class_to_wire(id));
    }
    Ok(pw.into_bytes())
}

// ---------------------------------------------------------------------------
// `ThreadReference.Interrupt` / `Stop` (interpreter round i1 wave 24)
// ---------------------------------------------------------------------------

/// `ThreadReference.Interrupt` (11/11): what `Thread.interrupt()` does to the
/// thread, as HotSpot's JVMTI `InterruptThread` does it — the VM's interrupt
/// flag set and the thread woken from `sleep`, `wait` or `park`
/// (`NativeContext::thread_interrupt`, the `Thread.interrupt0` native's work),
/// and the Java `interrupted` field set, which the JDK's
/// `Thread.isInterrupted()` reads. On the quiesced server the VM half only
/// ([`tr_interrupt_quiesced`]). `INVALID_THREAD` for an id naming no live thread. It
/// answered `NOT_IMPLEMENTED`.
fn interrupt_thread(shared: &SharedVm, thread: &mut JvmThread, data: &[u8]) -> CommandResult {
    use cratonvm_native_api::NativeContext as _;
    let Ok(tid) = PayloadReader::new(data).read_u64_be() else {
        return CommandResult::error(ERR_INTERNAL);
    };
    let target = {
        let ds = shared.debug.debug_state.lock();
        interrupt_target(shared, &ds, tid)
    };
    let target = match target {
        Ok(t) => t,
        Err(code) => return CommandResult::error(code),
    };
    let mut ctx = crate::vm::NativeContextImpl { shared, thread };
    // `thread_interrupt` writes `Thread`'s own `interrupted` field FIRST,
    // then raises the flag and wakes the target (SBR wave 6). Interpreter
    // round i3 wave 79, lane L6 (page
    // `i77-L6-debugger-interrupt-writes-the-interrupted-field-after-the-flag`):
    // the second, by-name write that followed here, after the wake, raced
    // with a target that had already consumed the interrupt and left the
    // field `true` after its `InterruptedException` (the race the
    // `Thread.interrupt0` bridge removed). HotSpot's `JavaThread::interrupt`
    // writes the field once, before the wake.
    ctx.thread_interrupt(target);
    CommandResult::ok(Vec::new())
}

/// JVMTI `InterruptThread` of the C table (interpreter round i2 wave 63,
/// lane L1): what [`interrupt_thread`] does to `target`, the `Thread` of a
/// live thread, run on the calling (agent) thread `thread`.
pub(crate) fn jvmti_interrupt(shared: &SharedVm, thread: &mut JvmThread, target: ObjectRef) {
    use cratonvm_native_api::NativeContext as _;
    let mut ctx = crate::vm::NativeContextImpl { shared, thread };
    // The field is written once, by `thread_interrupt`, before the wake
    // (wave 79, lane L6: see `interrupt_thread`).
    ctx.thread_interrupt(target);
}

/// JVMTI `StopThread` of the C table (wave 63): `throwable` recorded as live
/// thread `tid`'s pending stop, as `ThreadReference.Stop` records one
/// ([`stop_thread`]), then `target` (its `Thread`) interrupted, so a thread
/// blocked in `sleep`, `wait` or `park` throws it at once. `Err` with a
/// JDWP error code when the throwable cannot be recorded.
pub(crate) fn jvmti_stop(
    shared: &SharedVm,
    thread: &mut JvmThread,
    tid: u64,
    target: ObjectRef,
    throwable: ObjectRef,
) -> Result<(), u16> {
    {
        let cm = shared.classes.class_manager.read();
        if !is_throwable(shared, &cm, throwable) {
            return Err(ERR_INVALID_OBJECT);
        }
        drop(cm);
        let mut ds = shared.debug.debug_state.lock();
        let id = super::export_object(shared, &mut ds, throwable, ObjectExport::Sent);
        if !super::set_pending_stop(shared, &mut ds, tid, id) {
            return Err(ERR_INVALID_OBJECT);
        }
        super::publish_debugger_gates(shared, &ds);
    }
    jvmti_interrupt(shared, thread, target);
    Ok(())
}

/// The `java.lang.Thread` a `ThreadReference.Interrupt` names, or its
/// refusal (`INVALID_THREAD`).
fn interrupt_target(shared: &SharedVm, ds: &DebugState, tid: u64) -> Result<ObjectRef, u16> {
    if let Some(code) = super::commands::live_thread_refusal(ds, tid) {
        return Err(code);
    }
    // `object_for_id` takes a wire id; `tid` is the VM's (wave 25).
    super::object_for_id(shared, ds, ds.wire_thread(tid)).ok_or(ERR_INVALID_THREAD)
}

/// [`interrupt_thread`] on the quiesced server (no mutator): the VM half
/// only — the interrupt flag, and the thread woken from `wait` or `park` —
/// which is what the blocking natives observe.
fn tr_interrupt_quiesced(shared: &SharedVm, ds: &DebugState, r: &mut PayloadReader<'_>) -> Reply {
    let tid = wire(r.read_u64_be())?;
    let target = interrupt_target(shared, ds, tid)?;
    wake_quiesced(shared, tid, target);
    Ok(Vec::new())
}

/// The VM half of an interrupt of thread `tid` (whose `Thread` object is
/// `target`), with no mutator to run `Thread.interrupt0`'s work on: the
/// registry's interrupt flag, and the thread woken from `Object.wait`,
/// `LockSupport.park` or, unmounted, a virtual thread's blocking call. The
/// sleep natives poll the flag.
///
/// nio21 wave 5 L1: the Java `Thread.interrupted` field is raised with the
/// flag, under the target's interrupt lock (`vm_exec::raise_thread_interrupt`,
/// as `NativeContextImpl::thread_interrupt`). It used to raise the flag only,
/// so a quiesced JDWP `ThreadReference.Interrupt` left `isInterrupted()` (the
/// field the JDK's body reads) false while the next `sleep` threw. HotSpot's
/// JVMTI `InterruptThread` raises the field too.
fn wake_quiesced(shared: &SharedVm, tid: u64, target: ObjectRef) {
    let registry = &shared.threads.thread_registry;
    let vm_tid = crate::threading::ThreadId(tid);
    // Interpreter round i3 wave 79, lane L6 (page
    // `i77-L6-debugger-interrupt-writes-the-interrupted-field-after-the-flag`)
    // made the same fix on dev (field, then flag); merged onto the locked
    // form: both records raised together under the target's interrupt lock,
    // then `set_interrupted`'s wake (no second flag store).
    crate::vm::vm_exec::raise_thread_interrupt(shared, Some(vm_tid), target);
    registry.wake_raised_interrupt(vm_tid);
    if let Some(monitor) = registry.peek_jmx_waiting_monitor(vm_tid) {
        let _ = shared
            .threads
            .monitors
            .wake_waiter_for_interrupt(monitor, vm_tid);
    }
    if shared.threads.virtual_thread_manager.is_virtual(tid) {
        shared.threads.virtual_thread_manager.interrupt_virtual(tid);
    }
    // Interpreter round i4 wave 91, lane L6 (page
    // `i77-L6-proposal-every-vm-unpark-through-the-shape`): the unpark
    // `Thread.interrupt` gives (`ParkState::interrupt_unpark` with the VM's
    // shape), not the fixed `notify_one` -- with `CRATONVM_PARK_UNTIMED_NO_POLL`
    // (default on) no 5 ms poll rescues a wake that lands on another waiter.
    if let Some(park) = shared.find_park_state_for_thread_obj(target) {
        park.interrupt_unpark(registry.unpark_shape());
    }
}

/// `ThreadReference.Stop` on a mutator (interpreter round i1 wave 25): the
/// stop is recorded ([`tr_stop`]), then the target is interrupted exactly as
/// [`interrupt_thread`] does it. HotSpot's `JavaThread::install_async_exception`
/// does the same — it sets the thread's `interrupted` field and wakes it —
/// so a thread blocked in `Thread.sleep`, `Object.wait` or `LockSupport.park`
/// throws the stop at once instead of when its blocking call returns on its
/// own. The woken sleep or wait throws the stop itself in place of its
/// `InterruptedException` (`NativeContext::take_debugger_stop`,
/// `exceptions::throw_runtime_error`), so the program's handlers see it
/// whether the frame that catches runs interpreted or compiled; a park
/// returns, and the thread throws it at its next interpreted bytecode; any
/// other exception in flight when the stop is taken is replaced by the
/// unwinder (`interpreter::deliver_exception_event_if_armed`). The sleep and
/// the wait clear the interrupt they were woken by; a park leaves it set, as
/// on HotSpot (`tools/probes/interp/L1/L1W25JdiStopMonitors.java`).
fn stop_thread(shared: &SharedVm, thread: &mut JvmThread, data: &[u8]) -> CommandResult {
    let recorded = {
        let cm = shared.classes.class_manager.read();
        let mut ds = shared.debug.debug_state.lock();
        let recorded = tr_stop(shared, &cm, &mut ds, &mut PayloadReader::new(data));
        drop(cm);
        // Armed before the wake, so the woken thread finds the stop.
        if recorded.is_ok() {
            super::publish_debugger_gates(shared, &ds);
        }
        recorded
    };
    if let Err(code) = recorded {
        return CommandResult::error(code);
    }
    // `data` starts with the thread id, which is all the interrupt reads; a
    // thread that died since answers as it would have (nothing to wake).
    let _ = interrupt_thread(shared, thread, data);
    CommandResult::ok(Vec::new())
}

/// `ThreadReference.Stop` on the quiesced server (wave 25): [`tr_stop`], then
/// the VM half of the interrupt ([`wake_quiesced`]); see [`stop_thread`].
fn tr_stop_quiesced(
    shared: &SharedVm,
    cm: &ClassManager,
    ds: &mut DebugState,
    data: &[u8],
) -> Reply {
    let tid = wire(PayloadReader::new(data).read_u64_be())?;
    let reply = tr_stop(shared, cm, ds, &mut PayloadReader::new(data))?;
    super::publish_debugger_gates(shared, ds);
    if let Ok(target) = interrupt_target(shared, ds, tid) {
        wake_quiesced(shared, tid, target);
    }
    Ok(reply)
}

// ---------------------------------------------------------------------------
// Array classes (interpreter round i1 wave 25, L1c)
// ---------------------------------------------------------------------------

/// The one-dimensional primitive array classes.
const PRIMITIVE_ARRAY_CLASSES: [&str; 8] = ["[Z", "[B", "[C", "[S", "[I", "[J", "[F", "[D"];

/// The JNI name of array `arr`'s class and, for an array of references, its
/// component class. This VM's heap keeps an array's COMPONENT class id
/// (`java.lang.Object`'s for a primitive array) in the array's header, and
/// its element type beside it: the array class is derived from both, as
/// `lang_class::array_descriptor_for` derives it for `getClass()`.
pub(crate) fn array_descriptor(
    shared: &SharedVm,
    cm: &ClassManager,
    arr: ObjectRef,
) -> Option<(String, Option<ClassId>)> {
    let primitive = match shared.mem.heap.array_element_type(arr)? {
        ArrayElementType::Boolean => "[Z",
        ArrayElementType::Byte => "[B",
        ArrayElementType::Char => "[C",
        ArrayElementType::Short => "[S",
        ArrayElementType::Int => "[I",
        ArrayElementType::Long => "[J",
        ArrayElementType::Float => "[F",
        ArrayElementType::Double => "[D",
        ArrayElementType::Reference => "",
    };
    if !primitive.is_empty() {
        return Some((primitive.to_string(), None));
    }
    let component = shared.mem.heap.class_id_of(arr);
    let name = &cm.class_store.get(component)?.name;
    let desc = if name.starts_with('[') {
        format!("[{name}")
    } else {
        format!("[L{name};")
    };
    Some((desc, Some(component)))
}

/// The array class named `desc` (`[I`, `[Ljava/lang/String;`), made if this
/// VM has not made it yet — its heap keeps no array class per array — and
/// known to the session at once (its metadata added as the server's poll
/// would add it), so the `ReferenceType` commands a debugger sends next
/// answer for it. Found first through `component`'s loader. `None` when it
/// cannot be made (a component the bootstrap namespace cannot see). With no
/// lock held; takes the class manager's write lock only to make the class.
/// Make the array classes JDWP array signature `signature` names
/// (`[...L<name>;` for every loaded class `<name>`, by its loader, or a
/// primitive array) and describe them to the session, before
/// `VirtualMachine.ClassesBySignature` answers (interpreter round i5 wave
/// 103 hand-back, lane L1). This VM keeps an object array's component class
/// in its header and makes the array class only when something asks
/// ([`known_array_class`]), where HotSpot makes it at the array's
/// allocation, so `classesByName("Item[]")` found nothing with an `Item[]`
/// alive (`L1W103JdiInstances`, w103b). Made on a parked thread (making a
/// class may allocate); without one, nothing is made and the command answers
/// as before. A component no loader has loaded makes nothing.
///
/// Interpreter round i5 wave 104 (lane L1): only the array classes HotSpot
/// would have: one this VM already made (by any path) is described as it
/// is; a missing one is made only when an array of it exists on the heap
/// ([`arrays_on_the_heap`]: one walk in a non-moving pause, only when a class
/// is missing), and a one-dimensional primitive array class always (HotSpot
/// makes all eight at start-up, `Universe::genesis`). Until then an array
/// class nothing ever allocated was made and listed here.
pub(crate) fn materialize_array_classes(shared: &SharedVm, signature: &str) {
    let element = signature.trim_start_matches('[');
    let components: Vec<ClassId> = match element
        .strip_prefix('L')
        .and_then(|e| e.strip_suffix(';'))
    {
        Some(name) => {
            let cm = shared.classes.class_manager.read();
            cm.class_store
                .iter()
                .filter(|c| &*c.name == name)
                .map(|c| c.id)
                .collect()
        }
        None => Vec::new(),
    };
    let primitive = element.len() == 1 && "ZBCSIJFD".contains(element);
    if components.is_empty() && !primitive {
        return;
    }
    let desc = signature.to_string();
    let _ = super::run_on_parked_thread(shared, None, move |shared, thread| {
        if components.is_empty() {
            // A primitive array: one dimension always exists on HotSpot.
            let made = desc.len() == 2
                || existing_array_class(shared, &desc, None).is_some()
                || arrays_on_the_heap(shared, thread, &desc, &[]).0;
            if made {
                let _ = known_array_class(shared, &desc, None);
            }
            return;
        }
        let missing: Vec<ClassId> = components
            .iter()
            .copied()
            .filter(|&c| existing_array_class(shared, &desc, Some(c)).is_none())
            .collect();
        let allocated = if missing.is_empty() {
            Vec::new()
        } else {
            arrays_on_the_heap(shared, thread, &desc, &missing).1
        };
        for component in components {
            if missing.contains(&component) {
                if !allocated.contains(&component) {
                    continue;
                }
                // In the component's own loader (`known_array_class` alone
                // makes a missing one only as the bootstrap loader, which
                // cannot see an application component).
                let _ = crate::runtime::interpreter::resolve_array_class_loader_aware(
                    shared, thread, component, &desc,
                );
            }
            // Described to the session (found, not made, when it exists).
            let _ = known_array_class(shared, &desc, Some(component));
        }
    });
}

/// The array class named `desc` this VM has already made, found through
/// `component`'s loader (else the bootstrap loader's), as
/// [`known_array_class`] finds it first; `None` when none is made yet
/// (interpreter round i5 wave 104, lane L1).
fn existing_array_class(shared: &SharedVm, desc: &str, component: Option<ClassId>) -> Option<ClassId> {
    let cm = shared.classes.class_manager.read();
    component
        .and_then(|c| cm.find_class_by_name_for_class(desc, c))
        .or_else(|| {
            cm.find_class_by_name_for_loader(desc, crate::classloading::ClassLoaderId::Bootstrap)
        })
}

/// Is an array of JNI class name `desc` on the heap (interpreter round i5
/// wave 104, lane L1)? `.0`: any; `.1`: the members of `components` (element
/// classes) such an array is made of -- its header's component class itself
/// for one dimension, a component array class of the same loader for more.
/// One heap walk on `thread` (a registered mutator) in a non-moving pause
/// (`jvmti::native_env::debugger_heap_scan`).
fn arrays_on_the_heap(
    shared: &SharedVm,
    thread: &JvmThread,
    desc: &str,
    components: &[ClassId],
) -> (bool, Vec<ClassId>) {
    let one_dimension = !desc.starts_with("[[");
    crate::jvmti::native_env::debugger_heap_scan(shared, thread, |objects| {
        let cm = shared.classes.class_manager.read();
        let mut any = false;
        let mut found: Vec<ClassId> = Vec::new();
        for &obj in objects {
            if !is_array(shared, obj) {
                continue;
            }
            let Some((name, header_component)) = array_descriptor(shared, &cm, obj) else {
                continue;
            };
            if name != desc {
                continue;
            }
            any = true;
            let Some(header_component) = header_component else {
                continue;
            };
            let header_loader = cm.class_store.get(header_component).map(|c| c.loader_id);
            for &component in components {
                let matches = if one_dimension {
                    header_component == component
                } else {
                    header_loader.is_some()
                        && cm.class_store.get(component).map(|c| c.loader_id) == header_loader
                };
                if matches && !found.contains(&component) {
                    found.push(component);
                }
            }
            if found.len() == components.len() {
                break;
            }
        }
        (any, found)
    })
}

fn known_array_class(shared: &SharedVm, desc: &str, component: Option<ClassId>) -> Option<ClassId> {
    let found = {
        let cm = shared.classes.class_manager.read();
        component
            .and_then(|c| cm.find_class_by_name_for_class(desc, c))
            .or_else(|| {
                cm.find_class_by_name_for_loader(
                    desc,
                    crate::classloading::ClassLoaderId::Bootstrap,
                )
            })
    };
    let id = match found {
        Some(id) => id,
        None => shared.classes.class_manager_write().load_class(desc).ok()?,
    };
    let cm = shared.classes.class_manager.read();
    let class = cm.class_store.get(id)?;
    let mut ds = shared.debug.debug_state.lock();
    if !ds.class_signatures.contains_key(&u64::from(id.as_u32())) {
        let _ = super::add_class_metadata(&mut ds, class);
    }
    Some(id)
}

/// `ObjectReference.ReferenceType` (9/1) of an array, on a mutator (wave 25):
/// its array class (`refTypeTag` ARRAY), as HotSpot answers. The quiesced
/// path ([`or_reference_type`]) answered the class the heap keeps in the
/// array's header — its component — so JDI named an `int[]`
/// `java.lang.Object` and a `String[]` `java.lang.String`
/// (`L1W23JdiConformance`: `this.squares = java.lang.Obje[4]`). `None` for
/// an object that is not an array, or an array class that cannot be made:
/// the caller answers as before.
fn array_reference_type(shared: &SharedVm, data: &[u8]) -> Option<CommandResult> {
    let id = PayloadReader::new(data).read_u64_be().ok()?;
    let (desc, component) = {
        let cm = shared.classes.class_manager.read();
        let ds = shared.debug.debug_state.lock();
        let arr = super::object_for_id(shared, &ds, id)?;
        if !is_array(shared, arr) {
            return None;
        }
        array_descriptor(shared, &cm, arr)?
    };
    let class_id = known_array_class(shared, &desc, component)?;
    let mut pw = PayloadWriter::new();
    pw.put_u8(3); // ARRAY
    pw.put_u64_be(super::ids::class_to_wire(u64::from(class_id.as_u32())));
    Some(CommandResult::ok(pw.into_bytes()))
}

// ---------------------------------------------------------------------------
// Modules (JDWP 9; interpreter round i1 wave 25)
// ---------------------------------------------------------------------------

/// The module of class `class_id` as the program sees it: what
/// `Class.getModule()` answers for the class's mirror (a registered native
/// in this VM, which makes the canonical `Module` of the class's module;
/// an array class's is its element type's). `None` when the mirror cannot be
/// made or the call does not answer a module. On a mutator with no lock
/// held: the mirror and the module may be created here.
fn module_of_class(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
) -> Option<ObjectRef> {
    use cratonvm_native_api::NativeContext as _;
    let mirror = crate::vm::try_get_or_create_class_mirror(shared, class_id)?;
    let mut ctx = crate::vm::NativeContextImpl {
        shared,
        thread: &mut *thread,
    };
    match ctx.invoke_virtual(mirror, "getModule", "()Ljava/lang/Module;", &[]) {
        Ok(Some(Value::Object(module))) => module,
        _ => None,
    }
}

/// The module commands ([`is_module_command`]) on a mutator, as HotSpot's
/// back end answers them from JVMTI (`GetModule`... `GetAllModules`):
///
/// * `ReferenceType.Module` (2/19): the class's module ([`module_of_class`]);
///   `INVALID_CLASS` for an id naming no class;
/// * `ModuleReference.Name` (18/1): the module's `name`, the empty string for
///   an unnamed module (HotSpot's answer); `ClassLoader` (18/2): its
///   `loader`, `null` for the bootstrap loader's. `INVALID_MODULE` for an
///   object that is not a `java.lang.Module` and (interpreter round i1 wave
///   43, as HotSpot 25.0.3 answers, `L1W43RawJdwpObjectErrorAnswers`) for an
///   id naming nothing, which was `INVALID_OBJECT`;
/// * `VirtualMachine.AllModules` (1/22): the modules this VM has made —
///   its canonical `Module` per module name (`ClassRealm::module_mirrors`,
///   which `Class.getModule()` and the module system's definitions fill),
///   `java.base`'s always among them (made here through `Object`'s mirror if
///   no class asked yet). HotSpot lists every module defined to any loader,
///   a module no class of which was ever asked for included; this VM makes
///   a module's `Module` lazily, so such a module is not listed.
///
/// No lock is held across the calls that may run Java or allocate; the
/// answers are exported with no safepoint in between.
fn module_command(
    shared: &SharedVm,
    thread: &mut JvmThread,
    command_set: u8,
    command: u8,
    data: &[u8],
) -> CommandResult {
    use super::commands::{
        CMD_MR_CLASS_LOADER, CMD_RT_MODULE, CMD_VM_ALL_MODULES, ERR_INVALID_MODULE,
    };
    use cratonvm_native_api::NativeContext as _;
    let mut r = PayloadReader::new(data);
    let mut pw = PayloadWriter::new();
    match (command_set, command) {
        (CS_REF_TYPE, CMD_RT_MODULE) => {
            let Ok(raw) = r.read_u64_be() else {
                return CommandResult::error(ERR_INTERNAL);
            };
            let Ok(raw) = u32::try_from(raw) else {
                return CommandResult::error(ERR_INVALID_CLASS);
            };
            let class_id = ClassId::new(raw);
            let known = shared
                .classes
                .class_manager
                .read()
                .class_store
                .get(class_id)
                .is_some();
            if !known {
                return CommandResult::error(ERR_INVALID_CLASS);
            }
            let Some(module) = module_of_class(shared, thread, class_id) else {
                return CommandResult::error(ERR_INTERNAL);
            };
            let mut ds = shared.debug.debug_state.lock();
            pw.put_u64_be(super::export_object(
                shared,
                &mut ds,
                module,
                ObjectExport::Sent,
            ));
        }
        (CS_VM, CMD_VM_ALL_MODULES) => {
            let object = shared
                .classes
                .class_manager
                .read()
                .find_class_by_name_for_loader(
                    "java/lang/Object",
                    crate::classloading::ClassLoaderId::Bootstrap,
                );
            if let Some(object) = object {
                let _ = module_of_class(shared, thread, object);
            }
            let mut modules: Vec<ObjectRef> = Vec::new();
            for &m in shared.classes.module_mirrors.read().values() {
                if !modules.contains(&m) {
                    modules.push(m);
                }
            }
            let mut ds = shared.debug.debug_state.lock();
            // Cast: bounded by the module count.
            pw.put_u32_be(modules.len() as u32);
            for m in modules {
                pw.put_u64_be(super::export_object(shared, &mut ds, m, ObjectExport::Sent));
            }
        }
        _ => {
            // `ModuleReference.Name` / `ClassLoader`.
            let Ok(id) = r.read_u64_be() else {
                return CommandResult::error(ERR_INTERNAL);
            };
            let module = {
                let ds = shared.debug.debug_state.lock();
                super::object_for_id(shared, &ds, id)
            };
            let Some(module) = module else {
                return CommandResult::error(ERR_INVALID_MODULE);
            };
            let is_module = {
                let cm = shared.classes.class_manager.read();
                let class = shared.mem.heap.class_id_of(module);
                cm.class_store
                    .get(class)
                    .is_some_and(|c| &*c.name == "java/lang/Module")
            };
            if !is_module {
                return CommandResult::error(ERR_INVALID_MODULE);
            }
            let ctx = crate::vm::NativeContextImpl {
                shared,
                thread: &mut *thread,
            };
            if command == CMD_MR_CLASS_LOADER {
                let loader = match ctx.get_field_by_name(module, "loader") {
                    Value::Object(loader) => loader,
                    _ => None,
                };
                let mut ds = shared.debug.debug_state.lock();
                let id = loader.map_or(0, |l| {
                    super::export_object(shared, &mut ds, l, ObjectExport::Sent)
                });
                pw.put_u64_be(id);
            } else {
                let name = match ctx.get_field_by_name(module, "name") {
                    Value::Object(Some(s)) => ctx.read_string(s),
                    _ => None,
                };
                pw.put_string(name.as_deref().unwrap_or(""));
            }
        }
    }
    CommandResult::ok(pw.into_bytes())
}

// ---------------------------------------------------------------------------
// Owned and contended monitors (interpreter round i1 wave 25)
// ---------------------------------------------------------------------------

/// A monitor command ([`is_monitor_command`]) on the thread it names, when
/// that thread is parked at a suspend point: its live frames' monitor
/// records (`Frame::held_monitors`, `Frame::monitor_on_exit`, interpreter
/// round i1 wave 23) attribute each monitor to its frame. `None` when the
/// thread is not parked (blocked in native code, or busy): the caller serves
/// the command on any mutator, or quiesced, from the registry
/// ([`monitor_reply`] without frames). A thread the session does not list,
/// or does not hold suspended, is refused here, as HotSpot's back end
/// refuses it (`INVALID_THREAD`, `THREAD_NOT_SUSPENDED`).
fn monitor_command_on_its_thread(
    shared: &SharedVm,
    command: u8,
    data: &[u8],
) -> Option<CommandResult> {
    let Ok(tid) = PayloadReader::new(data).read_u64_be() else {
        return Some(CommandResult::error(ERR_INTERNAL));
    };
    {
        let ds = shared.debug.debug_state.lock();
        if let Some(code) = super::commands::frames_refusal(&ds, tid) {
            return Some(CommandResult::error(code));
        }
    }
    // Interpreter round i5 wave 110, lane L1 (the `L1W25JdiStopMonitors`
    // intermittent, 2 in 16 on w107): HotSpot answers these commands at a
    // handshake, where the thread's monitor state is consistent. Here a
    // suspended thread is read either on itself at its suspend point or
    // through its inspection window; one caught between the two (leaving a
    // region on its way to its suspend point, a window another reader holds
    // for a moment) fell to the registry answer at once, whose frame
    // attribution matches no listing (every monitor at -1, lock-stack order).
    // Now such a thread is waited for, a bounded while
    // ([`super::SUSPEND_POINT_WAIT`]), while it is still on its way
    // ([`monitor_target_on_its_way`]); the registry answers only after that.
    let deadline = std::time::Instant::now() + super::SUSPEND_POINT_WAIT;
    loop {
        match monitor_command_on_parked_target(shared, command, tid) {
            Ok(answer) => return Some(answer),
            Err(super::ParkedError::Busy) => {
                return monitor_command_through_window(shared, command, tid);
            }
            Err(super::ParkedError::NotParked) => {
                if let Some(answer) = monitor_command_through_window(shared, command, tid) {
                    return Some(answer);
                }
                if std::time::Instant::now() >= deadline
                    || !monitor_target_on_its_way(shared, tid)
                {
                    return None;
                }
                super::await_debug_state_poll(shared);
            }
            Err(super::ParkedError::Lost) => return Some(CommandResult::error(ERR_INTERNAL)),
        }
    }
}

/// [`monitor_command_on_its_thread`]'s read on the target itself, parked at
/// its suspend point: its live frames' monitor records attribute each
/// monitor to its frame (interpreter round i5 wave 110, lane L1: split out
/// so the bounded wait can ask again).
fn monitor_command_on_parked_target(
    shared: &SharedVm,
    command: u8,
    tid: u64,
) -> Result<CommandResult, super::ParkedError> {
    super::run_on_parked_thread(shared, Some(tid), move |shared, thread| {
        let frames: &[crate::runtime::frame::Frame] = &thread.frames;
        let pairs = crate::runtime::interpreter::held_monitors::frame_locked_monitors(frames);
        // Interpreter round i5 wave 102, lane L1: each frame's method id,
        // bottom first, so a listing that also shows rows this thread has no
        // interpreter frame for can still be aligned ([`align_frames_to_listing`]).
        let method_ids: Vec<u64> = frames.iter().map(super::frame_method_id).collect();
        let own = MonitorFrames::Own(method_ids, pairs);
        let cm = shared.classes.class_manager.read();
        let mut ds = shared.debug.debug_state.lock();
        match monitor_reply(shared, &cm, &mut ds, command, tid, Some(own)) {
            Ok(bytes) => CommandResult::ok(bytes),
            Err(code) => CommandResult::error(code),
        }
    })
}

/// Is suspended thread `tid` still on its way to a place a monitor command
/// can read it consistently -- its suspend point, or its inspection window
/// (interpreter round i5 wave 110, lane L1)? A live thread the debugger
/// holds suspended that is blocked in a region (its window opening, or held
/// by another reader for a moment) or still running toward its suspend
/// point. Not a dead one, an unmounted virtual thread (its frames sit in the
/// virtual-thread manager, never in a window), nor one in a native outside
/// a region that acknowledged the suspension there (it parks only at the
/// native's return): those are answered from the registry at once, as
/// before.
fn monitor_target_on_its_way(shared: &SharedVm, tid: u64) -> bool {
    let id = crate::threading::ThreadId(tid);
    let registry = &shared.threads.thread_registry;
    if !registry.is_alive(id)
        || shared.threads.virtual_thread_manager.is_deposited(tid)
        || super::in_native_acknowledged(registry, id)
    {
        return false;
    }
    // A thread that parked since the last try is read on itself next time.
    shared.debug.debug_state.lock().is_thread_suspended(tid)
}

/// Where a monitor command's per-frame attribution comes from
/// ([`monitor_reply`]; interpreter round i5 wave 102, lane L1).
enum MonitorFrames {
    /// The thread's own frames, read on it at its suspend point: each
    /// frame's JDWP method id (bottom first) and `frame_locked_monitors`.
    Own(Vec<u64>, Vec<(usize, ObjectRef)>),
    /// A thread blocked in a native region, read through its inspection
    /// window (`interpreter::with_blocked_frames`): each listing row's
    /// interpreter frame index (top first, `None` for a native head or a
    /// compiled row), the number of its interpreter frames, and the
    /// attribution its frames' own records give (frame indexes from the
    /// bottom; addresses as of its block, so only a fallback).
    ///
    /// Interpreter round i5 wave 110, lane L1: the attribution itself is
    /// resolved in [`monitor_reply`], with no collection able to run, against
    /// the lock stack of that same instant
    /// (`ThreadRegistry::jmx_frame_monitors(tid, frame count)`: slots, not
    /// addresses). It was resolved when the window was read, with no lock
    /// held and the server not a mutator, so a collection between that read
    /// and the reply left every pair naming a pre-move address that no owned
    /// (remapped) monitor equals: all at -1, in lock-stack order -- the
    /// `L1W25JdiStopMonitors` `owner` rows of the intermittent failures.
    Window(Vec<Option<usize>>, usize, Vec<(usize, ObjectRef)>),
}

/// A monitor command on thread `tid` blocked in a native region
/// (interpreter round i5 wave 102 hand-back, w102c: `L1W25JdiStopMonitors`
/// still listed every owned monitor at -1 in lock-stack order -- the owner
/// sleeps, so the command was served from the registry, whose publication
/// is indexed by a frame count that no longer matched the listing). As the
/// C JVMTI table's `GetOwnedMonitorStackDepthInfo` already reads such a
/// thread (`jvmti::native_env::monitors_through_window`): through its
/// inspection window, each listing row naming the interpreter frame it is,
/// so a monitor's depth is the row of the frame that locked it. The window
/// is read with no lock held; the reply is made quiesced, as
/// [`run_quiesced`] makes it. A monitor whose frame record a collection
/// moved since the read no longer equals its owned (current) address and
/// falls to -1 (equality only: no stale address is dereferenced or sent).
/// `None` (the registry answer, as before) when the window is not open.
fn monitor_command_through_window(
    shared: &SharedVm,
    command: u8,
    tid: u64,
) -> Option<CommandResult> {
    // Wave 110 (lane L1): the rows and the frame count only; the published
    // attribution is resolved inside the quiesced reply (see
    // `MonitorFrames::Window`). The frames' own records are kept as the
    // fallback for a thread that published none.
    let window = crate::runtime::interpreter::with_blocked_frames(shared, tid, |held| {
        let frames = held.frames();
        let rows: Vec<Option<usize>> = held.rows.iter().map(|row| row.frame).collect();
        let fallback = crate::runtime::interpreter::held_monitors::frame_locked_monitors(frames);
        (rows, frames.len(), fallback)
    })?;
    loop {
        {
            let cm = shared.classes.class_manager.read();
            let mut out = None;
            let ran = shared.mem.gc_barrier.run_if_no_stw_requested(|| {
                let mut ds = shared.debug.debug_state.lock();
                let frames = MonitorFrames::Window(window.0.clone(), window.1, window.2.clone());
                out = Some(match monitor_reply(shared, &cm, &mut ds, command, tid, Some(frames)) {
                    Ok(bytes) => CommandResult::ok(bytes),
                    Err(code) => CommandResult::error(code),
                });
            });
            if let (true, Some(result)) = (ran, out) {
                return Some(result);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// The reply to a monitor command for thread `tid`, with no collection able
/// to run (a mutator between polls, or the server quiesced), as HotSpot's
/// JVMTI answers `GetOwnedMonitorInfo`, `GetOwnedMonitorStackDepthInfo` and
/// `GetCurrentContendedMonitor`:
///
/// * the owned monitors are the thread's lock stack (the registry's record,
///   which the interpreter, the helpers and compiled code's inline
///   `monitorenter` all publish to), less the monitor it waits on in
///   `Object.wait` and the one it is blocked entering (neither is owned);
/// * listed by frame, innermost first, and within a frame in the order the
///   frame entered them — a synchronized method's own monitor, then its
///   blocks' outermost first: HotSpot walks `javaVFrame::monitors()`
///   forwards (`JvmtiEnvBase::get_locked_objects_in_frame`), unlike
///   `ThreadInfo.getLockedMonitors()`, which lists a frame's newest first; an
///   object entered again deeper is listed once, at its innermost frame;
/// * a monitor's stack depth is the frame's place in the thread's JDWP frame
///   listing (`ThreadReference.Frames`), which also counts native methods
///   and compiled activations; -1 (JVMTI's "not attributed to a frame": a
///   monitor entered through JNI) for one no interpreter frame records —
///   compiled code's, or all of them when no attribution matches the
///   listing;
/// * the contended monitor is the one the thread is blocked entering, never
///   the one it waits on (JDK 23+, JDK-8256314).
///
/// `own` is `(interpreter frame count, frame_locked_monitors of the frames)`
/// read from the thread's live frames; `None` takes the attribution the
/// thread published when it blocked (`ThreadRegistry::jmx_frame_monitors`,
/// interpreter round i1 wave 24, lane L7), used only when it describes as
/// many interpreter frames as the listing shows.
fn monitor_reply(
    shared: &SharedVm,
    cm: &ClassManager,
    ds: &mut DebugState,
    command: u8,
    tid: u64,
    own: Option<MonitorFrames>,
) -> Reply {
    use super::commands::{CMD_TR_CURRENT_CONTENDED_MONITOR, CMD_TR_OWNED_MONITORS};
    if let Some(code) = super::commands::frames_refusal(ds, tid) {
        return Err(code);
    }
    let registry = &shared.threads.thread_registry;
    let vm_tid = crate::threading::ThreadId(tid);
    let (contended, waiting, locked, _, _, _, _, _) = registry
        .jmx_lock_snapshot(vm_tid)
        .ok_or(ERR_INVALID_THREAD)?;
    // Interpreter round i2 wave 55, lane L6 (`L6W55JdiSuspendedWaiterReentryTwice`):
    // a notified waiter re-entering its monitor -- BLOCKED while its wait's
    // monitor still stands, a contended re-acquisition or a suspended
    // waiter's give-back -- contends for that monitor, as HotSpot's
    // `GetCurrentContendedMonitor` answers it (the waiting monitor of a
    // thread `BLOCKED_ON_MONITOR_ENTER`, JDK-8256314). This answered null.
    let contended =
        contended.or_else(|| waiting.filter(|_| registry.java_block_state(vm_tid) == 2));
    let mut pw = PayloadWriter::new();
    if command == CMD_TR_CURRENT_CONTENDED_MONITOR {
        let tag = contended.map(|o| object_tag(shared, cm, o));
        put_tagged_with_tag(shared, ds, &mut pw, b'L', Value::Object(contended), tag);
        return Ok(pw.into_bytes());
    }
    let owned: Vec<ObjectRef> = locked
        .into_iter()
        .filter(|&o| Some(o) != waiting && Some(o) != contended)
        .collect();
    // The listing's interpreter frames, top first: their JDWP depths (and
    // method ids, wave 102).
    let listing_len = ds.thread_frames.get(&tid).map_or(0, Vec::len);
    let interpreted_rows: Vec<(usize, u64)> =
        ds.thread_frames.get(&tid).map_or_else(Vec::new, |frames| {
            frames
                .iter()
                .enumerate()
                .filter(|(_, e)| {
                    e.offset != super::NATIVE_FRAME_LOCATION
                        && ds.opaque_frames.get(&(tid, e.frame_id)) != Some(&true)
                        // Wave 138 (L1): a lambda proxy row is no frame of the thread's.
                        && !ds.proxy_frames.contains(&(tid, e.frame_id))
                })
                .map(|(depth, e)| (depth, e.method_id))
                .collect()
        });
    let interpreted: Vec<usize> = interpreted_rows.iter().map(|&(depth, _)| depth).collect();
    // Interpreter round i5 wave 102, lane L1 (`L1W25JdiStopMonitors` on
    // w101b: every owned monitor at -1, in lock-stack order): the
    // attribution was dropped whole whenever the listing showed a row the
    // thread has no interpreter frame for (a served JDK frame of a stand-in
    // above the thread's own frames), since frame indexes were only ever
    // mapped by position. Now: by position when the counts agree (as
    // before); else, with the thread's own frames, by aligning their method
    // ids to the listing's from the bottom; else, from the registry, the
    // attribution indexed into the whole published trace when that trace is
    // as long as the listing.
    let (depth_map, pairs): (Vec<Option<i32>>, Option<Vec<(usize, ObjectRef)>>) = match own {
        Some(MonitorFrames::Own(method_ids, pairs)) => {
            let map = if method_ids.len() == interpreted.len() {
                position_depths(method_ids.len(), &interpreted)
            } else {
                align_frames_to_listing(&method_ids, &interpreted_rows)
            };
            (map, Some(pairs))
        }
        // Wave 102 hand-back: the window's rows name their frames.
        // Wave 110: the publication resolved against the lock stack read
        // above, at the same instant (no collection runs during the reply).
        Some(MonitorFrames::Window(rows, frame_count, fallback)) => {
            let pairs = registry
                .jmx_frame_monitors(vm_tid, frame_count)
                .unwrap_or(fallback);
            (window_depths(&rows), Some(pairs))
        }
        None => {
            let count = interpreted.len();
            match registry.jmx_frame_monitors(vm_tid, count) {
                Some(pairs) => (position_depths(count, &interpreted), Some(pairs)),
                None if listing_len != count => {
                    let all: Vec<usize> = (0..listing_len).collect();
                    (
                        position_depths(listing_len, &all),
                        registry.jmx_frame_monitors(vm_tid, listing_len),
                    )
                }
                None => (Vec::new(), None),
            }
        }
    };
    let monitors = jvmti_owned_monitor_order(&owned, pairs.unwrap_or_default(), |index| {
        // `index` counts from the bottom.
        depth_map.get(index).copied().flatten()
    });
    let depths = command != CMD_TR_OWNED_MONITORS;
    // Cast: bounded by the thread's lock stack.
    pw.put_u32_be(monitors.len() as u32);
    for (obj, depth) in monitors {
        let tag = Some(object_tag(shared, cm, obj));
        put_tagged_with_tag(shared, ds, &mut pw, b'L', Value::Object(Some(obj)), tag);
        if depths {
            // Cast: a JDWP int; -1 is "not attributed".
            pw.put_u32_be(depth as u32);
        }
    }
    Ok(pw.into_bytes())
}

/// Frame index (from the bottom) -> JDWP depth, for `count` frames that are
/// the listing rows `depths_top_first` one for one (interpreter round i5
/// wave 102, lane L1: the positional mapping `monitor_reply` always made).
/// Empty (every monitor at -1) when the counts differ.
fn position_depths(count: usize, depths_top_first: &[usize]) -> Vec<Option<i32>> {
    if count != depths_top_first.len() {
        return Vec::new();
    }
    (0..count)
        .map(|index| {
            depths_top_first
                .get(count - 1 - index)
                .and_then(|&depth| i32::try_from(depth).ok())
        })
        .collect()
}

/// Frame index (from the bottom) -> JDWP depth from a window listing whose
/// rows (top first) name their interpreter frame (interpreter round i5 wave
/// 102 hand-back): a frame's depth is the position of its row; a frame no
/// row names maps to `None`.
fn window_depths(rows: &[Option<usize>]) -> Vec<Option<i32>> {
    let count = rows.iter().flatten().map(|&i| i + 1).max().unwrap_or(0);
    (0..count)
        .map(|index| {
            rows.iter()
                .position(|&row| row == Some(index))
                .and_then(|depth| i32::try_from(depth).ok())
        })
        .collect()
}

/// Frame index (from the bottom) -> JDWP depth when the listing shows rows
/// the thread has no interpreter frame for (interpreter round i5 wave 102,
/// lane L1): `method_ids` are the thread's frames' method ids, bottom first;
/// `rows` the listing's interpreted rows, top first, as `(depth, method id)`.
/// Matched from the bottom (where the two agree; extra rows sit at the top,
/// a stand-in's served JDK frames): each frame takes the next row up with
/// its method id; a frame no row matches maps to `None` (its monitors at -1)
/// and consumes nothing.
fn align_frames_to_listing(method_ids: &[u64], rows: &[(usize, u64)]) -> Vec<Option<i32>> {
    let mut out = Vec::with_capacity(method_ids.len());
    let mut next = rows.len();
    for &id in method_ids {
        match rows[..next].iter().rposition(|&(_, row_id)| row_id == id) {
            Some(at) => {
                out.push(i32::try_from(rows[at].0).ok());
                next = at;
            }
            None => out.push(None),
        }
    }
    out
}

/// The owned monitors `owned` in JVMTI `GetOwnedMonitorInfo`'s order, each
/// with its stack depth ([`monitor_reply`]): `pairs` is
/// `held_monitors::frame_locked_monitors` of the frames — frames innermost
/// first, and within a frame its newest block first and its synchronized
/// method's monitor last, which is reversed here to the frame's entry order
/// — and `depth_of` maps a frame index (from the bottom) to its JDWP depth.
/// A frame entry `owned` does not hold is dropped; an object already listed
/// (entered again by an outer frame) is not listed again; an owned monitor
/// no entry names follows at depth -1.
pub(crate) fn jvmti_owned_monitor_order(
    owned: &[ObjectRef],
    pairs: Vec<(usize, ObjectRef)>,
    depth_of: impl Fn(usize) -> Option<i32>,
) -> Vec<(ObjectRef, i32)> {
    let mut out: Vec<(ObjectRef, i32)> = Vec::with_capacity(owned.len());
    let mut rest = pairs.as_slice();
    while let Some(&(index, _)) = rest.first() {
        let run = rest.iter().take_while(|&&(i, _)| i == index).count();
        let (frame, tail) = rest.split_at(run);
        rest = tail;
        for &(_, obj) in frame.iter().rev() {
            if !owned.contains(&obj) || out.iter().any(|&(o, _)| o == obj) {
                continue;
            }
            out.push((obj, depth_of(index).unwrap_or(-1)));
        }
    }
    for &obj in owned {
        if !out.iter().any(|&(o, _)| o == obj) {
            out.push((obj, -1));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Thread groups (interpreter round i1 wave 24)
// ---------------------------------------------------------------------------

/// The deepest `ThreadGroup.parent` chain the walks below follow.
const MAX_GROUP_CHAIN: usize = 64;

/// The thread-group commands on a mutator (interpreter round i1 wave 24),
/// answered from the program's own `ThreadGroup` objects as HotSpot's back
/// end answers them through JVMTI (`GetTopThreadGroups`, `GetThreadInfo`,
/// `GetThreadGroupInfo`, `GetThreadGroupChildren`): a thread's group is its
/// `Thread.holder.group`, a group's id is its object id (so the group is an
/// `ObjectReference` too, and a group value is tagged `g`), its name and
/// parent are its fields, and its children are the live threads whose group
/// it is and its subgroups (`ThreadGroup.groups`, then the weakly held
/// `weaks`). The server modelled one `system` group holding every thread,
/// so jdb's `threads` printed no `main` group and a program's own groups
/// were invisible. Without real groups — a `Thread` class with neither
/// `holder` nor `group` — the one-group model answers
/// (`commands::dispatch`).
///
/// The heap is read with no lock held (a field read by name may take the
/// class manager, which is ordered before the debug state), and no
/// allocation happens between resolving the ids and exporting the answer.
fn thread_group_command(
    shared: &SharedVm,
    thread: &mut JvmThread,
    command_set: u8,
    command: u8,
    data: &[u8],
) -> CommandResult {
    let ctx = crate::vm::NativeContextImpl { shared, thread };
    let answer = match (command_set, command) {
        (CS_VM, CMD_VM_TOP_LEVEL_THREAD_GROUPS) => top_level_thread_groups(shared, &ctx),
        (CS_THREAD_REF, CMD_TR_THREAD_GROUP) => thread_group_of_thread(shared, &ctx, data),
        (CS_THREAD_GROUP_REF, _) => thread_group_reference(shared, &ctx, command, data),
        _ => None,
    };
    answer.unwrap_or_else(|| {
        let mut ds = shared.debug.debug_state.lock();
        super::commands::dispatch(command_set, command, data, &mut ds)
    })
}

/// The live threads the server lists, with their `java.lang.Thread`
/// objects (read under the debug-state lock, then the registry's map).
fn listed_thread_objects(shared: &SharedVm) -> Vec<(u64, ObjectRef)> {
    let ds = shared.debug.debug_state.lock();
    super::commands::known_thread_ids(&ds)
        .into_iter()
        .filter_map(|tid| {
            let wire = ds.wire_thread(tid);
            Some((tid, super::object_for_id(shared, &ds, wire)?))
        })
        .collect()
}

/// A thread's group: `Thread.holder.group` (JDK 19+), else a `group` field
/// on the thread itself (older layouts); `None` for a `Thread` with neither.
///
/// Round 15 wave 4 (lane threadvm): `holder` and `group` are the fields
/// `java.lang.Thread` declares (`vm_exec::thread_field_owner`); by name a
/// subclass's own `holder` / `group` field was taken for the thread's group.
/// `Thread$FieldHolder` is final, so its `group` is read by name.
pub(crate) fn group_of_thread(
    ctx: &crate::vm::NativeContextImpl<'_>,
    thread_obj: ObjectRef,
) -> Option<ObjectRef> {
    use cratonvm_native_api::NativeContext as _;
    let (holder_slot, group_slot) = {
        let shared = ctx.shared;
        let cm = shared.classes.class_manager.read();
        let class_id = shared.mem.heap.class_id_of(thread_obj);
        let owner = crate::vm::vm_exec::thread_field_owner(shared, &cm.class_store, class_id);
        let slot = |name: &str| {
            crate::vm::vm_exec::resolve_field_index_in_hierarchy(owner, name, &cm.class_store)
        };
        (slot("holder"), slot("group"))
    };
    if let Some(Value::Object(Some(holder))) = holder_slot.map(|s| ctx.get_field(thread_obj, s)) {
        return match ctx.get_field_by_name(holder, "group") {
            Value::Object(group) => group,
            _ => None,
        };
    }
    match group_slot.map(|s| ctx.get_field(thread_obj, s)) {
        Some(Value::Object(group)) => group,
        _ => None,
    }
}

/// A group's parent (`ThreadGroup.parent`); `None` for the root.
pub(crate) fn group_parent(
    ctx: &crate::vm::NativeContextImpl<'_>,
    group: ObjectRef,
) -> Option<ObjectRef> {
    use cratonvm_native_api::NativeContext as _;
    match ctx.get_field_by_name(group, "parent") {
        Value::Object(parent) => parent,
        _ => None,
    }
}

/// The root of `group`'s `ThreadGroup.parent` chain (at most
/// [`MAX_GROUP_CHAIN`] steps up). Also the C JVMTI table's
/// `GetTopThreadGroups` (interpreter round i1 wave 52, lane L1).
pub(crate) fn group_root(
    ctx: &crate::vm::NativeContextImpl<'_>,
    mut group: ObjectRef,
) -> ObjectRef {
    for _ in 0..MAX_GROUP_CHAIN {
        match group_parent(ctx, group) {
            Some(parent) => group = parent,
            None => break,
        }
    }
    group
}

/// A group's subgroups, as `ThreadGroup.subgroups()` lists them: the
/// strongly held `groups[0..ngroups]`, then the live referents of
/// `weaks[0..nweaks]`. Also the C JVMTI table's `GetThreadGroupChildren`
/// (interpreter round i2 wave 56, lane L1).
pub(crate) fn group_subgroups(
    shared: &SharedVm,
    ctx: &crate::vm::NativeContextImpl<'_>,
    group: ObjectRef,
) -> Vec<ObjectRef> {
    use cratonvm_native_api::NativeContext as _;
    let count = |field: &str| match ctx.get_field_by_name(group, field) {
        Value::Int(n) => usize::try_from(n).unwrap_or(0),
        _ => 0,
    };
    let elements = |field: &str, n: usize| -> Vec<ObjectRef> {
        let Value::Object(Some(array)) = ctx.get_field_by_name(group, field) else {
            return Vec::new();
        };
        if !is_array(shared, array) {
            return Vec::new();
        }
        let n = n.min(shared.mem.heap.array_length(array));
        (0..n)
            .filter_map(|i| match shared.mem.heap.get_array_element(array, i) {
                Ok(Value::Object(Some(o))) => Some(o),
                _ => None,
            })
            .collect()
    };
    let mut subgroups = elements("groups", count("ngroups"));
    for weak in elements("weaks", count("nweaks")) {
        if let Value::Object(Some(referent)) = ctx.get_field_by_name(weak, "referent") {
            subgroups.push(referent);
        }
    }
    subgroups
}

/// `VirtualMachine.TopLevelThreadGroups` (1/5): the roots of the listed
/// threads' groups (`system`). `None` when no thread has a group.
fn top_level_thread_groups(
    shared: &SharedVm,
    ctx: &crate::vm::NativeContextImpl<'_>,
) -> Option<CommandResult> {
    let mut roots: Vec<ObjectRef> = Vec::new();
    for (_, thread_obj) in listed_thread_objects(shared) {
        let Some(group) = group_of_thread(ctx, thread_obj) else {
            continue;
        };
        let group = group_root(ctx, group);
        if !roots.iter().any(|r| r.as_ptr() == group.as_ptr()) {
            roots.push(group);
        }
    }
    if roots.is_empty() {
        return None;
    }
    let mut ds = shared.debug.debug_state.lock();
    let mut pw = PayloadWriter::new();
    // Cast: a handful of roots.
    pw.put_u32_be(roots.len() as u32);
    for root in roots {
        pw.put_u64_be(super::export_object(
            shared,
            &mut ds,
            root,
            ObjectExport::Sent,
        ));
    }
    Some(CommandResult::ok(pw.into_bytes()))
}

/// `ThreadReference.ThreadGroup` (11/5): the thread's group object. `None`
/// (the one-group model: `INVALID_THREAD`, a dead thread's null group) for
/// an id naming no live thread, and for a thread without a group.
fn thread_group_of_thread(
    shared: &SharedVm,
    ctx: &crate::vm::NativeContextImpl<'_>,
    data: &[u8],
) -> Option<CommandResult> {
    let tid = PayloadReader::new(data).read_u64_be().ok()?;
    let thread_obj = {
        let ds = shared.debug.debug_state.lock();
        if super::commands::live_thread_refusal(&ds, tid).is_some() {
            return None;
        }
        super::object_for_id(shared, &ds, ds.wire_thread(tid))?
    };
    let group = group_of_thread(ctx, thread_obj)?;
    let mut ds = shared.debug.debug_state.lock();
    let mut pw = PayloadWriter::new();
    pw.put_u64_be(super::export_object(
        shared,
        &mut ds,
        group,
        ObjectExport::Sent,
    ));
    Some(CommandResult::ok(pw.into_bytes()))
}

/// `ThreadGroupReference.Name` (12/1), `Parent` (12/2) and `Children`
/// (12/3) of a group object. `None` for the one-group model's own id;
/// `INVALID_OBJECT` for an id naming nothing, `INVALID_THREAD_GROUP` for an
/// object that is not a `ThreadGroup`.
fn thread_group_reference(
    shared: &SharedVm,
    ctx: &crate::vm::NativeContextImpl<'_>,
    command: u8,
    data: &[u8],
) -> Option<CommandResult> {
    let group_id = PayloadReader::new(data).read_u64_be().ok()?;
    if group_id == super::commands::SYSTEM_THREAD_GROUP_ID {
        return None;
    }
    let group = {
        let cm = shared.classes.class_manager.read();
        let ds = shared.debug.debug_state.lock();
        match object(shared, &ds, group_id) {
            Ok(g) if object_tag(shared, &cm, g) == b'g' => g,
            Ok(_) => {
                return Some(CommandResult::error(
                    super::commands::ERR_INVALID_THREAD_GROUP,
                ))
            }
            Err(code) => return Some(CommandResult::error(code)),
        }
    };
    let mut pw = PayloadWriter::new();
    match command {
        CMD_TGR_NAME => {
            use cratonvm_native_api::NativeContext as _;
            let name = match ctx.get_field_by_name(group, "name") {
                Value::Object(Some(s)) => crate::vm::read_java_string(&shared.mem.heap, s),
                _ => None,
            };
            pw.put_string(name.as_deref().unwrap_or(""));
        }
        CMD_TGR_PARENT => {
            let parent = group_parent(ctx, group);
            let mut ds = shared.debug.debug_state.lock();
            pw.put_u64_be(parent.map_or(0, |p| {
                super::export_object(shared, &mut ds, p, ObjectExport::Sent)
            }));
        }
        CMD_TGR_CHILDREN => {
            // Interpreter round i5 wave 102, lane L1: platform threads only.
            // HotSpot's back end lists JVMTI `GetThreadGroupChildren`'s
            // threads, which never include a virtual thread (JDK 21+ JVMTI),
            // whatever `includevirtualthreads` says (that adds them to
            // `AllThreads` only); here every virtual thread was a child of
            // the `VirtualThreads` group.
            let virtual_tids: std::collections::HashSet<u64> = {
                let ds = shared.debug.debug_state.lock();
                super::commands::known_thread_ids(&ds)
                    .into_iter()
                    .filter(|&tid| ds.is_virtual_thread(tid))
                    .collect()
            };
            let threads: Vec<u64> = listed_thread_objects(shared)
                .into_iter()
                .filter(|(tid, _)| !virtual_tids.contains(tid))
                .filter(|&(_, t)| {
                    group_of_thread(ctx, t).is_some_and(|g| g.as_ptr() == group.as_ptr())
                })
                .map(|(tid, _)| tid)
                .collect();
            let subgroups = group_subgroups(shared, ctx, group);
            let mut ds = shared.debug.debug_state.lock();
            // Casts: bounded by the thread and group counts.
            pw.put_u32_be(threads.len() as u32);
            for tid in threads {
                pw.put_u64_be(ds.wire_thread(tid));
            }
            pw.put_u32_be(subgroups.len() as u32);
            for sub in subgroups {
                pw.put_u64_be(super::export_object(
                    shared,
                    &mut ds,
                    sub,
                    ObjectExport::Sent,
                ));
            }
        }
        _ => return None,
    }
    Some(CommandResult::ok(pw.into_bytes()))
}

/// `ThreadReference.Stop` (11/10): `thread`, `throwable`. The thread throws
/// the object at the next bytecode it interprets (`debug::set_pending_stop`,
/// delivered by the interpreter's suspend point): at once when it is
/// suspended at one and resumed, and a running thread reaches one because a
/// pending stop arms the debugger gate for every method. HotSpot's JVMTI
/// `StopThread` throws it as an asynchronous exception at the thread's
/// current bytecode. `INVALID_THREAD` for an id naming no live thread,
/// `INVALID_OBJECT` for an object id naming nothing. It answered
/// `NOT_IMPLEMENTED` (jdb `kill`). This records the stop only; the command's
/// two callers ([`stop_thread`], [`tr_stop_quiesced`]) also wake the thread
/// (wave 25), which a blocked thread needs to throw it before its blocking
/// call returns on its own.
fn tr_stop(
    shared: &SharedVm,
    cm: &ClassManager,
    ds: &mut DebugState,
    r: &mut PayloadReader<'_>,
) -> Reply {
    let tid = wire(r.read_u64_be())?;
    let throwable = wire(r.read_u64_be())?;
    if let Some(code) = super::commands::live_thread_refusal(ds, tid) {
        return Err(code);
    }
    // Only a `Throwable` can be thrown (JVMTI `StopThread` refuses anything
    // else); a thread's id names its `Thread` object too.
    if !is_throwable(shared, cm, object(shared, ds, throwable)?) {
        return Err(ERR_INVALID_OBJECT);
    }
    if !super::set_pending_stop(shared, ds, tid, throwable) {
        return Err(ERR_INVALID_OBJECT);
    }
    Ok(Vec::new())
}

/// `ReferenceType.ClassObject` (2/11, interpreter round i1 wave 23): the
/// class's `java.lang.Class` mirror, created if the class has none yet, on a
/// parked thread or the heap service (a mutator; no lock is held while the
/// mirror is allocated). It was not served (`NOT_IMPLEMENTED`), so JDI's
/// `ReferenceType.classObject()` — an IDE's `Foo.class`, a static field's
/// class node — threw. On the quiesced server path ([`execute`]) only a
/// mirror that already exists is answered.
fn class_object(shared: &SharedVm, data: &[u8]) -> CommandResult {
    let Ok(raw) = PayloadReader::new(data).read_u64_be() else {
        return CommandResult::error(ERR_INTERNAL);
    };
    let Ok(raw) = u32::try_from(raw) else {
        return CommandResult::error(ERR_INVALID_CLASS);
    };
    let class_id = ClassId::new(raw);
    let known = shared
        .classes
        .class_manager
        .read()
        .class_store
        .get(class_id)
        .is_some()
        || shared.classes.is_lambda_proxy_class(class_id);
    if !known {
        return CommandResult::error(ERR_INVALID_CLASS);
    }
    let Some(mirror) = crate::vm::try_get_or_create_class_mirror(shared, class_id) else {
        return CommandResult::error(ERR_INTERNAL);
    };
    // The mirror is held by the class-mirror table; no safepoint between
    // its lookup and the export.
    let mut ds = shared.debug.debug_state.lock();
    let id = super::export_object(shared, &mut ds, mirror, ObjectExport::Sent);
    let mut pw = PayloadWriter::new();
    pw.put_u64_be(id);
    CommandResult::ok(pw.into_bytes())
}

/// [`class_object`] where nothing may be allocated (the server thread,
/// quiesced): the class's mirror if it already exists, else `INTERNAL`.
fn rt_existing_class_object(
    shared: &SharedVm,
    cm: &ClassManager,
    ds: &mut DebugState,
    r: &mut PayloadReader<'_>,
) -> Reply {
    let raw = u32::try_from(wire(r.read_u64_be())?).map_err(|_| ERR_INVALID_CLASS)?;
    let class_id = ClassId::new(raw);
    if cm.class_store.get(class_id).is_none() && !shared.classes.is_lambda_proxy_class(class_id) {
        return Err(ERR_INVALID_CLASS);
    }
    let mirror = shared
        .classes
        .class_mirrors
        .read()
        .get(&class_id)
        .copied()
        .ok_or(ERR_INTERNAL)?;
    let mut pw = PayloadWriter::new();
    pw.put_u64_be(super::export_object(shared, ds, mirror, ObjectExport::Sent));
    Ok(pw.into_bytes())
}

/// A heap command on the JDWP server thread, which is not a mutator: run it
/// only while no stop-the-world pause is requested, and keep one from being
/// requested until it is done. Retries every millisecond while a pause is in
/// flight. The class-manager lock is taken first and released between tries
/// (a pause may need it).
fn run_quiesced(shared: &SharedVm, command_set: u8, command: u8, data: &[u8]) -> CommandResult {
    loop {
        {
            let cm = shared.classes.class_manager.read();
            let mut out = None;
            let ran = shared.mem.gc_barrier.run_if_no_stw_requested(|| {
                let mut ds = shared.debug.debug_state.lock();
                out = Some(execute(shared, &cm, &mut ds, command_set, command, data));
            });
            if let (true, Some(result)) = (ran, out) {
                return result;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// `VirtualMachine.CreateString`: a real `java.lang.String`, allocated with
/// the collecting allocator on a parked thread. Until wave 8 the id named no
/// object at all (a side-table entry), so passing it to an invocation was
/// `INVALID_OBJECT`.
fn create_string(shared: &SharedVm, thread: &mut JvmThread, data: &[u8]) -> CommandResult {
    let mut reader = PayloadReader::new(data);
    let Ok(text) = reader.read_string() else {
        return CommandResult::error(ERR_INTERNAL);
    };
    let units: Vec<u16> = text.encode_utf16().collect();
    let Ok(string) =
        crate::runtime::interpreter::create_string_from_units_or_oom(shared, thread, &units)
    else {
        return CommandResult::error(ERR_INTERNAL);
    };
    // No safepoint between the allocation's return and the export.
    let mut ds = shared.debug.debug_state.lock();
    let id = super::export_object(shared, &mut ds, string, ObjectExport::Sent);
    let mut pw = PayloadWriter::new();
    pw.put_u64_be(id);
    CommandResult::ok(pw.into_bytes())
}

/// Every heap command but `CreateString`, with no collection able to run
/// (a parked mutator between polls, or the server inside
/// `run_if_no_stw_requested`).
fn execute(
    shared: &SharedVm,
    cm: &ClassManager,
    ds: &mut DebugState,
    command_set: u8,
    command: u8,
    data: &[u8],
) -> CommandResult {
    // The tag and the wire id of a snapshot's object id (wave 24: a live
    // thread's value is its thread id).
    let tag_of = |ds: &DebugState, id: u64| {
        super::object_for_id(shared, ds, id).map(|o| match object_tag(shared, cm, o) {
            b't' => thread_value(shared, ds, o).unwrap_or((b'L', id)),
            tag => (tag, id),
        })
    };
    match (command_set, command) {
        (CS_STACK_FRAME, CMD_SF_GET_VALUES) => {
            return super::commands::sf_get_values_tagged(data, ds, &tag_of);
        }
        (CS_STACK_FRAME, CMD_SF_THIS_OBJECT) => {
            return super::commands::sf_this_object_tagged(data, ds, &tag_of);
        }
        // Without a mutator to read the program's thread groups (wave 24):
        // the one `system` group `commands::dispatch` models.
        (set, cmd) if is_thread_group_command(set, cmd) => {
            return super::commands::dispatch(set, cmd, data, ds);
        }
        _ => {}
    }
    let mut r = PayloadReader::new(data);
    let result = match (command_set, command) {
        (CS_OBJECT_REF, CMD_OR_REFERENCE_TYPE) => or_reference_type(shared, cm, ds, &mut r),
        (CS_OBJECT_REF, CMD_OR_GET_VALUES) => or_get_values(shared, cm, ds, &mut r),
        (CS_OBJECT_REF, CMD_OR_SET_VALUES) => or_set_values(shared, cm, ds, &mut r),
        (CS_OBJECT_REF, CMD_OR_DISABLE_COLLECTION) => set_collection(shared, ds, &mut r, false),
        (CS_OBJECT_REF, CMD_OR_ENABLE_COLLECTION) => set_collection(shared, ds, &mut r, true),
        (CS_OBJECT_REF, CMD_OR_IS_COLLECTED) => or_is_collected(shared, ds, &mut r),
        (CS_REF_TYPE, CMD_RT_GET_VALUES) => rt_get_values(shared, cm, ds, &mut r),
        (CS_REF_TYPE, CMD_RT_CLASS_LOADER) => rt_class_loader(shared, cm, ds, &mut r),
        (CS_REF_TYPE, CMD_RT_CLASS_OBJECT) => rt_existing_class_object(shared, cm, ds, &mut r),
        (CS_CLASS_TYPE, CMD_CT_SET_VALUES) => ct_set_values(shared, cm, ds, &mut r),
        (CS_STRING_REF, CMD_SR_VALUE) => sr_value(shared, cm, ds, &mut r),
        (CS_ARRAY_REF, CMD_AR_LENGTH) => ar_length(shared, ds, &mut r),
        (CS_ARRAY_REF, CMD_AR_GET_VALUES) => ar_get_values(shared, cm, ds, &mut r),
        (CS_ARRAY_REF, CMD_AR_SET_VALUES) => ar_set_values(shared, cm, ds, &mut r),
        (CS_CLASS_OBJ_REF, CMD_COR_REFLECTED_TYPE) => cor_reflected_type(shared, cm, ds, &mut r),
        // Interpreter round i1 wave 24.
        (CS_THREAD_REF, CMD_TR_STOP) => tr_stop_quiesced(shared, cm, ds, data),
        // Wave 25: from the registry's record of the thread's monitors.
        (CS_THREAD_REF, cmd) if is_monitor_command(CS_THREAD_REF, cmd) => {
            wire(r.read_u64_be()).and_then(|tid| monitor_reply(shared, cm, ds, cmd, tid, None))
        }
        (CS_THREAD_REF, CMD_TR_INTERRUPT) => tr_interrupt_quiesced(shared, ds, &mut r),
        (CS_CLASSLOADER_REF, CMD_CLR_VISIBLE_CLASSES) => {
            visible_classes_quiesced(shared, cm, ds, &mut r)
        }
        // Interpreter round i5 wave 103 (lane L1).
        (CS_OBJECT_REF, super::commands::CMD_OR_MONITOR_INFO) => {
            or_monitor_info(shared, ds, &mut r)
        }
        // A heap walk needs a parked thread to pause the world from.
        (set, cmd) if is_heap_walk_command(set, cmd) => Err(ERR_THREAD_NOT_SUSPENDED),
        _ => Err(ERR_INTERNAL),
    };
    // Interpreter round i6 wave 127 (lane L1, `CRATONVM_JDWP_EVENT_OBJECT_HOLD`):
    // the first command naming an event's object has seen it; its hold goes.
    super::release_event_hold_named(shared, ds, command_set, data);
    match result {
        Ok(bytes) => CommandResult::ok(bytes),
        Err(code) => CommandResult::error(code),
    }
}

/// A command's reply payload, or its JDWP error code.
type Reply = Result<Vec<u8>, u16>;

/// `ObjectReference.MonitorInfo` (9/5, interpreter round i5 wave 103, lane
/// L1; `canGetMonitorInfo`): the owner (a thread, or null), its entry count,
/// and the threads waiting for a `notify` in `Object.wait`, in arrival order,
/// as HotSpot's back end answers it from JVMTI `GetObjectMonitorUsage`'s
/// `notify_waiters` (JDK 23+; the C table's `get_object_monitor_usage`
/// reads the same monitor state).
///
/// Interpreter round i5 wave 111, lane L1 (`L1W110JdiStopMonitorsStress`:
/// HotSpot `LockB owner=owner entryCount=1 waiting=[contender]`, CratonVM
/// `waiting=[]`): JDK 23+'s back end (`objectReferenceImpl.c`) writes
/// `waiter_count + notify_waiter_count` and then BOTH lists, the threads
/// waiting to own the monitor (blocked entering it, or notified and
/// re-entering it) first and the threads waiting for a notify after. Only
/// the second list was sent. Both now come from [`monitor_usage`], the one
/// snapshot the C table's `GetObjectMonitorUsage` reads too.
fn or_monitor_info(shared: &SharedVm, ds: &DebugState, r: &mut PayloadReader<'_>) -> Reply {
    let obj = object(shared, ds, wire(r.read_u64_be())?)?;
    let usage = monitor_usage(shared, obj);
    let waiters: Vec<u64> = usage
        .entering
        .iter()
        .chain(usage.waiting.iter())
        .map(|tid| ds.wire_thread(tid.0))
        .collect();
    let mut pw = PayloadWriter::new();
    pw.put_u64_be(usage.owner.map_or(0, |tid| ds.wire_thread(tid.0)));
    pw.put_u32_be(usage.entry_count);
    // Cast: bounded by the live threads.
    pw.put_u32_be(waiters.len() as u32);
    for waiter in waiters {
        pw.put_u64_be(waiter);
    }
    Ok(pw.into_bytes())
}

/// The usage of `obj`'s monitor as JVMTI `GetObjectMonitorUsage` reports it
/// (JDK 23+, JDK-8247972) and JDWP `ObjectReference.MonitorInfo` sends it
/// (interpreter round i5 wave 111, lane L1).
pub(crate) struct MonitorUsage {
    /// The owning thread, if any.
    pub(crate) owner: Option<crate::threading::ThreadId>,
    /// The owner's entry count; 0 without an owner.
    pub(crate) entry_count: u32,
    /// JVMTI `waiters`: the live threads waiting to own the monitor -- blocked
    /// entering it, or notified and re-entering it -- in thread-list order
    /// (HotSpot's `Threads::get_pending_threads` walks the threads list; this
    /// registry's ids are its start order).
    pub(crate) entering: Vec<crate::threading::ThreadId>,
    /// JVMTI `notify_waiters`: the live threads in `Object.wait` no notify
    /// marked yet, in wait-set (arrival) order (HotSpot walks
    /// `ObjectMonitor::first_waiter` / `next_waiter`).
    pub(crate) waiting: Vec<crate::threading::ThreadId>,
}

/// [`MonitorUsage`] of `obj` from one read of the monitor
/// (`MonitorTable::usage_snapshot`: owner, count and wait set together;
/// they were three reads, i110-L1 page) and the registry's contention
/// records. A thread that owns the monitor by the table while the registry
/// still records it contending for it -- a suspended winner giving the
/// monitor back (`give_back_fresh_hold_while_held_off`, HotSpot's
/// `ExitOnSuspend`), or one that has not yet published its acquisition --
/// is reported entering, not owning: HotSpot's safepoint read never sees a
/// thread inside `ExitOnSuspend` as the owner.
pub(crate) fn monitor_usage(shared: &SharedVm, obj: ObjectRef) -> MonitorUsage {
    let registry = &shared.threads.thread_registry;
    let (mut owner, mut entry_count, unnotified) = shared.threads.monitors.usage_snapshot(obj);
    if let Some(o) = owner {
        if registry.current_contended_monitor(o) == Some(obj) {
            owner = None;
            entry_count = 0;
        }
    }
    let mut tids: Vec<crate::threading::ThreadId> = registry
        .all_thread_names()
        .into_iter()
        .map(|(tid, _)| tid)
        .collect();
    tids.sort_unstable_by_key(|tid| tid.0);
    let entering = tids
        .into_iter()
        .filter(|&tid| {
            registry.is_alive(tid)
                && Some(tid) != owner
                && !unnotified.contains(&tid)
                && (registry.current_contended_monitor(tid) == Some(obj)
                    || registry.peek_jmx_waiting_monitor(tid) == Some(obj))
        })
        .collect();
    let waiting = unnotified
        .into_iter()
        .filter(|&tid| registry.is_alive(tid) && Some(tid) != owner)
        .collect();
    MonitorUsage {
        owner,
        entry_count,
        entering,
        waiting,
    }
}

/// A heap walk command ([`is_heap_walk_command`]) on parked thread `thread`:
/// read the request ([`heap_walk_query`]), walk the heap in a non-moving
/// pause (`jvmti::native_env::debugger_heap_query`) and answer -- counts for
/// `InstanceCounts`, tagged object ids for `Instances` and
/// `ReferringObjects`.
fn heap_walk_command(
    shared: &SharedVm,
    thread: &mut JvmThread,
    command_set: u8,
    command: u8,
    data: &[u8],
) -> CommandResult {
    use crate::jvmti::native_env::{debugger_heap_query, DebuggerHeapAnswer};
    let mut r = PayloadReader::new(data);
    let query = match heap_walk_query(shared, command_set, command, &mut r) {
        Ok(query) => query,
        Err(code) => return CommandResult::error(code),
    };
    // Interpreter round i5 wave 104 (lane L1): reachable objects only, as
    // HotSpot's `FollowReferences` (`CRATONVM_JDWP_HEAP_WALK_FROM_ROOTS`).
    let from_roots = shared.debug.debug_state.lock().heap_walk_from_roots;
    let answer = debugger_heap_query(shared, thread, query, from_roots);
    // No safepoint since the walk: the objects are where it found them.
    let cm = shared.classes.class_manager.read();
    let mut ds = shared.debug.debug_state.lock();
    let mut pw = PayloadWriter::new();
    match answer {
        DebuggerHeapAnswer::Counts(counts) => {
            // Cast: the request's own count.
            pw.put_u32_be(counts.len() as u32);
            for count in counts {
                pw.put_u64_be(count);
            }
        }
        DebuggerHeapAnswer::Objects(objects) => {
            // Cast: bounded by the heap's object count.
            pw.put_u32_be(objects.len() as u32);
            for obj in objects {
                let tag = Some(object_tag(shared, &cm, obj));
                put_tagged_with_tag(shared, &mut ds, &mut pw, b'L', Value::Object(Some(obj)), tag);
            }
        }
    }
    CommandResult::ok(pw.into_bytes())
}

/// The [`crate::jvmti::native_env::DebuggerHeapQuery`] a heap walk command's
/// payload asks for. A negative count or maximum is `ILLEGAL_ARGUMENT`, a
/// class id naming nothing `INVALID_OBJECT`, as HotSpot's back end reads
/// them (`inStream_readClassRef`).
fn heap_walk_query(
    shared: &SharedVm,
    command_set: u8,
    command: u8,
    r: &mut PayloadReader<'_>,
) -> Result<crate::jvmti::native_env::DebuggerHeapQuery, u16> {
    use super::commands::{
        CMD_OR_REFERRING_OBJECTS, CMD_RT_INSTANCES, CMD_VM_INSTANCE_COUNTS, ERR_ILLEGAL_ARGUMENT,
    };
    use crate::jvmti::native_env::DebuggerHeapQuery;
    // A JDWP int read as a non-negative count.
    let count = |r: &mut PayloadReader<'_>| -> Result<usize, u16> {
        // Cast: a JDWP int.
        let n = wire(r.read_u32_be())? as i32;
        usize::try_from(n).map_err(|_| ERR_ILLEGAL_ARGUMENT)
    };
    let known_class = |raw: u64| -> Result<ClassId, u16> {
        let raw = u32::try_from(raw).map_err(|_| ERR_INVALID_OBJECT)?;
        let id = ClassId::new(raw);
        let known = shared.classes.class_manager.read().get_class(id).is_some()
            || shared.classes.is_lambda_proxy_class(id);
        if known {
            Ok(id)
        } else {
            Err(ERR_INVALID_OBJECT)
        }
    };
    match (command_set, command) {
        (CS_VM, CMD_VM_INSTANCE_COUNTS) => {
            let n = count(r)?;
            let mut classes = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                // Ids after the first are in their wire form
                // (`debug::ids_from_wire` rewrites leading ids only).
                let wire_id = wire(r.read_u64_be())?;
                classes.push(known_class(super::ids::class_from_wire(wire_id))?);
            }
            Ok(DebuggerHeapQuery::InstanceCounts(classes))
        }
        (CS_REF_TYPE, CMD_RT_INSTANCES) => {
            // The leading class id is already the VM's (`ids_from_wire`).
            let class = known_class(wire(r.read_u64_be())?)?;
            let max = count(r)?;
            Ok(DebuggerHeapQuery::Instances(class, max))
        }
        (CS_OBJECT_REF, CMD_OR_REFERRING_OBJECTS) => {
            let id = wire(r.read_u64_be())?;
            let max = count(r)?;
            let target = {
                let ds = shared.debug.debug_state.lock();
                object(shared, &ds, id)?
            };
            Ok(DebuggerHeapQuery::Referrers(target, max))
        }
        _ => Err(ERR_INTERNAL),
    }
}

/// Any wire read that ran out of bytes is a malformed packet.
fn wire<T>(r: std::io::Result<T>) -> Result<T, u16> {
    r.map_err(|_| ERR_INTERNAL)
}

/// The live object a non-null id names, or `INVALID_OBJECT`.
fn object(shared: &SharedVm, ds: &DebugState, id: u64) -> Result<ObjectRef, u16> {
    if id == 0 {
        return Err(ERR_INVALID_OBJECT);
    }
    super::object_for_id(shared, ds, id).ok_or(ERR_INVALID_OBJECT)
}

pub(super) fn is_array(shared: &SharedVm, obj: ObjectRef) -> bool {
    shared.mem.heap.kind_of(obj) == ObjectKind::Array
}

/// The JDWP value tag of object `obj`: `[` for an array, `s` for a
/// `java.lang.String`, `c` for a `java.lang.Class`, `l` for a class loader,
/// `t` for a `java.lang.Thread`, `g` for a `java.lang.ThreadGroup`, `L`
/// otherwise, as HotSpot's back end tags them (`specificTypeKey`). The tag
/// decides which JDI mirror the debugger builds (a `String` tagged `L` is
/// printed as "instance of java.lang.String", not as its text).
///
/// Interpreter round i1 wave 24: a `Thread` and a `ThreadGroup` were tagged
/// `L`. A value tagged `t` must carry the thread's THREAD id, which is not
/// its object id here: every writer of a tagged object value resolves a `t`
/// through [`thread_value`] (the thread id of a thread the server lists,
/// else `L` and the object id). A thread group's id is its object id.
pub(crate) fn object_tag(shared: &SharedVm, cm: &ClassManager, obj: ObjectRef) -> u8 {
    if is_array(shared, obj) {
        return b'[';
    }
    let mut class = Some(shared.mem.heap.class_id_of(obj));
    for _ in 0..MAX_CHAIN {
        let Some(c) = class.and_then(|id| cm.class_store.get(id)) else {
            break;
        };
        match &*c.name {
            "java/lang/String" => return b's',
            "java/lang/Class" => return b'c',
            "java/lang/ClassLoader" => return b'l',
            "java/lang/Thread" => return b't',
            "java/lang/ThreadGroup" => return b'g',
            _ => {}
        }
        class = c.superclass;
    }
    b'L'
}

/// A thread value's wire form (interpreter round i1 wave 24): `(b't', thread
/// id)` when `obj` is the `java.lang.Thread` of a thread the server knows —
/// the id events, frames and the `ThreadReference` commands use, so the
/// value and the event's `thread()` are one JDI `ThreadReference`; a thread
/// that has ended but whose entry the registry still keeps answers
/// `ThreadReference.Status` ZOMBIE through it, as HotSpot answers for a
/// terminated thread. `None` for a thread not started, or purged: written as
/// a plain object, `L`. Lock order: the debug state, then the thread
/// registry's map.
pub(crate) fn thread_value(
    shared: &SharedVm,
    ds: &DebugState,
    obj: ObjectRef,
) -> Option<(u8, u64)> {
    let tid = match shared
        .threads
        .thread_registry
        .find_thread_id_by_thread_obj(obj)
    {
        Some(tid) => tid.0,
        // A started JDK virtual thread no carrier has mounted yet has no
        // entry until its first mount: name it now
        // (`vm::jdk_continuation::ensure_continuation_thread`).
        None => crate::vm::jdk_continuation::ensure_continuation_thread(shared, obj)?.0,
    };
    // Every registered thread but the back end's own heap service, whether
    // or not the server's thread table has caught up with it (a thread id
    // names its `Thread` object from the registry, `debug::object_for_id`).
    // Its wire id (wave 25: the main thread's is not 0, JDWP's null).
    // Interpreter round i5 wave 104 (lane L1): once `main` has ended its
    // `Thread` still resolves to the launcher's entry (VM thread 0, now
    // `DestroyJavaVM`) through its `Thread.tid`; it is the ended `main`.
    if tid == 0
        && ds.destroy_java_vm_split.is_some()
        && shared
            .threads
            .thread_registry
            .java_thread_obj(crate::threading::ThreadId(0))
            .is_some_and(|current| current.as_ptr() != obj.as_ptr())
    {
        return Some((b't', super::ids::MAIN_THREAD_WIRE_ID));
    }
    (ds.heap_service_thread() != Some(tid)).then_some((b't', ds.wire_thread(tid)))
}

/// The JDWP `refTypeTag` of class `class_id`: 3 for an array class, 2 for an
/// interface, 1 otherwise.
fn ref_type_tag(cm: &ClassManager, class_id: ClassId) -> u8 {
    match cm.class_store.get(class_id) {
        Some(c) if c.name.starts_with('[') => 3,
        Some(c) if c.is_interface() => 2,
        _ => 1,
    }
}

/// Is `obj` a `java.lang.Throwable` (its class or a superclass is)?
fn is_throwable(shared: &SharedVm, cm: &ClassManager, obj: ObjectRef) -> bool {
    let mut class = Some(shared.mem.heap.class_id_of(obj));
    for _ in 0..MAX_CHAIN {
        let Some(c) = class.and_then(|id| cm.class_store.get(id)) else {
            return false;
        };
        if &*c.name == "java/lang/Throwable" {
            return true;
        }
        class = c.superclass;
    }
    false
}

/// Is `obj` an instance of class `class_id` (its class or a superclass)?
fn instance_of_class(
    shared: &SharedVm,
    cm: &ClassManager,
    obj: ObjectRef,
    class_id: ClassId,
) -> bool {
    let mut class = Some(shared.mem.heap.class_id_of(obj));
    for _ in 0..MAX_CHAIN {
        let Some(id) = class else {
            return false;
        };
        if id == class_id {
            return true;
        }
        class = cm.class_store.get(id).and_then(|c| c.superclass);
    }
    false
}

/// Where a field's value lives.
#[derive(Debug, Clone, Copy)]
struct FieldSlot {
    class_id: ClassId,
    /// The first byte of the field's descriptor.
    desc: u8,
    is_static: bool,
    /// The static slot of `class_id`, or the absolute instance slot.
    slot: usize,
}

/// Resolve a JDWP field id against `obj` (`None`: a static field is
/// required). `INVALID_FIELDID` for an id naming no field, an instance field
/// the object does not have, or an instance field without an object.
fn resolve_field(
    shared: &SharedVm,
    cm: &ClassManager,
    obj: Option<ObjectRef>,
    field_id: u64,
) -> Result<FieldSlot, u16> {
    let (class_id, index) = super::field_of_id(field_id);
    let Some(class) = cm.class_store.get(class_id) else {
        return lambda_proxy_field(shared, obj, class_id, index);
    };
    let field = class.fields.get(index).ok_or(ERR_INVALID_FIELDID)?;
    let desc = field.descriptor.as_bytes().first().copied().unwrap_or(b'I');
    let before = &class.fields[..index];
    if field.is_static() {
        return Ok(FieldSlot {
            class_id,
            desc,
            is_static: true,
            slot: before.iter().filter(|f| f.is_static()).count(),
        });
    }
    let obj = obj.ok_or(ERR_INVALID_FIELDID)?;
    if !instance_of_class(shared, cm, obj, class_id) {
        return Err(ERR_INVALID_FIELDID);
    }
    let slot = class.first_field_index + before.iter().filter(|f| !f.is_static()).count();
    if slot >= shared.mem.heap.num_fields(obj) {
        return Err(ERR_INVALID_FIELDID);
    }
    Ok(FieldSlot {
        class_id,
        desc,
        is_static: false,
        slot,
    })
}

/// Capture field `index` (`arg$<index + 1>`) of lambda proxy class `class_id`
/// on `obj`, an instance of it (the i54-L1 retirement): the proxy object's
/// field slot `index`, where the lambda dispatch reads the capture, typed by
/// the capture's kind. `INVALID_FIELDID` for any other class or object.
fn lambda_proxy_field(
    shared: &SharedVm,
    obj: Option<ObjectRef>,
    class_id: crate::classloading::ClassId,
    index: usize,
) -> Result<FieldSlot, u16> {
    let site = shared
        .classes
        .lambda_call_site_for(class_id)
        .ok_or(ERR_INVALID_FIELDID)?;
    let kind = *site.capture_types.get(index).ok_or(ERR_INVALID_FIELDID)?;
    let obj = obj.ok_or(ERR_INVALID_FIELDID)?;
    if shared.mem.heap.class_id_of(obj) != class_id || index >= shared.mem.heap.num_fields(obj) {
        return Err(ERR_INVALID_FIELDID);
    }
    Ok(FieldSlot {
        class_id,
        desc: u8::try_from(kind).unwrap_or(b'L'),
        is_static: false,
        slot: index,
    })
}

fn read_field(shared: &SharedVm, obj: Option<ObjectRef>, field: FieldSlot) -> Value {
    match obj {
        Some(o) if !field.is_static => shared.mem.heap.get_field_as(o, field.slot, field.desc),
        _ => crate::vm::get_static_shared(shared, field.class_id, field.slot),
    }
}

/// Store through the VM's own accessors, as for `putfield` / `putstatic`.
///
/// The card mark runs inside `set_field_as`, and so does the SATB pre-barrier
/// on G1 and ZGC; the Generational accessors card-mark only. nio21 wave 5 L1
/// (page
/// `r21-direct-heap-api-reference-stores-outside-the-jit-skip-generational-satb-FIXED-20261012.md`):
/// an instance-field store therefore publishes the value it overwrites first,
/// through the same `vm_exec::publish_overwritten_field` every
/// `NativeContextImpl` field store runs (a no-op unless a Generational mark is
/// under way, the slot may hold a reference, and `CRATONVM_NATIVE_STORE_SATB`
/// is on).
fn write_field(shared: &SharedVm, obj: Option<ObjectRef>, field: FieldSlot, value: Value) {
    match obj {
        Some(o) if !field.is_static => {
            let heap = &shared.mem.heap;
            let (o, _) = heap.load_and_forward_checked(o);
            crate::vm::vm_exec::publish_overwritten_field(
                heap,
                crate::runtime::env_cache::native_store_satb,
                o,
                field.slot,
                Some(field.desc),
            );
            heap.set_field_as(o, field.slot, value, field.desc);
        }
        _ => crate::vm::set_static_shared(shared, field.class_id, field.slot, value),
    }
}

fn as_int(v: Value) -> i32 {
    match v {
        Value::Int(i) => i,
        // Truncation: a sub-int or int slot decoded wide
        Value::Long(l) => l as i32,
        _ => 0,
    }
}

fn as_long(v: Value) -> i64 {
    match v {
        Value::Long(l) => l,
        Value::Int(i) => i64::from(i),
        _ => 0,
    }
}

fn as_float_bits(v: Value) -> u32 {
    match v {
        Value::Float(f) => f.to_bits(),
        // Cast: a zero-initialised or int-shaped slot, numerically
        Value::Int(i) => (i as f32).to_bits(),
        _ => 0,
    }
}

fn as_double_bits(v: Value) -> u64 {
    match v {
        Value::Double(d) => d.to_bits(),
        // Cast: a long-shaped slot holds the double's raw bits (as `getstatic`
        // reads it)
        Value::Long(l) => l as u64,
        Value::Int(i) => f64::from(i).to_bits(),
        _ => 0,
    }
}

fn as_ref(v: Value) -> Option<ObjectRef> {
    match v {
        Value::Object(o) => o,
        _ => None,
    }
}

/// Write a primitive value of descriptor type `desc` untagged.
fn put_untagged_primitive(pw: &mut PayloadWriter, desc: u8, v: Value) {
    // Casts below: JDWP's fixed-width, two's-complement wire encodings.
    match desc {
        b'Z' => pw.put_u8(u8::from(as_int(v) != 0)),
        b'B' => pw.put_u8(as_int(v) as u8),
        b'C' | b'S' => pw.put_u16_be(as_int(v) as u16),
        b'I' => pw.put_u32_be(as_int(v) as u32),
        b'J' => pw.put_u64_be(as_long(v) as u64),
        b'F' => pw.put_u32_be(as_float_bits(v)),
        b'D' => pw.put_u64_be(as_double_bits(v)),
        _ => {}
    }
}

/// Write `v`, read from a slot of descriptor type `desc`, as a JDWP tagged
/// value; an object goes out as its (exported) object id.
fn put_tagged(
    shared: &SharedVm,
    cm: &ClassManager,
    ds: &mut DebugState,
    pw: &mut PayloadWriter,
    desc: u8,
    v: Value,
) {
    let tag = as_ref(v).map(|o| object_tag(shared, cm, o));
    put_tagged_with_tag(shared, ds, pw, desc, v, tag);
}

/// [`put_tagged`] with the object's tag ([`object_tag`]) computed by the
/// caller — for a caller that must not lock the class manager while it
/// holds the debug-state lock (the interpreter's exception and field-watch
/// events, wave 10). `object_tag` `None` writes an object as `L`.
pub(crate) fn put_tagged_with_tag(
    shared: &SharedVm,
    ds: &mut DebugState,
    pw: &mut PayloadWriter,
    desc: u8,
    v: Value,
    object_tag: Option<u8>,
) {
    if matches!(desc, b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D') {
        pw.put_u8(desc);
        put_untagged_primitive(pw, desc, v);
        return;
    }
    match as_ref(v) {
        Some(o) => {
            let tag = object_tag.unwrap_or(b'L');
            // A live thread goes out as its thread id (wave 24).
            if tag == b't' {
                if let Some((thread_tag, tid)) = thread_value(shared, ds, o) {
                    pw.put_u8(thread_tag);
                    pw.put_u64_be(tid);
                    return;
                }
                pw.put_u8(b'L');
            } else {
                pw.put_u8(tag);
            }
            pw.put_u64_be(super::export_object(shared, ds, o, ObjectExport::Sent));
        }
        None => {
            // `null` is tagged `L` whatever the declared type, as HotSpot's
            // back end tags it (`specificTypeKey(NULL)`); an array-typed
            // field or element went out as `[` (interpreter round i1 wave
            // 23; JDI reads both as null).
            pw.put_u8(b'L');
            pw.put_u64_be(0);
        }
    }
}

/// Read an untagged value of descriptor type `desc` off the wire.
fn read_untagged(
    shared: &SharedVm,
    ds: &DebugState,
    r: &mut PayloadReader<'_>,
    desc: u8,
) -> Result<Value, u16> {
    // Casts below: JDWP's fixed-width, two's-complement wire encodings.
    Ok(match desc {
        b'Z' => Value::Int(i32::from(wire(r.read_u8())? != 0)),
        b'B' => Value::Int(i32::from(wire(r.read_u8())? as i8)),
        b'C' => Value::Int(i32::from(wire(r.read_u16_be())?)),
        b'S' => Value::Int(i32::from(wire(r.read_u16_be())? as i16)),
        b'I' => Value::Int(wire(r.read_u32_be())? as i32),
        b'J' => Value::Long(wire(r.read_u64_be())? as i64),
        b'F' => Value::Float(f32::from_bits(wire(r.read_u32_be())?)),
        b'D' => Value::Double(f64::from_bits(wire(r.read_u64_be())?)),
        _ => {
            let id = wire(r.read_u64_be())?;
            if id == 0 {
                Value::Object(None)
            } else {
                let o = object(shared, ds, id)?;
                if desc == b'[' && !is_array(shared, o) {
                    return Err(ERR_TYPE_MISMATCH);
                }
                Value::Object(Some(o))
            }
        }
    })
}

/// `ObjectReference.ReferenceType` (9/1): the object's class, from its
/// header.
fn or_reference_type(
    shared: &SharedVm,
    cm: &ClassManager,
    ds: &mut DebugState,
    r: &mut PayloadReader<'_>,
) -> Reply {
    let obj = object(shared, ds, wire(r.read_u64_be())?)?;
    // An array's header names its component class (wave 25): its own class
    // when this VM has made it (the mutator path makes it,
    // [`array_reference_type`]).
    let array_class = if is_array(shared, obj) {
        array_descriptor(shared, cm, obj).and_then(|(desc, component)| {
            component
                .and_then(|c| cm.find_class_by_name_for_class(&desc, c))
                .or_else(|| {
                    cm.find_class_by_name_for_loader(
                        &desc,
                        crate::classloading::ClassLoaderId::Bootstrap,
                    )
                })
        })
    } else {
        None
    };
    let class_id = array_class.unwrap_or_else(|| shared.mem.heap.class_id_of(obj));
    let mut pw = PayloadWriter::new();
    pw.put_u8(ref_type_tag(cm, class_id));
    pw.put_u64_be(super::ids::class_to_wire(u64::from(class_id.as_u32())));
    Ok(pw.into_bytes())
}

/// `ObjectReference.GetValues` (9/2): instance or static fields.
fn or_get_values(
    shared: &SharedVm,
    cm: &ClassManager,
    ds: &mut DebugState,
    r: &mut PayloadReader<'_>,
) -> Reply {
    let obj = object(shared, ds, wire(r.read_u64_be())?)?;
    let count = wire(r.read_u32_be())?;
    let mut fields = Vec::new();
    for _ in 0..count {
        fields.push(resolve_field(
            shared,
            cm,
            Some(obj),
            wire(r.read_u64_be())?,
        )?);
    }
    let mut pw = PayloadWriter::new();
    pw.put_u32_be(count);
    for field in fields {
        let v = read_field(shared, Some(obj), field);
        put_tagged(shared, cm, ds, &mut pw, field.desc, v);
    }
    Ok(pw.into_bytes())
}

/// `ObjectReference.SetValues` (9/3): every value is read and checked before
/// anything is written.
fn or_set_values(
    shared: &SharedVm,
    cm: &ClassManager,
    ds: &mut DebugState,
    r: &mut PayloadReader<'_>,
) -> Reply {
    let obj = object(shared, ds, wire(r.read_u64_be())?)?;
    let count = wire(r.read_u32_be())?;
    let mut writes = Vec::new();
    for _ in 0..count {
        let field = resolve_field(shared, cm, Some(obj), wire(r.read_u64_be())?)?;
        writes.push((field, read_untagged(shared, ds, r, field.desc)?));
    }
    for (field, value) in writes {
        write_field(shared, Some(obj), field, value);
    }
    Ok(Vec::new())
}

/// `ReferenceType.GetValues` (2/6): static fields. Not served at all until
/// wave 8 (`NOT_IMPLEMENTED`), so jdb could print no static.
fn rt_get_values(
    shared: &SharedVm,
    cm: &ClassManager,
    ds: &mut DebugState,
    r: &mut PayloadReader<'_>,
) -> Reply {
    let _ref_type = wire(r.read_u64_be())?;
    let count = wire(r.read_u32_be())?;
    let mut fields = Vec::new();
    for _ in 0..count {
        fields.push(resolve_field(shared, cm, None, wire(r.read_u64_be())?)?);
    }
    let mut pw = PayloadWriter::new();
    pw.put_u32_be(count);
    for field in fields {
        let v = read_field(shared, None, field);
        put_tagged(shared, cm, ds, &mut pw, field.desc, v);
    }
    Ok(pw.into_bytes())
}

/// `ReferenceType.ClassLoader` (2/2): the class's defining loader, `null`
/// for the bootstrap loader. It answered `null` for every class, so a
/// debugger could not tell two same-named classes of different loaders apart.
fn rt_class_loader(
    shared: &SharedVm,
    cm: &ClassManager,
    ds: &mut DebugState,
    r: &mut PayloadReader<'_>,
) -> Reply {
    let raw = u32::try_from(wire(r.read_u64_be())?).map_err(|_| ERR_INVALID_CLASS)?;
    let class_id = ClassId::new(raw);
    let class_id = match cm.class_store.get(class_id) {
        Some(_) => class_id,
        None => proxy_host_under(shared, cm, class_id).ok_or(ERR_INVALID_CLASS)?,
    };
    let loader = cratonvm_native_builtins::classloader::defining_loader_for(
        &shared.loader_identity,
        class_id.as_u32(),
    );
    let mut pw = PayloadWriter::new();
    pw.put_u64_be(match loader {
        Some(l) => super::export_object(shared, ds, l, ObjectExport::Sent),
        None => 0,
    });
    Ok(pw.into_bytes())
}

/// `ClassType.SetValues` (3/2): static fields.
fn ct_set_values(
    shared: &SharedVm,
    cm: &ClassManager,
    ds: &mut DebugState,
    r: &mut PayloadReader<'_>,
) -> Reply {
    let _class = wire(r.read_u64_be())?;
    let count = wire(r.read_u32_be())?;
    let mut writes = Vec::new();
    for _ in 0..count {
        let field = resolve_field(shared, cm, None, wire(r.read_u64_be())?)?;
        writes.push((field, read_untagged(shared, ds, r, field.desc)?));
    }
    for (field, value) in writes {
        write_field(shared, None, field, value);
    }
    Ok(Vec::new())
}

/// `ObjectReference.DisableCollection` (9/7) / `EnableCollection` (9/8).
/// Interpreter round i1 wave 43 (lane L1), as HotSpot 25.0.3 answers
/// (`tools/probes/interp/L1/L1W43RawJdwpObjectErrorAnswers.java`): enabling
/// the collection of an id the session does not know (never handed out,
/// disposed of, or gone) succeeds and does nothing — there is nothing left
/// to hold — where disabling it is `INVALID_OBJECT`. Both answered
/// `INVALID_OBJECT`.
fn set_collection(
    shared: &SharedVm,
    ds: &mut DebugState,
    r: &mut PayloadReader<'_>,
    enable: bool,
) -> Reply {
    let id = wire(r.read_u64_be())?;
    if super::set_collection_enabled(shared, ds, id, enable) || (enable && id != 0) {
        Ok(Vec::new())
    } else {
        Err(ERR_INVALID_OBJECT)
    }
}

/// `ObjectReference.IsCollected` (9/9). Interpreter round i1 wave 43: an id
/// the session does not know (never handed out, or disposed of) is
/// collected, as HotSpot answers it (true); it was `INVALID_OBJECT`.
fn or_is_collected(shared: &SharedVm, ds: &mut DebugState, r: &mut PayloadReader<'_>) -> Reply {
    let id = wire(r.read_u64_be())?;
    if id == 0 {
        return Err(ERR_INVALID_OBJECT);
    }
    let collected = super::object_collected(shared, ds, id).unwrap_or(true);
    let mut pw = PayloadWriter::new();
    pw.put_u8(u8::from(collected));
    Ok(pw.into_bytes())
}

/// `StringReference.Value` (10/1): the string's characters.
fn sr_value(
    shared: &SharedVm,
    cm: &ClassManager,
    ds: &mut DebugState,
    r: &mut PayloadReader<'_>,
) -> Reply {
    let obj = object(shared, ds, wire(r.read_u64_be())?)?;
    if object_tag(shared, cm, obj) != b's' {
        return Err(ERR_INVALID_STRING);
    }
    let text = crate::vm::read_java_string(&shared.mem.heap, obj).ok_or(ERR_INVALID_STRING)?;
    let mut pw = PayloadWriter::new();
    pw.put_string(&text);
    Ok(pw.into_bytes())
}

/// The array `id` names, or `INVALID_OBJECT` / `INVALID_ARRAY`.
fn array(shared: &SharedVm, ds: &DebugState, id: u64) -> Result<ObjectRef, u16> {
    let obj = object(shared, ds, id)?;
    if is_array(shared, obj) {
        Ok(obj)
    } else {
        Err(ERR_INVALID_ARRAY)
    }
}

/// `ArrayReference.Length` (13/1).
fn ar_length(shared: &SharedVm, ds: &mut DebugState, r: &mut PayloadReader<'_>) -> Reply {
    let arr = array(shared, ds, wire(r.read_u64_be())?)?;
    let mut pw = PayloadWriter::new();
    // Cast: a Java array length fits an i32.
    pw.put_u32_be(shared.mem.heap.array_length(arr) as u32);
    Ok(pw.into_bytes())
}

/// The descriptor byte of `arr`'s elements (`L` or `[` for a reference
/// array, from the array class's name).
fn element_desc(shared: &SharedVm, cm: &ClassManager, arr: ObjectRef) -> u8 {
    match shared.mem.heap.array_element_type(arr) {
        Some(ArrayElementType::Boolean) => b'Z',
        Some(ArrayElementType::Byte) => b'B',
        Some(ArrayElementType::Char) => b'C',
        Some(ArrayElementType::Short) => b'S',
        Some(ArrayElementType::Int) => b'I',
        Some(ArrayElementType::Long) => b'J',
        Some(ArrayElementType::Float) => b'F',
        Some(ArrayElementType::Double) => b'D',
        Some(ArrayElementType::Reference) | None => {
            let nested = cm
                .class_store
                .get(shared.mem.heap.class_id_of(arr))
                // The header names the COMPONENT class (wave 25): an array
                // of arrays has an array class there. It checked `[[`.
                .is_some_and(|c| c.name.starts_with('['));
            if nested {
                b'['
            } else {
                b'L'
            }
        }
    }
}

/// Check `first` / `length` (JDWP ints) against an array of `len` elements,
/// as HotSpot's back end checks them (interpreter round i1 wave 43, lane L1;
/// `tools/probes/interp/L1/L1W43RawJdwpObjectErrorAnswers.java`, measured on
/// HotSpot 25.0.3): `first` must name an element (`INVALID_INDEX` otherwise,
/// even for an empty region: `(3, 0)` of a 3-element array, and any region
/// of an empty array), and the region must end inside the array
/// (`INVALID_LENGTH`). With `rest`, `GetValues`' `length` of -1 is every
/// element from `first` on. Wave 43: `(len, 0)` answered an empty region,
/// `(len, 1)` `INVALID_LENGTH` and `GetValues`' -1 `INVALID_LENGTH`.
fn region(first: u32, length: u32, len: usize, rest: bool) -> Result<(usize, usize), u16> {
    // Cast: JDWP sends these as signed ints.
    let (first, length) = (first as i32, length as i32);
    let first = usize::try_from(first).map_err(|_| ERR_INVALID_INDEX)?;
    if first >= len {
        return Err(ERR_INVALID_INDEX);
    }
    let length = if rest && length == -1 {
        len - first
    } else {
        usize::try_from(length).map_err(|_| ERR_INVALID_LENGTH)?
    };
    if length > len - first {
        return Err(ERR_INVALID_LENGTH);
    }
    Ok((first, length))
}

/// `ArrayReference.GetValues` (13/2): an `arrayregion` — the element tag,
/// the count, then the values, untagged for a primitive array and tagged for
/// a reference array.
fn ar_get_values(
    shared: &SharedVm,
    cm: &ClassManager,
    ds: &mut DebugState,
    r: &mut PayloadReader<'_>,
) -> Reply {
    let arr = array(shared, ds, wire(r.read_u64_be())?)?;
    let (first, length) = region(
        wire(r.read_u32_be())?,
        wire(r.read_u32_be())?,
        shared.mem.heap.array_length(arr),
        true,
    )?;
    let desc = element_desc(shared, cm, arr);
    let primitive = !matches!(desc, b'L' | b'[');
    let mut pw = PayloadWriter::new();
    pw.put_u8(desc);
    // Cast: bounded by the array length.
    pw.put_u32_be(length as u32);
    for i in first..first + length {
        let v = shared
            .mem
            .heap
            .get_array_element(arr, i)
            .map_err(|_| ERR_INVALID_INDEX)?;
        if primitive {
            put_untagged_primitive(&mut pw, desc, v);
        } else {
            put_tagged(shared, cm, ds, &mut pw, desc, v);
        }
    }
    Ok(pw.into_bytes())
}

/// `ArrayReference.SetValues` (13/3): untagged values of the component
/// type, all read and checked before any is stored.
fn ar_set_values(
    shared: &SharedVm,
    cm: &ClassManager,
    ds: &mut DebugState,
    r: &mut PayloadReader<'_>,
) -> Reply {
    let arr = array(shared, ds, wire(r.read_u64_be())?)?;
    let (first, length) = region(
        wire(r.read_u32_be())?,
        wire(r.read_u32_be())?,
        shared.mem.heap.array_length(arr),
        false,
    )?;
    let desc = element_desc(shared, cm, arr);
    let mut values = Vec::with_capacity(length);
    for _ in 0..length {
        values.push(read_untagged(shared, ds, r, desc)?);
    }
    // nio21 wave 5 L1: the overwritten element is published first on a
    // marking Generational heap, as for `write_field` above (the same gap: its
    // `set_array_element` card-marks only).
    let heap = &shared.mem.heap;
    let (arr, _) = heap.load_and_forward_checked(arr);
    for (i, value) in values.into_iter().enumerate() {
        crate::vm::vm_exec::publish_overwritten_array_element(
            heap,
            crate::runtime::env_cache::native_store_satb,
            arr,
            first + i,
        );
        heap.set_array_element(arr, first + i, value)
            .map_err(|_| ERR_INVALID_INDEX)?;
    }
    Ok(Vec::new())
}

/// `ClassObjectReference.ReflectedType` (17/1): the class a `Class` mirror
/// stands for. It used to echo the object id back as a class id.
fn cor_reflected_type(
    shared: &SharedVm,
    cm: &ClassManager,
    ds: &mut DebugState,
    r: &mut PayloadReader<'_>,
) -> Reply {
    let obj = object(shared, ds, wire(r.read_u64_be())?)?;
    let class_id = crate::vm::class_id_from_mirror(shared, obj).ok_or(ERR_INVALID_OBJECT)?;
    let mut pw = PayloadWriter::new();
    pw.put_u8(ref_type_tag(cm, class_id));
    pw.put_u64_be(super::ids::class_to_wire(u64::from(class_id.as_u32())));
    Ok(pw.into_bytes())
}

// ---------------------------------------------------------------------------
// Values crossing an invocation (`SharedVmBridge`)
// ---------------------------------------------------------------------------

/// A [`DebuggerValue`] argument as a VM value; an object id resolves through
/// the object table (`INVALID_OBJECT` for one it does not know or whose
/// object was collected).
pub(crate) fn debugger_value_to_vm(
    shared: &SharedVm,
    ds: &DebugState,
    v: &DebuggerValue,
) -> Result<Value, BridgeError> {
    Ok(match v {
        DebuggerValue::Void => Value::Uninitialized,
        DebuggerValue::Boolean(b) => Value::Int(i32::from(*b != 0)),
        DebuggerValue::Byte(b) => Value::Int(i32::from(*b)),
        DebuggerValue::Char(c) => Value::Int(i32::from(*c)),
        DebuggerValue::Short(s) => Value::Int(i32::from(*s)),
        DebuggerValue::Int(i) => Value::Int(*i),
        DebuggerValue::Long(l) => Value::Long(*l),
        DebuggerValue::Float(f) => Value::Float(f32::from_bits(*f)),
        DebuggerValue::Double(d) => Value::Double(f64::from_bits(*d)),
        DebuggerValue::Object(id)
        | DebuggerValue::Array(id)
        | DebuggerValue::String(id)
        | DebuggerValue::Thread(id)
        | DebuggerValue::ThreadGroup(id)
        | DebuggerValue::ClassLoader(id)
        | DebuggerValue::ClassObject(id) => {
            if *id == 0 {
                Value::Object(None)
            } else {
                Value::Object(Some(
                    super::object_for_id(shared, ds, *id).ok_or(BridgeError::InvalidObject)?,
                ))
            }
        }
    })
}

/// A returned VM value as a [`DebuggerValue`] of the method's return type
/// `return_sig`; an object is exported and tagged by its class
/// ([`object_tag`]: a returned `String` is `s`, so JDI shows its text).
pub(crate) fn vm_value_to_debugger(
    shared: &SharedVm,
    cm: &ClassManager,
    ds: &mut DebugState,
    v: Value,
    return_sig: &str,
) -> DebuggerValue {
    // Casts below: narrowing to the declared return type, as the JVM does.
    match return_sig.as_bytes().first().copied().unwrap_or(b'V') {
        b'V' => DebuggerValue::Void,
        b'Z' => DebuggerValue::Boolean(u8::from(as_int(v) != 0)),
        b'B' => DebuggerValue::Byte(as_int(v) as i8),
        b'C' => DebuggerValue::Char(as_int(v) as u16),
        b'S' => DebuggerValue::Short(as_int(v) as i16),
        b'I' => DebuggerValue::Int(as_int(v)),
        b'J' => DebuggerValue::Long(as_long(v)),
        b'F' => DebuggerValue::Float(as_float_bits(v)),
        b'D' => DebuggerValue::Double(as_double_bits(v)),
        // A null return is tagged `L` whatever the return type, as HotSpot's
        // back end writes it (wave 23: a null array return was `[`).
        _ => match as_ref(v) {
            None => DebuggerValue::Object(0),
            Some(o) => {
                let tag = object_tag(shared, cm, o);
                // A live thread is its thread id (wave 24).
                if tag == b't' {
                    if let Some((_, tid)) = thread_value(shared, ds, o) {
                        return DebuggerValue::Thread(tid);
                    }
                }
                let id = super::export_object(shared, ds, o, ObjectExport::Sent);
                match tag {
                    b'[' => DebuggerValue::Array(id),
                    b's' => DebuggerValue::String(id),
                    b'c' => DebuggerValue::ClassObject(id),
                    b'l' => DebuggerValue::ClassLoader(id),
                    b'g' => DebuggerValue::ThreadGroup(id),
                    _ => DebuggerValue::Object(id),
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(addr: usize) -> ObjectRef {
        // SAFETY: never dereferenced; the ordering only compares.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    /// Wave 25: JVMTI `GetOwnedMonitorInfo`'s order for the
    /// `L1W25JdiStopMonitors` owner — frame 2 (`LockC.enter`, a synchronized
    /// method on C with a block on B), frame 1 (`ownerRun`, a block on A and
    /// a re-entry of C), frame 0 holding nothing — given as
    /// `frame_locked_monitors` lists it (frames innermost first, a frame's
    /// newest block first, its method monitor last): C then B (the frame's
    /// entry order), then A; the re-entered C is not listed again; a frame
    /// entry the lock stack does not hold is dropped; an owned monitor no
    /// frame names follows at -1.
    #[test]
    fn owned_monitors_follow_jvmti_order_and_depths() {
        let (a, b, c, stale, jni) = (obj(0x10), obj(0x20), obj(0x30), obj(0x40), obj(0x50));
        let pairs = vec![(2, b), (2, c), (1, c), (1, a), (1, stale)];
        let owned = [a, b, c, jni];
        // Frame index (from the bottom) -> JDWP depth: two native / compiled
        // entries sit above frame 2.
        let depth_of = |index: usize| i32::try_from(4 - index).ok();
        let got = jvmti_owned_monitor_order(&owned, pairs, depth_of);
        assert_eq!(got, vec![(c, 2), (b, 2), (a, 3), (jni, -1)]);
        // No attribution: the lock stack's order, every one at -1.
        let got = jvmti_owned_monitor_order(&owned, Vec::new(), depth_of);
        assert_eq!(got, vec![(a, -1), (b, -1), (c, -1), (jni, -1)]);
    }

    /// Interpreter round i5 wave 102, lane L1: frames align to a listing
    /// with extra rows by method id from the bottom; the positional map
    /// needs equal counts.
    #[test]
    fn w102_l1_owned_monitor_frames_align_to_a_longer_listing() {
        // Thread frames bottom first: run(1), ownerRun(2), enter(3),
        // sleepQuietly(4). The listing, top first: Thread.sleep(9) served
        // (depth 1, the native sleep0 at depth 0 is not a row here),
        // sleepQuietly(4) depth 2, enter(3) 3, ownerRun(2) 4, run(1) 5.
        let rows = vec![(1, 9), (2, 4), (3, 3), (4, 2), (5, 1)];
        let got = align_frames_to_listing(&[1, 2, 3, 4], &rows);
        assert_eq!(got, vec![Some(5), Some(4), Some(3), Some(2)]);
        // A frame no row names stays unattributed and consumes nothing.
        let got = align_frames_to_listing(&[1, 7, 2], &rows);
        assert_eq!(got, vec![Some(5), None, Some(4)]);
        assert_eq!(position_depths(2, &[0, 3]), vec![Some(3), Some(0)]);
        assert!(position_depths(3, &[0, 3]).is_empty());
        // Wave 102 hand-back: a window listing names its frames. Top first:
        // the native head, a served level, then frames 2, 1, 0.
        let rows = [None, None, Some(2), Some(1), Some(0)];
        assert_eq!(window_depths(&rows), vec![Some(4), Some(3), Some(2)]);
        assert!(window_depths(&[None]).is_empty());
    }

    /// Wave 25: the monitor commands are heap commands, and the capabilities
    /// JDI checks before sending them are granted.
    #[test]
    fn monitor_commands_are_served_with_the_heap() {
        use crate::debug::commands::{
            CMD_TR_CURRENT_CONTENDED_MONITOR, CMD_TR_OWNED_MONITORS,
            CMD_TR_OWNED_MONITORS_STACK_DEPTH_INFO,
        };
        for cmd in [
            CMD_TR_OWNED_MONITORS,
            CMD_TR_CURRENT_CONTENDED_MONITOR,
            CMD_TR_OWNED_MONITORS_STACK_DEPTH_INFO,
        ] {
            assert!(is_monitor_command(CS_THREAD_REF, cmd));
            assert!(served_with_the_heap(CS_THREAD_REF, cmd));
        }
        assert!(!is_monitor_command(CS_THREAD_REF, CMD_TR_STOP));
        // The JDWP 9 module commands, too.
        use crate::debug::commands::{
            CMD_MR_CLASS_LOADER, CMD_MR_NAME, CMD_RT_MODULE, CMD_VM_ALL_MODULES, CS_MODULE_REF,
        };
        for (set, cmd) in [
            (CS_VM, CMD_VM_ALL_MODULES),
            (CS_REF_TYPE, CMD_RT_MODULE),
            (CS_MODULE_REF, CMD_MR_NAME),
            (CS_MODULE_REF, CMD_MR_CLASS_LOADER),
        ] {
            assert!(is_module_command(set, cmd));
            assert!(served_with_the_heap(set, cmd));
        }
    }

    /// Wave 8: the heap commands are recognised as such (and only they), the
    /// region check follows JDWP's `INVALID_INDEX` / `INVALID_LENGTH`, and a
    /// field id round-trips through its class and index.
    #[test]
    fn heap_commands_regions_and_field_ids() {
        assert!(is_heap_command(CS_OBJECT_REF, CMD_OR_GET_VALUES));
        assert!(is_heap_command(CS_VM, CMD_VM_CREATE_STRING));
        assert!(is_heap_command(CS_ARRAY_REF, CMD_AR_SET_VALUES));
        assert!(!is_heap_command(
            CS_OBJECT_REF,
            super::super::commands::CMD_OR_INVOKE_METHOD
        ));
        assert!(!is_heap_command(
            CS_VM,
            super::super::commands::CMD_VM_VERSION
        ));
        // HotSpot's checks (wave 43): the first index must name an element.
        for rest in [false, true] {
            assert_eq!(region(0, 4, 4, rest), Ok((0, 4)));
            assert_eq!(region(3, 0, 4, rest), Ok((3, 0)));
            assert_eq!(region(4, 0, 4, rest), Err(ERR_INVALID_INDEX));
            assert_eq!(region(0, 0, 0, rest), Err(ERR_INVALID_INDEX), "empty array");
            assert_eq!(region(5, 0, 4, rest), Err(ERR_INVALID_INDEX));
            assert_eq!(region(1, 4, 4, rest), Err(ERR_INVALID_LENGTH));
            assert_eq!(region(u32::MAX, 1, 4, rest), Err(ERR_INVALID_INDEX), "-1");
            assert_eq!(
                region(0, u32::MAX - 1, 4, rest),
                Err(ERR_INVALID_LENGTH),
                "-2"
            );
        }
        // -1 is the rest of the array for `GetValues` only.
        assert_eq!(region(1, u32::MAX, 4, true), Ok((1, 3)));
        assert_eq!(region(0, u32::MAX, 4, false), Err(ERR_INVALID_LENGTH), "-1");
        let id = super::super::jdwp_field_id(ClassId::new(0x0123_4567), 9);
        assert_eq!(
            super::super::field_of_id(id),
            (ClassId::new(0x0123_4567), 9)
        );
        assert_ne!(
            super::super::jdwp_field_id(ClassId::new(1), 0),
            super::super::jdwp_field_id(ClassId::new(2), 0),
            "field 0 of two classes"
        );
    }

    /// Wave 84 (lane L1): HotSpot's `is_assignable` for a local written by a
    /// debugger — a `String` fits a `String` or `Object` variable, not an
    /// `Integer` or an array one.
    #[test]
    fn a_debugger_local_write_must_fit_the_declared_class() {
        assert_eq!(reference_name_of_signature("Ljava/lang/String;"), "java/lang/String");
        assert_eq!(reference_name_of_signature("[Ljava/lang/String;"), "[Ljava/lang/String;");
        assert_eq!(reference_name_of_signature("[I"), "[I");
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let cm = shared.classes.class_manager.read();
        let Some(string) = cm.find_class_by_name_for_loader("java/lang/String", cratonvm_types::ClassLoaderId::Bootstrap) else {
            return; // a stripped test image
        };
        assert!(object_fits_local_signature(&cm, string, "Ljava/lang/String;"));
        assert!(object_fits_local_signature(&cm, string, "Ljava/lang/Object;"));
        assert!(!object_fits_local_signature(&cm, string, "[Ljava/lang/String;"));
        assert!(!object_fits_local_signature(&cm, string, "[I"));
        if cm.find_class_by_name_for_loader("java/lang/Integer", cratonvm_types::ClassLoaderId::Bootstrap).is_some() {
            assert!(!object_fits_local_signature(&cm, string, "Ljava/lang/Integer;"));
        }
    }

    /// Interpreter round i2 wave 61, lane L1: a C agent's `NotifyFramePop` of
    /// a blocked thread waits for the thread's wake, once per frame and only
    /// for the frame it was recorded against; a `ClearAllFramePops` drops the
    /// thread's requests there.
    #[test]
    fn a_blocked_threads_frame_pop_requests_wait_for_its_wake() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut thread = JvmThread::new(crate::threading::ThreadId(77), "blocked");
        let frame = |name: &str| {
            crate::runtime::frame::Frame::new(
                ClassId::new(0),
                "T".to_string(),
                name.to_string(),
                "()V".to_string(),
                None,
                vec![0xb1],
                vec![],
                1,
                1,
                &[],
            )
        };
        thread.frames.push(frame("a"));
        thread.frames.push(frame("b"));
        let method_b = thread
            .frames
            .get(1)
            .map(super::super::frame_method_id)
            .unwrap();
        assert!(defer_jvmti_frame_pop(&shared, 77, 1, 2, 0, method_b));
        assert!(
            !defer_jvmti_frame_pop(&shared, 77, 1, 2, 0, method_b),
            "a duplicate"
        );
        // Recorded against a method frame 0 does not run: dropped at the wake.
        assert!(defer_jvmti_frame_pop(&shared, 77, 0, 2, 0, method_b ^ 1));
        assert!(shared.debug.debugger_gates.frame_writes_pending());
        apply_deferred_frame_writes_if_any(&shared, &mut thread);
        assert_eq!(thread.frame_pop_requests, vec![1]);
        assert!(!shared.debug.debugger_gates.frame_writes_pending());
        defer_jvmti_clear_frame_pops(&shared, 77);
        apply_deferred_frame_writes_if_any(&shared, &mut thread);
        assert!(thread.frame_pop_requests.is_empty());
    }

    /// Wave 8: reading and writing an object's fields and an array's
    /// elements through the object table, on the quiesced server path (no
    /// thread is parked in a unit test).
    #[test]
    fn object_fields_and_array_elements_round_trip() {
        use std::sync::Arc;
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let Some(ints) =
            shared
                .mem
                .heap
                .try_alloc_array_full(ClassId::new(0), ArrayElementType::Int, 3)
        else {
            return; // no room in a stripped test heap
        };
        if shared
            .mem
            .heap
            .is_object_address(ints.as_ptr() as usize) // Cast: address probe
            .is_none()
        {
            return; // a backend that cannot answer the liveness probe
        }
        let id = {
            let mut ds = shared.debug.debug_state.lock();
            super::super::export_object(&shared, &mut ds, ints, ObjectExport::Sent)
        };
        // ArrayReference.SetValues(id, 1, [7, -2]) then GetValues(id, 0, 3).
        let mut set = PayloadWriter::new();
        set.put_u64_be(id);
        set.put_u32_be(1);
        set.put_u32_be(2);
        set.put_u32_be(7);
        set.put_u32_be((-2i32) as u32); // Cast: wire encoding
        let res = run_heap_command(&shared, CS_ARRAY_REF, CMD_AR_SET_VALUES, &set.into_bytes());
        assert_eq!(res.error_code, 0);
        let mut get = PayloadWriter::new();
        get.put_u64_be(id);
        get.put_u32_be(0);
        get.put_u32_be(3);
        let res = run_heap_command(&shared, CS_ARRAY_REF, CMD_AR_GET_VALUES, &get.into_bytes());
        assert_eq!(res.error_code, 0);
        let mut want = vec![b'I', 0, 0, 0, 3];
        for v in [0i32, 7, -2] {
            want.extend_from_slice(&v.to_be_bytes());
        }
        assert_eq!(res.data, want, "tag, count, three untagged ints");
        let res = run_heap_command(&shared, CS_ARRAY_REF, CMD_AR_LENGTH, &id.to_be_bytes());
        assert_eq!(res.data, 3u32.to_be_bytes().to_vec());
        let mut past = PayloadWriter::new();
        past.put_u64_be(id);
        past.put_u32_be(2);
        past.put_u32_be(2);
        let res = run_heap_command(&shared, CS_ARRAY_REF, CMD_AR_GET_VALUES, &past.into_bytes());
        assert_eq!(res.error_code, ERR_INVALID_LENGTH);
        // An id the table never handed out.
        let res = run_heap_command(&shared, CS_ARRAY_REF, CMD_AR_LENGTH, &77u64.to_be_bytes());
        assert_eq!(res.error_code, ERR_INVALID_OBJECT);
        // Wave 43, as HotSpot answers: such an id is collected, enabling its
        // collection succeeds, disabling it is `INVALID_OBJECT`.
        let res = run_heap_command(
            &shared,
            CS_OBJECT_REF,
            CMD_OR_IS_COLLECTED,
            &77u64.to_be_bytes(),
        );
        assert_eq!((res.error_code, res.data), (0, vec![1]));
        let res = run_heap_command(
            &shared,
            CS_OBJECT_REF,
            CMD_OR_ENABLE_COLLECTION,
            &77u64.to_be_bytes(),
        );
        assert_eq!(res.error_code, 0);
        let res = run_heap_command(
            &shared,
            CS_OBJECT_REF,
            CMD_OR_DISABLE_COLLECTION,
            &77u64.to_be_bytes(),
        );
        assert_eq!(res.error_code, ERR_INVALID_OBJECT);
        // IsCollected on a live id; DisableCollection makes its handle strong.
        let res = run_heap_command(
            &shared,
            CS_OBJECT_REF,
            CMD_OR_IS_COLLECTED,
            &id.to_be_bytes(),
        );
        assert_eq!(res.data, vec![0]);
        let res = run_heap_command(
            &shared,
            CS_OBJECT_REF,
            CMD_OR_DISABLE_COLLECTION,
            &id.to_be_bytes(),
        );
        assert_eq!(res.error_code, 0);
        let handle = shared.debug.debug_state.lock().objects.handle_of(id);
        assert_eq!(
            handle.and_then(|h| shared.natives.jni_global_refs.lock().kind(h)),
            Some(crate::native::jni::JniGlobalKind::Strong)
        );
        let res = run_heap_command(
            &shared,
            CS_OBJECT_REF,
            CMD_OR_ENABLE_COLLECTION,
            &id.to_be_bytes(),
        );
        assert_eq!(res.error_code, 0);
        let mut ds = shared.debug.debug_state.lock();
        let handle = ds.objects.handle_of(id);
        assert_eq!(
            handle.and_then(|h| shared.natives.jni_global_refs.lock().kind(h)),
            Some(crate::native::jni::JniGlobalKind::Weak)
        );
        ds.objects.release_all();
        super::super::release_disposed_objects(&shared, &mut ds);
    }
}

/// Interpreter round i6 wave 123, lane L2 (stage 3b of i43-L1): which frame a
/// pending `SetValues` to a compiled activation is applied to.
#[cfg(test)]
mod w123_l2_compiled_home_write_tests {
    use super::*;
    use crate::threading::jvm_thread::CompiledHomeBody;

    fn write() -> super::super::CompiledHomeWrite {
        super::super::CompiledHomeWrite {
            body: CompiledHomeBody {
                cm_ptr: 0x5000,
                frame_rbp: 0x7ff0,
            },
            exit_bci: 12,
            class_id: 7,
            method_id: 9,
            slot: 1,
            value: super::super::DeferredLocal::Plain(Value::Int(5)),
        }
    }

    #[test]
    fn a_write_matches_its_own_activation_only() {
        let w = write();
        // The body that left, at the exit bci, on the frame base.
        assert!(compiled_home_write_matches(&w, 0x7ff0, Some(0x5000), 12, 7, 9));
        // Wave 124 (L2, i124-L9): the frame being built is the authority, so
        // another method's frame never takes the write, body named or not.
        assert!(!compiled_home_write_matches(&w, 0x7ff0, Some(0x5000), 12, 0, 0));
        assert!(!compiled_home_write_matches(&w, 0x7ff0, Some(0x5000), 12, 7, 8));
        // Another body, base or bci: not this activation.
        assert!(!compiled_home_write_matches(&w, 0x7ff0, Some(0x5008), 12, 7, 9));
        assert!(!compiled_home_write_matches(&w, 0x7fe0, Some(0x5000), 12, 7, 9));
        assert!(!compiled_home_write_matches(&w, 0x7ff0, Some(0x5000), 13, 7, 9));
        // An unnamed body falls back to the frame's class and method.
        assert!(compiled_home_write_matches(&w, 0x7ff0, None, 12, 7, 9));
        assert!(!compiled_home_write_matches(&w, 0x7ff0, None, 12, 7, 8));
        // No base: never.
        let mut zero = w;
        zero.body.frame_rbp = 0;
        assert!(!compiled_home_write_matches(&zero, 0, Some(0x5000), 12, 7, 9));
    }
}
