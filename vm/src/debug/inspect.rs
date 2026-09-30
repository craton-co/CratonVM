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
//! not type-checked against the declared class (JDI checks before sending);
//! JDWP's `TYPE_MISMATCH` is answered only for a non-array stored where the
//! descriptor requires an array.

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
use cratonvm_native_api::{NativeHeapAccess as _, NativeInvokeAccess as _, NativeThreadAccess as _};

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
    ) || is_monitor_command(command_set, command)
        || is_module_command(command_set, command)
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
// docs/internal/fixed-bugs/interpreter-L1-jdwp-suspension-does-not-reach-compiled-or-native-code-FIXED-20261005.md).
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
    let Some(writes) = ds.deferred_frame_writes.remove(&tid) else {
        return;
    };
    shared
        .debug
        .debugger_gates
        .set_frame_writes_pending(!ds.deferred_frame_writes.is_empty());
    let depth = thread.frames.len();
    let (mut applied, mut dropped) = (0usize, 0usize);
    for w in &writes {
        let value = match w.value {
            super::DeferredLocal::Plain(v) => Some(v),
            super::DeferredLocal::Object(id) => {
                super::object_for_id(shared, &ds, id).map(|o| Value::Object(Some(o)))
            }
        };
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
        if let super::DeferredLocal::Object(id) = w.value {
            release_deferred_object(shared, &mut ds, id);
        }
    }
    super::release_disposed_objects(shared, &mut ds);
    drop(ds);
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[DEFERRED_SETVALUES] applied tid={tid} writes={applied} dropped={dropped}");
    }
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
        let value = match w.value {
            super::DeferredLocal::Plain(v) => Some(v),
            super::DeferredLocal::Object(id) => {
                super::object_for_id(shared, ds, id).map(|o| Value::Object(Some(o)))
            }
        };
        if let Some(v) = value {
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
}

/// [`discard_deferred_frame_writes_if_any`] with a write waiting somewhere.
#[cold]
#[inline(never)]
fn discard_deferred_frame_writes(shared: &SharedVm, tid: u64) {
    let mut ds = shared.debug.debug_state.lock();
    let Some(writes) = ds.deferred_frame_writes.remove(&tid) else {
        return;
    };
    shared
        .debug
        .debugger_gates
        .set_frame_writes_pending(!ds.deferred_frame_writes.is_empty());
    for w in writes {
        if let super::DeferredLocal::Object(id) = w.value {
            release_deferred_object(shared, &mut ds, id);
        }
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
    // A native method's frame (wave 15) has no locals to write; nor has a
    // compiled activation, or a frame whose body runs compiled (wave 21).
    if entry.offset == super::NATIVE_FRAME_LOCATION
        || ds.opaque_frames.contains_key(&(tid, frame_id))
    {
        return Err(ERR_OPAQUE_FRAME);
    }
    let depth = thread.frames.len();
    // The frame's place among the interpreter frames, top first. The listing
    // may interleave frames that have no `Frame`: a native method's, and
    // (wave 21) the compiled activations between interpreted callers, so the
    // frame id is not the depth from the top.
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
fn class_loader_of(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
) -> Option<ObjectRef> {
    use cratonvm_native_api::NativeContext as _;
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
fn class_loader_command(shared: &SharedVm, thread: &mut JvmThread, data: &[u8]) -> CommandResult {
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
        .is_some();
    if !known {
        return CommandResult::error(ERR_INVALID_CLASS);
    }
    let loader = class_loader_of(shared, thread, class_id);
    // No safepoint between the answer and the export.
    let mut ds = shared.debug.debug_state.lock();
    let id = loader.map_or(0, |l| super::export_object(shared, &mut ds, l, ObjectExport::Sent));
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
    ctx.thread_interrupt(target);
    ctx.set_field_by_name(target, "interrupted", Value::Int(1));
    CommandResult::ok(Vec::new())
}

/// The `java.lang.Thread` a `ThreadReference.Interrupt` names, or its
/// refusal (`INVALID_THREAD`).
fn interrupt_target(shared: &SharedVm, ds: &DebugState, tid: u64) -> Result<ObjectRef, u16> {
    if let Some(code) = super::commands::live_thread_refusal(ds, tid) {
        return Err(code);
    }
    // `object_for_id` takes a wire id; `tid` is the VM's (wave 25).
    super::object_for_id(shared, ds, super::ids::thread_to_wire(tid)).ok_or(ERR_INVALID_THREAD)
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
fn wake_quiesced(shared: &SharedVm, tid: u64, target: ObjectRef) {
    let registry = &shared.threads.thread_registry;
    let vm_tid = crate::threading::ThreadId(tid);
    registry.set_interrupted(vm_tid, true);
    if let Some(monitor) = registry.peek_jmx_waiting_monitor(vm_tid) {
        let _ = shared.threads.monitors.wake_waiter_for_interrupt(monitor, vm_tid);
    }
    if shared.threads.virtual_thread_manager.is_virtual(tid) {
        shared.threads.virtual_thread_manager.interrupt_virtual(tid);
    }
    if let Some(park) = shared.find_park_state_for_thread_obj(target) {
        park.unpark();
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
fn array_descriptor(
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
fn known_array_class(
    shared: &SharedVm,
    desc: &str,
    component: Option<ClassId>,
) -> Option<ClassId> {
    let found = {
        let cm = shared.classes.class_manager.read();
        component
            .and_then(|c| cm.find_class_by_name_for_class(desc, c))
            .or_else(|| {
                cm.find_class_by_name_for_loader(desc, crate::classloading::ClassLoaderId::Bootstrap)
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
            pw.put_u64_be(super::export_object(shared, &mut ds, module, ObjectExport::Sent));
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
    let on_target = super::run_on_parked_thread(shared, Some(tid), move |shared, thread| {
        let frames: &[crate::runtime::frame::Frame] = &thread.frames;
        let pairs = crate::runtime::interpreter::held_monitors::frame_locked_monitors(frames);
        let own = (frames.len(), pairs);
        let cm = shared.classes.class_manager.read();
        let mut ds = shared.debug.debug_state.lock();
        match monitor_reply(shared, &cm, &mut ds, command, tid, Some(own)) {
            Ok(bytes) => CommandResult::ok(bytes),
            Err(code) => CommandResult::error(code),
        }
    });
    match on_target {
        Ok(answer) => Some(answer),
        Err(super::ParkedError::NotParked | super::ParkedError::Busy) => None,
        Err(super::ParkedError::Lost) => Some(CommandResult::error(ERR_INTERNAL)),
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
    own: Option<(usize, Vec<(usize, ObjectRef)>)>,
) -> Reply {
    use super::commands::{CMD_TR_CURRENT_CONTENDED_MONITOR, CMD_TR_OWNED_MONITORS};
    if let Some(code) = super::commands::frames_refusal(ds, tid) {
        return Err(code);
    }
    let registry = &shared.threads.thread_registry;
    let vm_tid = crate::threading::ThreadId(tid);
    let (contended, waiting, locked, _, _, _, _, _) =
        registry.jmx_lock_snapshot(vm_tid).ok_or(ERR_INVALID_THREAD)?;
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
    // The listing's interpreter frames, top first: their JDWP depths.
    let interpreted: Vec<usize> = ds.thread_frames.get(&tid).map_or_else(Vec::new, |frames| {
        frames
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                e.offset != super::NATIVE_FRAME_LOCATION
                    && ds.opaque_frames.get(&(tid, e.frame_id)) != Some(&true)
            })
            .map(|(depth, _)| depth)
            .collect()
    });
    let (frame_count, pairs) = match own {
        Some((count, pairs)) => (count, Some(pairs)),
        None => {
            let count = interpreted.len();
            (count, registry.jmx_frame_monitors(vm_tid, count))
        }
    };
    let pairs = pairs.filter(|_| frame_count == interpreted.len());
    let monitors = jvmti_owned_monitor_order(&owned, pairs.unwrap_or_default(), |index| {
        // `index` counts from the bottom; `interpreted` from the top.
        frame_count
            .checked_sub(index + 1)
            .and_then(|from_top| interpreted.get(from_top))
            .and_then(|&depth| i32::try_from(depth).ok())
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
            let wire = super::ids::thread_to_wire(tid);
            Some((tid, super::object_for_id(shared, &ds, wire)?))
        })
        .collect()
}

/// A thread's group: `Thread.holder.group` (JDK 19+), else a `group` field
/// on the thread itself (older layouts); `None` for a `Thread` with neither.
fn group_of_thread(ctx: &crate::vm::NativeContextImpl<'_>, thread_obj: ObjectRef) -> Option<ObjectRef> {
    use cratonvm_native_api::NativeContext as _;
    if let Value::Object(Some(holder)) = ctx.get_field_by_name(thread_obj, "holder") {
        return match ctx.get_field_by_name(holder, "group") {
            Value::Object(group) => group,
            _ => None,
        };
    }
    match ctx.get_field_by_name(thread_obj, "group") {
        Value::Object(group) => group,
        _ => None,
    }
}

/// A group's parent (`ThreadGroup.parent`); `None` for the root.
fn group_parent(ctx: &crate::vm::NativeContextImpl<'_>, group: ObjectRef) -> Option<ObjectRef> {
    use cratonvm_native_api::NativeContext as _;
    match ctx.get_field_by_name(group, "parent") {
        Value::Object(parent) => parent,
        _ => None,
    }
}

/// A group's subgroups, as `ThreadGroup.subgroups()` lists them: the
/// strongly held `groups[0..ngroups]`, then the live referents of
/// `weaks[0..nweaks]`.
fn group_subgroups(
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
        let Some(mut group) = group_of_thread(ctx, thread_obj) else {
            continue;
        };
        for _ in 0..MAX_GROUP_CHAIN {
            match group_parent(ctx, group) {
                Some(parent) => group = parent,
                None => break,
            }
        }
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
        pw.put_u64_be(super::export_object(shared, &mut ds, root, ObjectExport::Sent));
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
        super::object_for_id(shared, &ds, super::ids::thread_to_wire(tid))?
    };
    let group = group_of_thread(ctx, thread_obj)?;
    let mut ds = shared.debug.debug_state.lock();
    let mut pw = PayloadWriter::new();
    pw.put_u64_be(super::export_object(shared, &mut ds, group, ObjectExport::Sent));
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
            let threads: Vec<u64> = listed_thread_objects(shared)
                .into_iter()
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
                pw.put_u64_be(super::ids::thread_to_wire(tid));
            }
            pw.put_u32_be(subgroups.len() as u32);
            for sub in subgroups {
                pw.put_u64_be(super::export_object(shared, &mut ds, sub, ObjectExport::Sent));
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
        .is_some();
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
    if cm.class_store.get(class_id).is_none() {
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
        _ => Err(ERR_INTERNAL),
    };
    match result {
        Ok(bytes) => CommandResult::ok(bytes),
        Err(code) => CommandResult::error(code),
    }
}

/// A command's reply payload, or its JDWP error code.
type Reply = Result<Vec<u8>, u16>;

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
pub(crate) fn thread_value(shared: &SharedVm, ds: &DebugState, obj: ObjectRef) -> Option<(u8, u64)> {
    let tid = shared
        .threads
        .thread_registry
        .find_thread_id_by_thread_obj(obj)?
        .0;
    // Every registered thread but the back end's own heap service, whether
    // or not the server's thread table has caught up with it (a thread id
    // names its `Thread` object from the registry, `debug::object_for_id`).
    // Its wire id (wave 25: the main thread's is not 0, JDWP's null).
    (ds.heap_service_thread() != Some(tid)).then_some((b't', super::ids::thread_to_wire(tid)))
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
    let class = cm.class_store.get(class_id).ok_or(ERR_INVALID_FIELDID)?;
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

fn read_field(shared: &SharedVm, obj: Option<ObjectRef>, field: FieldSlot) -> Value {
    match obj {
        Some(o) if !field.is_static => shared.mem.heap.get_field_as(o, field.slot, field.desc),
        _ => crate::vm::get_static_shared(shared, field.class_id, field.slot),
    }
}

/// Store through the VM's own barriered accessors (the SATB pre-barrier and
/// the card mark run inside them, as for `putfield` / `putstatic`).
fn write_field(shared: &SharedVm, obj: Option<ObjectRef>, field: FieldSlot, value: Value) {
    match obj {
        Some(o) if !field.is_static => shared
            .mem
            .heap
            .set_field_as(o, field.slot, value, field.desc),
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
    if cm.class_store.get(class_id).is_none() {
        return Err(ERR_INVALID_CLASS);
    }
    let loader =
        cratonvm_native_builtins::classloader::defining_loader_for(shared.vm_identity, raw);
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
    for (i, value) in values.into_iter().enumerate() {
        shared
            .mem
            .heap
            .set_array_element(arr, first + i, value)
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
            assert_eq!(region(0, u32::MAX - 1, 4, rest), Err(ERR_INVALID_LENGTH), "-2");
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
        let res = run_heap_command(&shared, CS_OBJECT_REF, CMD_OR_IS_COLLECTED, &77u64.to_be_bytes());
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
